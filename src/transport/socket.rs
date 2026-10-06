//! Socket Transport Implementation.
//!
//! Communicates via TCP connection using JSONL frame protocol (one message per line, delimited by `\n`).
//! Key difference from StdioTransport: each inbound request spawns independent task for concurrent handling.
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
use tokio::net::TcpStream;
use tokio::sync::{Mutex, RwLock, oneshot, watch};
use tokio::task::{JoinHandle, JoinSet};

use super::egress::{self, Egress, PendingSlot};
use super::{RequestHandler, XacppTransport};
use crate::error::XacppError;
use crate::message::{XacppEnvelope, XacppRequest, XacppResponse};

type BoxedWriter = Pin<Box<dyn AsyncWrite + Send>>;
type BoxedReader = Pin<Box<dyn AsyncBufRead + Send>>;

/// Shared state: accessed via Arc between reader task and inflight handler tasks.
struct SharedState {
    request_handler: RwLock<Option<RequestHandler>>,
    pending: egress::PendingMap,
    connected: Arc<AtomicBool>,
    /// dispatch task handle (including reader_task), aborted on disconnect.
    reader_handle: Mutex<Option<JoinHandle<()>>>,
    /// inflight handler tasks, all aborted on disconnect.
    inflight: Mutex<JoinSet<()>>,
}

/// Socket Transport Implementation.
pub struct SocketTransport {
    /// Write half parked before connect (server mode: split at construction).
    parked_writer: Mutex<Option<BoxedWriter>>,
    reader: Mutex<Option<BoxedReader>>,
    /// Ordered egress entry (present after connect).
    egress: Mutex<Option<Egress>>,
    /// Connection-close broadcast (see `XacppTransport::closed`).
    closed_tx: watch::Sender<bool>,
    shared: Arc<SharedState>,
    next_id: Arc<AtomicU64>,
    /// Remote address for client mode, used during connect.
    addr: Option<String>,
    /// Connection operation mutex, ensures connect / disconnect do not execute concurrently.
    connect_lock: Mutex<()>,
}

impl SocketTransport {
    /// Create client Transport, initiates TCP connection to specified address on connect.
    pub fn connect_to(addr: String) -> Self {
        Self::new_impl(None, Some(addr))
    }

    /// Create server Transport, using already accepted TcpStream.
    ///
    /// On connect, directly uses this stream without initiating TCP connection.
    pub fn new(stream: TcpStream) -> Self {
        let (read_half, write_half) = tokio::io::split(stream);
        let parked = (
            Box::pin(write_half) as BoxedWriter,
            Box::pin(tokio::io::BufReader::new(read_half)) as BoxedReader,
        );
        Self::new_impl(Some(parked), None)
    }

