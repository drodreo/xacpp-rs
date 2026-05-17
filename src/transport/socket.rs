//! Socket Transport Implementation.
//!
//! Communicates via TCP connection using JSONL frame protocol (one message per line, delimited by `\n`).
//! Key difference from StdioTransport: each inbound request spawns independent task for concurrent handling.

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{Mutex, RwLock, oneshot};
use tokio::task::{JoinHandle, JoinSet};

use super::{RequestHandler, XacppTransport};
use crate::error::XacppError;
use crate::message::{XacppEnvelope, XacppRequest, XacppResponse};

type BoxedWriter = Pin<Box<dyn AsyncWrite + Send>>;
type BoxedReader = Pin<Box<dyn AsyncBufRead + Send>>;

/// Shared state: accessed via Arc between reader task and inflight handler tasks.
struct SharedState {
    request_handler: RwLock<Option<RequestHandler>>,
    pending: Mutex<HashMap<String, oneshot::Sender<XacppResponse>>>,
    connected: AtomicBool,
    /// dispatch task handle (including reader_task), aborted on disconnect.
    reader_handle: Mutex<Option<JoinHandle<()>>>,
    /// inflight handler tasks, all aborted on disconnect.
    inflight: Mutex<JoinSet<()>>,
}

/// Socket Transport Implementation.
pub struct SocketTransport {
    writer: Arc<Mutex<Option<BoxedWriter>>>,
    reader: Mutex<Option<BoxedReader>>,
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
        Self {
            writer: Arc::new(Mutex::new(None)),
            reader: Mutex::new(None),
            shared: Arc::new(SharedState {
                request_handler: RwLock::new(None),
                pending: Mutex::new(HashMap::new()),
                connected: AtomicBool::new(false),
                reader_handle: Mutex::new(None),
                inflight: Mutex::new(JoinSet::new()),
            }),
            next_id: Arc::new(AtomicU64::new(1)),
            addr: Some(addr),
            connect_lock: Mutex::new(()),
        }
    }

    /// Create server Transport, using already accepted TcpStream.
    ///
    /// On connect, directly uses this stream without initiating TCP connection.
    pub fn new(stream: TcpStream) -> Self {
        let (read_half, write_half) = tokio::io::split(stream);
        Self {
            writer: Arc::new(Mutex::new(Some(Box::pin(write_half)))),
            reader: Mutex::new(Some(Box::pin(
                tokio::io::BufReader::new(read_half),
            ))),
            shared: Arc::new(SharedState {
                request_handler: RwLock::new(None),
                pending: Mutex::new(HashMap::new()),
                connected: AtomicBool::new(false),
                reader_handle: Mutex::new(None),
                inflight: Mutex::new(JoinSet::new()),
            }),
            next_id: Arc::new(AtomicU64::new(1)),
            addr: None,
            connect_lock: Mutex::new(()),
        }
    }

    fn next_id(&self) -> String {
        format!("r{}", self.next_id.fetch_add(1, Ordering::Relaxed))
    }

    /// Serialize wire message and send (JSONL format).
    async fn send_envelope(
        writer: &Arc<Mutex<Option<BoxedWriter>>>,
        msg: &XacppEnvelope,
    ) -> Result<(), XacppError> {
        let json = serde_json::to_vec(msg).map_err(|e| XacppError::Internal(e.to_string()))?;

        let mut guard = writer.lock().await;
        let w = guard.as_mut().ok_or(XacppError::NotConnected)?;

        w.write_all(&json).await.map_err(|_| XacppError::Closed)?;
        w.write_all(b"\n").await.map_err(|_| XacppError::Closed)?;
        w.flush().await.map_err(|_| XacppError::Closed)?;
        Ok(())
    }

    /// Reader task: read frames from TCP connection, parse, dispatch.
    async fn reader_task(
        mut frame_rx: tokio::sync::mpsc::Receiver<std::io::Result<Vec<u8>>>,
        writer: Arc<Mutex<Option<BoxedWriter>>>,
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
                    log::warn!("reader: failed to parse envelope ({} bytes): {e}\n  raw: {text}", data.len());
                    continue;
                }
            };

            match envelope {
                XacppEnvelope::Request { id, session_id, payload } => {
                    let sid_for_response = session_id.clone();
                    // Release read lock immediately after cloning Arc
                    let handler = shared.request_handler.read().await.clone();
                    if let Some(h) = handler {
                        let writer = Arc::clone(&writer);
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
                            if let Err(e) = Self::send_envelope(&writer, &response).await {
                                log::warn!("handler: failed to send response: {e}");
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
                        if let Err(e) = Self::send_envelope(&writer, &response).await {
                            log::warn!("reader: failed to send no_handler response: {e}");
                        }
                    }
                }
                XacppEnvelope::Response { id, payload, .. } => {
                    let mut pending_guard = shared.pending.lock().await;
                    if let Some(sender) = pending_guard.remove(&id) {
                        let _ = sender.send(payload);
                    } else {
                        log::warn!("reader: received response for unknown request {id}");
                    }
                }
            }
        }

        // Cleanup on exit
        log::info!("reader: task exited, cleaning up");
        {
            let mut guard = writer.lock().await;
            if let Some(mut w) = guard.take() {
                let _ = w.shutdown().await;
            }
        }
        shared.pending.lock().await.clear();
        shared.connected.store(false, Ordering::Release);
    }
}

