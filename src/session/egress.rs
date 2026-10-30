//! Ordered egress: single FIFO exit for all outbound frames.
//!
//! One drain task per connection owns the write half; every outbound frame
//! (request / event / response) is queued and written in submission order.
//! Two send paths differ only at enqueue time:
//!
//! - **acked** (`send_acked`): waits for the write result — fail fast on
//!   write error (command path, preserves historical `send` semantics);
//! - **fire-and-forget** (`send_faf`): returns as soon as the frame is
//!   queued — the caller never blocks on the wire (event path).
//!
//! Control frames (`send_control`) ride a second channel drained with
//! priority (SPEC §5 performance invariant 4: control frames take
//! scheduling priority and may interleave between data frames). Only the
//! relative order of data frames is a FIFO guarantee; control frames may
//! jump ahead of queued data frames. Outbound control frames originate
//! exclusively from local low-frequency paths (ACK batches, NACK feedback,
//! RESUME declarations/verdicts, heartbeats, protocol errors), so priority
//! draining cannot starve data traffic from our own side.

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::sync::{Mutex, mpsc, oneshot, watch};
use tokio::task::JoinHandle;

use crate::wire::error::XacppError;
use crate::wire::message::XacppResponse;

/// Pending slot for request/response correlation.
pub(crate) enum PendingSlot {
    /// Awaiting the response payload (caller of `send`).
    Respond(oneshot::Sender<XacppResponse>),
    /// Fire-and-forget: the peer's ack is silently consumed on arrival.
    Drop,
}

/// Shared pending table type (one per connection).
pub(crate) type PendingMap = Arc<Mutex<HashMap<String, PendingSlot>>>;

/// A queued outbound item.
enum EgressItem {
    /// Pre-encoded wire frame bytes (a complete binary frame, SPEC §3).
    Frame {
        frame: Vec<u8>,
        /// Write-result receipt: `Some` on the acked path, `None` for
        /// fire-and-forget.
        write_ack: Option<oneshot::Sender<Result<(), ()>>>,
    },
    /// Graceful teardown: flush queued frames, close the writer, exit.
    Close,
}

/// Ordered egress handle: cloneable entry for enqueueing outbound frames.
///
/// `Err` from either send method = connection already known dead.
#[derive(Clone)]
pub(crate) struct Egress {
    tx: mpsc::UnboundedSender<EgressItem>,
    /// Control-frame channel, drained with priority over `tx`. Never
    /// receives `Close` — teardown is driven solely through `tx`, so the
    /// control channel just dies with the drain task.
    control_tx: mpsc::UnboundedSender<EgressItem>,
    connected: Arc<AtomicBool>,
    closed_tx: watch::Sender<bool>,
}

impl Egress {
    /// Enqueue a frame and wait for the write result.
    pub(crate) async fn send_acked(&self, frame: Vec<u8>) -> Result<(), XacppError> {
        if !self.connected.load(Ordering::Acquire) {
            return Err(XacppError::NotConnected);
        }
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(EgressItem::Frame {
                frame,
                write_ack: Some(tx),
            })
            .map_err(|_| XacppError::Closed)?;
        match rx.await {
            Ok(Ok(())) => Ok(()),
            // Drain gone or write failed: connection dead.
            _ => Err(XacppError::Closed),
        }
    }

    /// Enqueue a frame without waiting for the write result (fire-and-forget).
    pub(crate) fn send_faf(&self, frame: Vec<u8>) -> Result<(), XacppError> {
        if !self.connected.load(Ordering::Acquire) {
            return Err(XacppError::NotConnected);
        }
        self.tx
            .send(EgressItem::Frame {
                frame,
                write_ack: None,
            })
            .map_err(|_| XacppError::Closed)
    }

    /// Enqueue a control frame with scheduling priority over queued data
    /// frames (fire-and-forget; SPEC §5 invariant 4). Same error semantics
    /// as [`Self::send_faf`].
    pub(crate) fn send_control(&self, frame: Vec<u8>) -> Result<(), XacppError> {
        if !self.connected.load(Ordering::Acquire) {
            return Err(XacppError::NotConnected);
        }
        self.control_tx
            .send(EgressItem::Frame {
                frame,
                write_ack: None,
            })
            .map_err(|_| XacppError::Closed)
    }

    /// Shut the egress down: queued frames flush FIFO, then the writer is
    /// closed and the drain task exits. Subsequent sends fail. Marks the
    /// connection closed for `closed()` watchers.
    pub(crate) fn close(&self) {
        self.connected.store(false, Ordering::Release);
        let _ = self.tx.send(EgressItem::Close);
        self.mark_closed();
    }

    /// Mark the connection closed for `closed()` watchers (idempotent).
    pub(crate) fn mark_closed(&self) {
        self.closed_tx.send_replace(true);
    }
}

