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
//! RESUME answers, heartbeats, protocol errors), so priority draining
//! cannot starve data traffic from our own side.

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::sync::{Mutex, mpsc, oneshot, watch};
use tokio::task::JoinHandle;

use crate::error::XacppError;
use crate::message::XacppResponse;

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
        loop {
            tokio::select! {
                biased;
                control = control_rx.recv(), if control_open => match control {
                    Some(EgressItem::Frame { frame, write_ack: _ }) => {
                        if write_frame(&mut writer, &frame).await.is_err() {
                            // Connection dead: fail fast everything, now and later.
                            task_connected.store(false, Ordering::Release);
                            task_closed.send_replace(true);
                            pending.lock().await.clear();
                            break;
                        }
                    }
                    // Never enqueued (teardown rides `tx`); defensive.
                    Some(EgressItem::Close) => {}
                    None => control_open = false,
                },
                item = rx.recv() => {
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