#[async_trait]
impl XacppTransport for SocketTransport {
    async fn connect(&self) -> Result<(), XacppError> {
        let _guard = self.connect_lock.lock().await;

        if self.shared.connected.load(Ordering::Acquire) {
            return Err(XacppError::AlreadyConnected);
        }

        // Get reader: client mode needs to establish TCP connection first
        let reader = if let Some(ref addr) = self.addr {
            let stream = TcpStream::connect(addr).await.map_err(|e| {
                XacppError::Internal(format!("connect to {addr}: {e}"))
            })?;
            let (read_half, write_half) = tokio::io::split(stream);
            *self.writer.lock().await = Some(Box::pin(write_half));
            Box::pin(tokio::io::BufReader::new(read_half))
                as BoxedReader
        } else {
            self.reader
                .lock()
                .await
                .take()
                .ok_or(XacppError::AlreadyConnected)?
        };

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

        let writer = Arc::clone(&self.writer);
        let shared = Arc::clone(&self.shared);
        let dispatch_handle = tokio::spawn(async move {
            Self::reader_task(frame_rx, writer, shared).await;
        });

        self.shared.connected.store(true, Ordering::Release);
        *self.shared.reader_handle.lock().await = Some(dispatch_handle);

        log::debug!("connect: socket transport connected");
        Ok(())
    }

    async fn disconnect(&self) -> Result<(), XacppError> {
        let _guard = self.connect_lock.lock().await;

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

        // Shutdown writer
        {
            let mut guard = self.writer.lock().await;
            if let Some(mut w) = guard.take() {
                let _ = w.shutdown().await;
            }
        }

        self.shared.pending.lock().await.clear();
        self.shared.connected.store(false, Ordering::Release);

        log::debug!("disconnect: socket transport disconnected");
        Ok(())
    }

    async fn send(
        &self,
        session_id: Option<&str>,
        payload: XacppRequest,
    ) -> Result<XacppResponse, XacppError> {
        let id = self.next_id();
        let (tx, rx) = oneshot::channel();

        {
            let mut pending = self.shared.pending.lock().await;
            pending.insert(id.clone(), tx);
        }

        let envelope = XacppEnvelope::Request {
            id: id.clone(),
            session_id: session_id.map(String::from),
            payload,
        };
        if let Err(e) = Self::send_envelope(&self.writer, &envelope).await {
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

    fn on_request(&self, handler: RequestHandler) -> Result<(), XacppError> {
        if self.shared.connected.load(Ordering::Acquire) {
            return Err(XacppError::AlreadyConnected);
        }
        let mut guard = self.shared.request_handler.try_write()
            .expect("on_request: lock contention before connect");
        *guard = Some(handler);
        Ok(())
    }
}