    fn new_impl(parked: Option<(BoxedWriter, BoxedReader)>, addr: Option<String>) -> Self {
        let (parked_writer, reader) = match parked {
            Some((w, r)) => (Some(w), Some(r)),
            None => (None, None),
        };
        let (closed_tx, _) = watch::channel(false);
        Self {
            parked_writer: Mutex::new(parked_writer),
            reader: Mutex::new(reader),
            egress: Mutex::new(None),
            closed_tx,
            shared: Arc::new(SharedState {
                request_handler: RwLock::new(None),
                pending: Arc::new(Mutex::new(HashMap::new())),
                connected: Arc::new(AtomicBool::new(false)),
                reader_handle: Mutex::new(None),
                inflight: Mutex::new(JoinSet::new()),
            }),
            next_id: Arc::new(AtomicU64::new(1)),
            addr,
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

    /// Reader task: read frames from TCP connection, parse, dispatch.
    async fn reader_task(
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
                Err(e) => {
                    let text = String::from_utf8_lossy(&data);
                    log::warn!(
                        "reader: failed to parse envelope ({} bytes): {e}\n  raw: {text}",
                        data.len()
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
                    // Release read lock immediately after cloning Arc
                    let handler = shared.request_handler.read().await.clone();
                    if let Some(h) = handler {
                        let egress = egress.clone();
                        let mut inflight = shared.inflight.lock().await;
                        inflight.spawn(async move {
                            let handler_result = h(session_id, payload).await;
                            let response_payload = match handler_result {
                                Ok(payload) => payload,
                                Err(e) => {
                                    log::error!("handler: error for request {id}: {e}");
                                    XacppResponse::Error {
                                        code: e.code().to_owned(),
                                        message: e.to_string(),
                                    }
                                }
                            };
                            let response = XacppEnvelope::Response {
                                id,
                                session_id: sid_for_response,
                                payload: response_payload,
                            };
                            match Self::serialize_envelope(&response) {
                                Ok(frame) => {
                                    if let Err(e) = egress.send_acked(frame).await {
                                        log::warn!("handler: failed to send response: {e}");
                                    }
                                }
                                Err(e) => {
                                    log::warn!("handler: failed to serialize response: {e}");
                                }
                            }
                        });
                    } else {
                        // No handler, return error response
                        let response = XacppEnvelope::Response {
                            id,
                            session_id: sid_for_response,
                            payload: XacppResponse::Error {
                                code: "no_handler".into(),
                                message: "no handler registered".into(),
                            },
                        };
                        match Self::serialize_envelope(&response) {
                            Ok(frame) => {
                                if let Err(e) = egress.send_acked(frame).await {
                                    log::warn!("reader: failed to send no_handler response: {e}");
                                }
                            }
                            Err(e) => {
                                log::warn!("reader: failed to serialize no_handler response: {e}");
                            }
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
                            log::warn!("reader: received response for unknown request {id}");
                        }
                    }
                }
            }
        }

        // Cleanup on exit: connection dead — fail fast subsequent sends,
        // drop pending waiters, shut the egress down.
        log::info!("reader: task exited, cleaning up");
        shared.connected.store(false, Ordering::Release);
        shared.pending.lock().await.clear();
        egress.close();
    }
}

#[async_trait]
impl XacppTransport for SocketTransport {
    async fn connect(&self) -> Result<(), XacppError> {
        let _guard = self.connect_lock.lock().await;

        if self.shared.connected.load(Ordering::Acquire) {
            return Err(XacppError::AlreadyConnected);
        }

        // Get reader + writer: client mode establishes TCP first;
        // server mode uses the halves parked at construction.
        let (writer, reader) = if let Some(ref addr) = self.addr {
            let stream = TcpStream::connect(addr)
                .await
                .map_err(|e| XacppError::Internal(format!("connect to {addr}: {e}")))?;
            let (read_half, write_half) = tokio::io::split(stream);
            (
                Box::pin(write_half) as BoxedWriter,
                Box::pin(tokio::io::BufReader::new(read_half)) as BoxedReader,
            )
        } else {
            let w = self
                .parked_writer
                .lock()
                .await
                .take()
                .ok_or(XacppError::AlreadyConnected)?;
            let r = self
                .reader
                .lock()
                .await
                .take()
                .ok_or(XacppError::AlreadyConnected)?;
            (w, r)
        };

        // Ordered egress: drain task owns the write half.
        let (egress, _egress_handle) = egress::spawn_egress(
            writer,
            Arc::clone(&self.shared.connected),
            Arc::clone(&self.shared.pending),
            self.closed_tx.clone(),
        );
        *self.egress.lock().await = Some(egress.clone());

        let (frame_tx, frame_rx) = tokio::sync::mpsc::channel(256);
        // Frame read task: no separate tracking needed, automatically stops when dispatch task drops frame_rx
        tokio::spawn(async move {
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
        let dispatch_handle = tokio::spawn(async move {
            Self::reader_task(frame_rx, egress, shared).await;
        });

        self.shared.connected.store(true, Ordering::Release);
        *self.shared.reader_handle.lock().await = Some(dispatch_handle);

        log::debug!("connect: socket transport connected");
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

        // Abort reader task
        if let Some(handle) = self.shared.reader_handle.lock().await.take() {
            handle.abort();
        }

        // Abort all inflight handler tasks
        {
            let mut inflight = self.shared.inflight.lock().await;
            inflight.abort_all();
            while inflight.join_next().await.is_some() {}
        }

        self.shared.pending.lock().await.clear();

        log::debug!("disconnect: socket transport disconnected");
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
