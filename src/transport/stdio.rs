//! Stdio Transport Implementation.
//!
//! Communicates via stdin/stdout pipe handles using the binary frame wire
//! protocol (SPEC §3): envelopes are carried as `DATA_JSON` frames.
//!
//! All outbound frames (requests, events, responses) go through the single
//! ordered egress queue (see `super::egress`): one drain task owns the write
//! half and writes frames FIFO in submission order.
//!
//! Log-bypass note: the frame decoder's magic scan silently tolerates
//! non-frame garbage bytes on this pipe; child process logs are expected
//! on stderr, not on stdout.

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use async_trait::async_trait;
use tokio::io::{AsyncBufRead, AsyncReadExt, AsyncWrite};
use tokio::sync::{Mutex, RwLock, oneshot, watch};
use tokio::task::JoinHandle;

use super::egress::{self, Egress, PendingSlot};
use super::transfer::TransferActivityHandlers;
use super::{RequestHandler, TransferWiring, XacppTransport};
use crate::error::XacppError;
use crate::frame::{Frame, FrameDecoder, FrameType};
use crate::message::{XacppEnvelope, XacppRequest, XacppResponse};

/// Shared state: accessed via Arc between accept loop and main task.
struct SharedState {
    request_handler: RwLock<Option<RequestHandler>>,
    pending: egress::PendingMap,
    connected: Arc<AtomicBool>,
    /// reader task handle, aborted on disconnect.
    reader_handle: Mutex<Option<JoinHandle<()>>>,
    /// accept_loop task handle, aborted on disconnect.
    accept_handle: Mutex<Option<JoinHandle<()>>>,
}

/// Stdio Transport Implementation.
pub struct StdioTransport {
    /// Write half parked before connect.
    parked_writer: Mutex<Option<Pin<Box<dyn AsyncWrite + Send>>>>,
    reader: Mutex<Option<Pin<Box<dyn AsyncBufRead + Send>>>>,
    /// Ordered egress entry (present after connect). Plain std lock: the
    /// narrow wiring methods (`write_control_frame` / `write_data_frame`)
    /// are sync signatures, and the critical section is a clone.
    egress: std::sync::Mutex<Option<Egress>>,
    /// Connection-close broadcast (see `XacppTransport::closed`).
    closed_tx: watch::Sender<bool>,
    shared: Arc<SharedState>,
    next_id: Arc<AtomicU64>,
    /// Inbound transfer hooks (registered by the wired transfer session
    /// through `TransferWiring::on_transfer_activity`; `None` = unwired —
    /// non-envelope frames are debug-ignored). Shared with the reader task
    /// so a late registration routes inbound frames immediately.
    transfer_hooks: Arc<std::sync::RwLock<Option<TransferActivityHandlers>>>,
    /// Connection operation mutex, ensures connect / disconnect do not execute concurrently.
    connect_lock: Mutex<()>,
}

impl StdioTransport {
    pub fn new(
        writer: Pin<Box<dyn AsyncWrite + Send>>,
        reader: Pin<Box<dyn AsyncBufRead + Send>>,
    ) -> Self {
        let (closed_tx, _) = watch::channel(false);
        Self {
            parked_writer: Mutex::new(Some(writer)),
            reader: Mutex::new(Some(reader)),
            egress: std::sync::Mutex::new(None),
            closed_tx,
            shared: Arc::new(SharedState {
                request_handler: RwLock::new(None),
                pending: Arc::new(Mutex::new(HashMap::new())),
                connected: Arc::new(AtomicBool::new(false)),
                reader_handle: Mutex::new(None),
                accept_handle: Mutex::new(None),
            }),
            next_id: Arc::new(AtomicU64::new(1)),
            transfer_hooks: Arc::new(std::sync::RwLock::new(None)),
            connect_lock: Mutex::new(()),
        }
    }

    fn next_id(&self) -> String {
        format!("r{}", self.next_id.fetch_add(1, Ordering::Relaxed))
    }

    /// Serialize an envelope into a complete binary wire frame: envelope JSON
    /// wrapped as a single-frame `DATA_JSON` transaction
    /// ([`super::encode_data_json_frame`]).
    fn serialize_envelope(msg: &XacppEnvelope) -> Result<Vec<u8>, XacppError> {
        let json = serde_json::to_vec(msg).map_err(|e| XacppError::Internal(e.to_string()))?;
        super::encode_data_json_frame(&json)
    }

    /// Clone the ordered egress entry (`Err(NotConnected)` before connect).
    fn egress(&self) -> Result<Egress, XacppError> {
        self.egress
            .lock()
            .expect("poisoned egress lock")
            .clone()
            .ok_or(XacppError::NotConnected)
    }

