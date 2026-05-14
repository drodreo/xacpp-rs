//! Stdio Transport Implementation.
//!
//! Communicates via stdin/stdout pipe handles using JSONL frame protocol (one message per line, delimited by `\n`).

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{Mutex, RwLock, oneshot};
use tokio::task::JoinHandle;

use super::{RequestHandler, XacppTransport};
use crate::error::XacppError;
use crate::message::{XacppEnvelope, XacppRequest, XacppResponse};

/// Stdio internal send state.
struct StdioInner {
    writer: Option<Pin<Box<dyn AsyncWrite + Send>>>,
}

/// Shared state: accessed via Arc between accept loop and main task.
struct SharedState {
    request_handler: RwLock<Option<RequestHandler>>,
    pending: Mutex<HashMap<String, oneshot::Sender<XacppResponse>>>,
    connected: AtomicBool,
    /// reader task handle, aborted on disconnect.
    reader_handle: Mutex<Option<JoinHandle<()>>>,
    /// accept_loop task handle, aborted on disconnect.
    accept_handle: Mutex<Option<JoinHandle<()>>>,
}

/// Stdio Transport Implementation.
pub struct StdioTransport {
    inner: Arc<Mutex<StdioInner>>,
    reader: Mutex<Option<Pin<Box<dyn AsyncBufRead + Send>>>>,
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
        Self {
            inner: Arc::new(Mutex::new(StdioInner {
                writer: Some(writer),
            })),
            reader: Mutex::new(Some(reader)),
            shared: Arc::new(SharedState {
                request_handler: RwLock::new(None),
                pending: Mutex::new(HashMap::new()),
                connected: AtomicBool::new(false),
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

    /// Serialize wire message and send (JSONL format).
    async fn send_envelope(inner: &Arc<Mutex<StdioInner>>, msg: &XacppEnvelope) -> Result<(), XacppError> {
        let json = serde_json::to_vec(msg).map_err(|e| XacppError::Internal(e.to_string()))?;

        let mut guard = inner.lock().await;
        let writer = guard.writer.as_mut().ok_or(XacppError::NotConnected)?;

        writer.write_all(&json).await.map_err(|_| XacppError::Closed)?;
        writer.write_all(b"\n").await.map_err(|_| XacppError::Closed)?;
        writer.flush().await.map_err(|_| XacppError::Closed)?;
        Ok(())
    }

    /// Accept loop: read from frame channel, parse, dispatch.
    ///
    /// On exit: disconnect writer and set connected to false, preventing subsequent send hang.
    async fn accept_loop(
        mut frame_rx: tokio::sync::mpsc::Receiver<std::io::Result<Vec<u8>>>,
        inner: Arc<Mutex<StdioInner>>,
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
                    log::warn!("accept: failed to parse envelope ({} bytes): {e}", data.len());
                    continue;
                }
            };

            match envelope {
                XacppEnvelope::Request { id, session_id, payload } => {
                    let sid_for_response = session_id.clone();
                    // Release read lock immediately after cloning Arc, avoid holding lock across await
                    let handler = shared.request_handler.read().await.clone();
                    let handler_result: Result<XacppResponse, XacppError> = if let Some(h) = handler {
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

                    let response = XacppEnvelope::Response { id: id.clone(), session_id: sid_for_response, payload: response_payload };
                    if let Err(e) = Self::send_envelope(&inner, &response).await {
                        log::warn!("accept: failed to send response for request {id}: {e}");
                    }
                }
                XacppEnvelope::Response { id, payload, .. } => {
                    let mut pending_guard = shared.pending.lock().await;
                    if let Some(sender) = pending_guard.remove(&id) {
                        let _ = sender.send(payload);
                    } else {
                        log::warn!("accept: received response for unknown request {id}");
                    }
                }
            }
        }

        // On exit: disconnect writer, clear pending, mark disconnected
        log::info!("accept: loop exited, cleaning up");
        {
            let mut guard = inner.lock().await;
            if let Some(mut writer) = guard.writer.take() {
                let _ = writer.shutdown().await;
            }
        }
        shared.pending.lock().await.clear();
        shared.connected.store(false, Ordering::Release);
    }
}

#[async_trait]
impl XacppTransport for StdioTransport {
    async fn connect(&self) -> Result<(), XacppError> {
        let _guard = self.connect_lock.lock().await;

        if self.shared.connected.load(Ordering::Acquire) {
            return Err(XacppError::AlreadyConnected);
        }

        let reader = self.reader.lock().await.take().ok_or(XacppError::AlreadyConnected)?;

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

        let inner = Arc::clone(&self.inner);
        let shared = Arc::clone(&self.shared);
        let accept_handle = tokio::spawn(async move {
            Self::accept_loop(frame_rx, inner, shared).await;
        });

        self.shared.connected.store(true, Ordering::Release);
        *self.shared.reader_handle.lock().await = Some(reader_handle);
        *self.shared.accept_handle.lock().await = Some(accept_handle);

        log::debug!("connect: transport connected");
        Ok(())
    }

    async fn disconnect(&self) -> Result<(), XacppError> {
        let _guard = self.connect_lock.lock().await;

        // Disconnect writer
        {
            let mut inner = self.inner.lock().await;
            if let Some(mut writer) = inner.writer.take() {
                let _ = writer.shutdown().await;
            }
        }

        // Abort both tasks
        if let Some(handle) = self.shared.reader_handle.lock().await.take() {
            handle.abort();
        }
        if let Some(handle) = self.shared.accept_handle.lock().await.take() {
            handle.abort();
        }

        self.shared.pending.lock().await.clear();
        self.shared.connected.store(false, Ordering::Release);

        log::debug!("disconnect: transport disconnected");
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
        if let Err(e) = Self::send_envelope(&self.inner, &envelope).await {
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
