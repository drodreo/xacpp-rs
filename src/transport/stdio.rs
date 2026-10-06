//! Stdio Transport Implementation.
//!
//! Communicates via stdin/stdout pipe handles using JSONL frame protocol (one message per line, delimited by `\n`).
//!
//! All outbound frames (requests, events, responses) go through the single
//! ordered egress queue (see `super::egress`): one drain task owns the write
//! half and writes frames FIFO in submission order.

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use async_trait::async_trait;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite};
use tokio::sync::{Mutex, RwLock, oneshot, watch};
use tokio::task::JoinHandle;

use super::egress::{self, Egress, PendingSlot};
use super::{RequestHandler, XacppTransport};
use crate::error::XacppError;
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
    /// Ordered egress entry (present after connect).
    egress: Mutex<Option<Egress>>,
    /// Connection-close broadcast (see `XacppTransport::closed`).
    closed_tx: watch::Sender<bool>,
    shared: Arc<SharedState>,
    next_id: Arc<AtomicU64>,
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
            egress: Mutex::new(None),
            closed_tx,
            shared: Arc::new(SharedState {
                request_handler: RwLock::new(None),
                pending: Arc::new(Mutex::new(HashMap::new())),
                connected: Arc::new(AtomicBool::new(false)),
                reader_handle: Mutex::new(None),
                accept_handle: Mutex::new(None),
            }),
            next_id: Arc::new(AtomicU64::new(1)),
            connect_lock: Mutex::new(()),
        }
    }

    fn next_id(&self) -> String {
        format!("r{}", self.next_id.fetch_add(1, Ordering::Relaxed))
    }

    /// Serialize wire message (JSONL payload without trailing newline).
    fn serialize_envelope(msg: &XacppEnvelope) -> Result<Vec<u8>, XacppError> {
        serde_json::to_vec(msg).map_err(|e| XacppError::Internal(e.to_string()))
    }

    /// Clone the ordered egress entry (`Err(NotConnected)` before connect).
    async fn egress(&self) -> Result<Egress, XacppError> {
        self.egress
            .lock()
            .await
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
                Err(_) => {
                    let text = String::from_utf8_lossy(&data);
                    log::debug!("[xacpp::stdio::log] {text}");
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
        let reader = self
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
        *self.egress.lock().await = Some(egress.clone());

        let (frame_tx, frame_rx) = tokio::sync::mpsc::channel(256);
        let reader_handle = tokio::spawn(async move {
            let mut lines = reader.lines();
            loop {
                match lines.next_line().await {
                    Ok(Some(line)) => {
                        if frame_tx.send(Ok(line.into_bytes())).await.is_err() {
                            break;
                        }
                    }
                    Ok(None) => break,
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
        if let Some(egress) = self.egress.lock().await.take() {
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

        let egress = self.egress().await?;
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

        let egress = self.egress().await?;
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