    /// Accept loop: read from frame channel, parse, dispatch.
    ///
    /// On exit: mark disconnected (subsequent sends fail fast), clear pending,
    /// shut the egress down.
    async fn accept_loop(
        mut frame_rx: tokio::sync::mpsc::Receiver<std::io::Result<Vec<u8>>>,
        egress: Egress,
        shared: Arc<SharedState>,
    ) {
        while let Some(result) = frame_rx.recv().await {
            let data = match result {
                Ok(d) => d,
                Err(_) => break,
            };

            let envelope = match serde_json::from_slice::<XacppEnvelope>(&data) {
                Ok(msg) => msg,
                // Only reachable for a `DATA_JSON` frame whose payload is not
                // a valid envelope (garbage bytes never get this far — the
                // frame decoder's magic scan drops them). Peer-side bug:
                // skip. Child logs belong on stderr.
                Err(_) => {
                    let text = String::from_utf8_lossy(&data);
                    log::debug!(
                        "[xacpp::stdio] skipping DATA_JSON payload with invalid envelope: {text}"
                    );
                    continue;
                }
            };

            match envelope {
                XacppEnvelope::Request {
                    id,
                    session_id,
                    payload,
                } => {
                    let sid_for_response = session_id.clone();
                    // Release read lock immediately after cloning Arc, avoid holding lock across await
                    let handler = shared.request_handler.read().await.clone();
                    let handler_result: Result<XacppResponse, XacppError> = if let Some(h) = handler
                    {
                        h(session_id, payload).await
                    } else {
                        Err(XacppError::NoHandler)
                    };

                    let response_payload = match handler_result {
                        Ok(payload) => payload,
                        Err(e) => {
                            log::error!("accept: handler error for request {id}: {e}");
                            XacppResponse::Error {
                                code: e.code().to_owned(),
                                message: e.to_string(),
                            }
                        }
                    };

                    let response = XacppEnvelope::Response {
                        id: id.clone(),
                        session_id: sid_for_response,
                        payload: response_payload,
                    };
                    match Self::serialize_envelope(&response) {
                        Ok(frame) => {
                            if let Err(e) = egress.send_acked(frame).await {
                                log::warn!("accept: failed to send response for request {id}: {e}");
                            }
                        }
                        Err(e) => {
                            log::warn!(
                                "accept: failed to serialize response for request {id}: {e}"
                            );
                        }
                    }
                }
                XacppEnvelope::Response { id, payload, .. } => {
                    let mut pending_guard = shared.pending.lock().await;
                    match pending_guard.remove(&id) {
                        Some(PendingSlot::Respond(sender)) => {
                            let _ = sender.send(payload);
                        }
                        // Fire-and-forget ack: silently consumed.
                        Some(PendingSlot::Drop) => {}
                        None => {
                            log::warn!("accept: received response for unknown request {id}");
                        }
                    }
                }
            }
        }

        // On exit: connection dead — fail fast subsequent sends, drop pending
        // waiters, shut the egress down.
        log::info!("accept: loop exited, cleaning up");
        shared.connected.store(false, Ordering::Release);
        shared.pending.lock().await.clear();
        egress.close();
    }
}

#[async_trait]
impl XacppTransport for StdioTransport {
    async fn connect(&self) -> Result<(), XacppError> {
        let _guard = self.connect_lock.lock().await;

        if self.shared.connected.load(Ordering::Acquire) {
            return Err(XacppError::AlreadyConnected);
        }

        let writer = self
            .parked_writer
            .lock()
            .await
            .take()
            .ok_or(XacppError::AlreadyConnected)?;
        let mut reader = self
            .reader
            .lock()
            .await
            .take()
            .ok_or(XacppError::AlreadyConnected)?;

        // Ordered egress: drain task owns the write half.
        let (egress, _egress_handle) = egress::spawn_egress(
            writer,
            Arc::clone(&self.shared.connected),
            Arc::clone(&self.shared.pending),
            self.closed_tx.clone(),
        );
        *self.egress.lock().expect("poisoned egress lock") = Some(egress.clone());

        let (frame_tx, frame_rx) = tokio::sync::mpsc::channel(256);
        let reader_hooks = Arc::clone(&self.transfer_hooks);
        let reader_handle = tokio::spawn(async move {
            // Binary wire: chunk-read the stream into the frame decoder.
            // `DATA_JSON` payloads forward to the accept loop (envelope
            // channel). Every other frame routes into the registered
            // transfer hooks (control dispatch + `DATA_BINARY` reassembly).
            // The hooks are re-read per frame so a late registration
            // (responder side, after connect) routes immediately.
            let mut decoder = FrameDecoder::new();
            let mut buf = [0u8; 8192];
            'read: loop {
                match reader.read(&mut buf).await {
                    Ok(0) => break, // EOF
                    Ok(n) => {
                        let frames = match decoder.feed(&buf[..n]) {
                            Ok(frames) => frames,
                            // Fatal (version mismatch / CRC / size limits,
                            // SPEC §7): decoder state is drained — surface as
                            // a connection error and tear down.
                            Err(e) => {
                                log::error!("reader: fatal frame decode error: {e}");
                                let io_err =
                                    std::io::Error::new(std::io::ErrorKind::InvalidData, e);
                                let _ = frame_tx.send(Err(io_err)).await;
                                break;
                            }
                        };
                        for frame in frames {
                            match frame.frame_type {
                                FrameType::DataJson => {
                                    // SPEC §5: any inbound frame refreshes the
                                    // peer-activity clock. Envelope frames never
                                    // reach the transfer dispatch point, so the
                                    // reader refreshes here — an envelope-only
                                    // connection must not look like a silent
                                    // peer to the clock.
                                    let hooks =
                                        reader_hooks.read().expect("poisoned hooks lock").clone();
                                    if let Some(hooks) = hooks {
                                        (hooks.on_activity)().await;
                                    }
                                    if frame_tx.send(Ok(frame.payload)).await.is_err() {
                                        break 'read;
                                    }
                                }
                                other => {
                                    let hooks =
                                        reader_hooks.read().expect("poisoned hooks lock").clone();
                                    if let Some(hooks) = hooks {
                                        (hooks.on_frame)(frame).await;
                                    } else {
                                        log::debug!(
                                            "reader: ignoring non-DATA_JSON frame (type {other:?}) — no transfer session wired"
                                        );
                                    }
                                }
                            }
                        }
                    }
                    Err(e) => {
                        let _ = frame_tx.send(Err(e)).await;
                        break;
                    }
                }
            }
        });