/// Spawn the egress drain task: takes ownership of the write half and writes
/// queued frames FIFO, with the control channel drained first (priority).
///
/// Teardown semantics:
/// - write failure (either channel) → mark disconnected + mark closed for
///   watchers, clear pending (all waiters fail), fail remaining queued acks
///   on the data channel, exit (subsequent enqueues fail);
/// - `Close` item or channel close (all handles dropped) → shutdown writer,
///   exit.
///
/// The returned `JoinHandle` is informational; teardown is driven through
/// [`Egress::close`], so the handle may be dropped (detached).
pub(crate) fn spawn_egress(
    mut writer: Pin<Box<dyn AsyncWrite + Send>>,
    connected: Arc<AtomicBool>,
    pending: PendingMap,
    closed_tx: watch::Sender<bool>,
) -> (Egress, JoinHandle<()>) {
    let (tx, mut rx) = mpsc::unbounded_channel::<EgressItem>();
    let (control_tx, mut control_rx) = mpsc::unbounded_channel::<EgressItem>();
    let task_connected = Arc::clone(&connected);
    let task_closed = closed_tx.clone();
    let handle = tokio::spawn(async move {
        let mut control_open = true;

        enum DrainEvent {
            Control(Option<EgressItem>),
            Item(Option<EgressItem>),
        }

        loop {
            let event = tokio::select! {
                biased;
                control = control_rx.recv(), if control_open => DrainEvent::Control(control),
                item = rx.recv() => DrainEvent::Item(item),
            };
            match event {
                DrainEvent::Control(Some(EgressItem::Frame { frame, write_ack: _ })) => {
                    if write_frame(&mut writer, &frame).await.is_err() {
                        // Connection dead: fail fast everything, now and later.
                        task_connected.store(false, Ordering::Release);
                        task_closed.send_replace(true);
                        pending.lock().await.clear();
                        break;
                    }
                }
                // Never enqueued (teardown rides `tx`); defensive.
                DrainEvent::Control(Some(EgressItem::Close)) => {}
                DrainEvent::Control(None) => control_open = false,
                DrainEvent::Item(item) => {
                    let (frame, write_ack) = match item {
                        None | Some(EgressItem::Close) => break,
                        Some(EgressItem::Frame { frame, write_ack }) => (frame, write_ack),
                    };
                    let result = write_frame(&mut writer, &frame).await;
                    if let Some(ack) = write_ack {
                        let _ = ack.send(result);
                    }
                    if result.is_err() {
                        // Connection dead: fail fast everything, now and later.
                        task_connected.store(false, Ordering::Release);
                        task_closed.send_replace(true);
                        pending.lock().await.clear();
                        while let Ok(rest) = rx.try_recv() {
                            if let EgressItem::Frame {
                                write_ack: Some(ack),
                                ..
                            } = rest
                            {
                                let _ = ack.send(Err(()));
                            }
                        }
                        break;
                    }
                }
            }
        }
        let _ = writer.shutdown().await;
    });
    (
        Egress {
            tx,
            control_tx,
            connected,
            closed_tx,
        },
        handle,
    )
}

/// Write one binary frame (bytes + flush). Frame boundaries are self-contained
/// in the frame format (magic + header + CRC trailer, SPEC §3); no delimiters
/// are written on the wire.
async fn write_frame(writer: &mut Pin<Box<dyn AsyncWrite + Send>>, frame: &[u8]) -> Result<(), ()> {
    writer.write_all(frame).await.map_err(|_| ())?;
    writer.flush().await.map_err(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::pin::Pin;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWrite, duplex};

    /// Park the writer with a frame larger than the pipe buffer, queue data and
    /// control frames behind it, then assert the control frame is drained first
    /// — SPEC §5 performance invariant 4 (connection-level control frames take
    /// scheduling priority over queued data frames).
    #[tokio::test]
    async fn control_frames_jump_queued_data_frames() {
        const BLOCK: usize = 8192;
        let (writer, mut reader) = duplex(1024);
        let boxed: Pin<Box<dyn AsyncWrite + Send>> = Box::pin(writer);
        let connected = Arc::new(AtomicBool::new(true));
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let (closed_tx, _closed_rx) = watch::channel(false);
        let (egress, _handle) =
            spawn_egress(boxed, Arc::clone(&connected), Arc::clone(&pending), closed_tx);

        // Larger than the pipe buffer: the drain task parks inside write_frame.
        egress.send_faf(vec![0xAA; BLOCK]).unwrap();
        tokio::task::yield_now().await;

        // While it is parked, queue data frames and one control frame.
        for i in 1..=5u8 {
            egress.send_faf(vec![i; 100]).unwrap();
        }
        egress.send_control(vec![0xFF; 10]).unwrap();

        let mut block = vec![0u8; BLOCK];
        reader.read_exact(&mut block).await.unwrap();
        assert!(block.iter().all(|&b| b == 0xAA), "blocking frame comes first");

        let mut ctrl = vec![0u8; 10];
        reader.read_exact(&mut ctrl).await.unwrap();
        assert!(
            ctrl.iter().all(|&b| b == 0xFF),
            "control frame must jump the queued data frames"
        );

        for i in 1..=5u8 {
            let mut data = vec![0u8; 100];
            reader.read_exact(&mut data).await.unwrap();
            assert!(data.iter().all(|&b| b == i), "data frame {i} out of order");
        }
    }

    /// A dead reader makes the next write fail: the drain task must mark the
    /// connection dead, terminate, and reject later sends fast instead of
    /// queueing them into a drain that no longer exists.
    #[tokio::test]
    async fn write_failure_marks_dead_and_rejects_later_sends() {
        let (writer, reader) = duplex(64);
        let boxed: Pin<Box<dyn AsyncWrite + Send>> = Box::pin(writer);
        let connected = Arc::new(AtomicBool::new(true));
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let (closed_tx, _closed_rx) = watch::channel(false);
        let (egress, handle) =
            spawn_egress(boxed, Arc::clone(&connected), Arc::clone(&pending), closed_tx);

        drop(reader);
        let _ = egress.send_faf(vec![1u8; 1024]);

        let finished = tokio::time::timeout(Duration::from_secs(5), handle).await;
        assert!(
            finished.is_ok(),
            "drain task must terminate after a write failure"
        );
        assert!(
            !connected.load(Ordering::Acquire),
            "connection must be marked dead"
        );
        assert!(
            egress.send_faf(vec![2u8; 16]).is_err(),
            "later sends must fail fast"
        );
    }
}