        let shared = Arc::clone(&self.shared);
        let accept_handle = tokio::spawn(async move {
            Self::accept_loop(frame_rx, egress, shared).await;
        });

        self.shared.connected.store(true, Ordering::Release);
        *self.shared.reader_handle.lock().await = Some(reader_handle);
        *self.shared.accept_handle.lock().await = Some(accept_handle);

        log::debug!("connect: transport connected");
        Ok(())
    }

    async fn disconnect(&self) -> Result<(), XacppError> {
        let _guard = self.connect_lock.lock().await;

        // Stop accepting new outbound traffic, then shut the egress down
        // (queued frames flush FIFO before the writer closes).
        self.shared.connected.store(false, Ordering::Release);
        if let Some(egress) = self.egress.lock().expect("poisoned egress lock").take() {
            egress.close();
        }

        // Abort both tasks
        if let Some(handle) = self.shared.reader_handle.lock().await.take() {
            handle.abort();
        }
        if let Some(handle) = self.shared.accept_handle.lock().await.take() {
            handle.abort();
        }

        self.shared.pending.lock().await.clear();

        log::debug!("disconnect: transport disconnected");
        Ok(())
    }

    async fn send(
        &self,
        session_id: Option<&str>,
        payload: XacppRequest,
    ) -> Result<XacppResponse, XacppError> {
        let id = self.next_id();
        let envelope = XacppEnvelope::Request {
            id: id.clone(),
            session_id: session_id.map(String::from),
            payload,
        };
        let frame = Self::serialize_envelope(&envelope)?;

        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.shared.pending.lock().await;
            pending.insert(id.clone(), PendingSlot::Respond(tx));
        }

        let egress = self.egress()?;
        if let Err(e) = egress.send_acked(frame).await {
            self.shared.pending.lock().await.remove(&id);
            return Err(e);
        }

        match rx.await {
            Ok(response_payload) => Ok(response_payload),
            Err(_) => {
                log::warn!("send: oneshot cancelled for request {id}, peer likely disconnected");
                Err(XacppError::Closed)
            }
        }
    }

    async fn send_faf(
        &self,
        session_id: Option<&str>,
        payload: XacppRequest,
    ) -> Result<(), XacppError> {
        let id = self.next_id();
        let envelope = XacppEnvelope::Request {
            id: id.clone(),
            session_id: session_id.map(String::from),
            payload,
        };
        let frame = Self::serialize_envelope(&envelope)?;

        // Drop slot: the peer's ack is silently consumed on arrival.
        self.shared
            .pending
            .lock()
            .await
            .insert(id.clone(), PendingSlot::Drop);

        let egress = self.egress()?;
        if let Err(e) = egress.send_faf(frame) {
            self.shared.pending.lock().await.remove(&id);
            return Err(e);
        }
        Ok(())
    }

    fn on_request(&self, handler: RequestHandler) -> Result<(), XacppError> {
        if self.shared.connected.load(Ordering::Acquire) {
            return Err(XacppError::AlreadyConnected);
        }
        let mut guard = self
            .shared
            .request_handler
            .try_write()
            .expect("on_request: lock contention before connect");
        *guard = Some(handler);
        Ok(())
    }

    fn closed(&self) -> watch::Receiver<bool> {
        self.closed_tx.subscribe()
    }
}

impl TransferWiring for StdioTransport {
    fn write_control_frame(&self, bytes: Vec<u8>) -> Result<(), XacppError> {
        let egress = self.egress()?;
        egress.send_control(bytes)
    }

    fn write_data_frame(&self, frame: Frame) -> Result<(), XacppError> {
        let egress = self.egress()?;
        let wire = frame
            .encode()
            .map_err(|e| XacppError::Internal(e.to_string()))?;
        egress.send_faf(wire)
    }

    fn on_transfer_activity(&self, handlers: Option<TransferActivityHandlers>) {
        *self.transfer_hooks.write().expect("poisoned hooks lock") = handlers;
    }
}
