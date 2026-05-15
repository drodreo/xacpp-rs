//! XACPP Peer — Protocol layer endpoint.
//!
//! ## Responsibility
//!
//! Peer is a protocol layer entity, representing one endpoint in a communication link. Core responsibilities:
//!
//! - **Typed operations**: Encapsulates Transport's payload layer API into semantically clear Command / Event operations
//! - **Protocol state machine**: Manages connection state
//! - **Session routing**: Routes inbound requests to corresponding Session handler based on session_id
//!
//! ## Boundary with Transport
//!
//! Peer holds `Arc<dyn XacppTransport>` (composition), all underlying IO is delegated to Transport.
//! Peer is unaware of envelope id, encoding/decoding, request-response correlation and other details (all encapsulated by Transport).

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::{Mutex, RwLock};

use crate::commands::XacppCommand;
use crate::error::XacppError;
use crate::events::XacppActivityEvent;
use crate::handler::{EstablishHandler, XacppSessionHandler};
use crate::message::{XacppRequest, XacppResponse};
use crate::session::XacppSession;
use crate::transport::XacppTransport;

/// Peer protocol state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerState {
    /// Not connected / connection closed.
    Disconnected,
    /// Communication channel established, logical session can be created.
    Connected,
}

/// XacppPeer shared state.
struct PeerInner {
    state: PeerState,
}

/// XACPP protocol endpoint.
///
/// Each communication party holds a `XacppPeer` instance and exchanges messages through a shared Transport.
#[derive(Clone)]
pub struct XacppPeer {
    transport: Arc<dyn XacppTransport>,
    inner: Arc<Mutex<PeerInner>>,
    sessions: Arc<RwLock<HashMap<String, Arc<dyn XacppSessionHandler>>>>,
    establish_handler: Arc<dyn EstablishHandler>,
}

impl XacppPeer {
    /// Creates a new Peer instance.
    ///
    /// Initial state is `Disconnected`; call `connect` to establish a connection.
    pub fn new(
        transport: Arc<dyn XacppTransport>,
        establish_handler: Arc<dyn EstablishHandler>,
    ) -> Self {
        Self {
            transport,
            inner: Arc::new(Mutex::new(PeerInner {
                state: PeerState::Disconnected,
            })),
            sessions: Arc::new(RwLock::new(HashMap::new())),
            establish_handler,
        }
    }

    /// Current protocol state.
    pub async fn state(&self) -> PeerState {
        self.inner.lock().await.state
    }

    // ---- Connection Management ----

    /// Establishes a connection.
    ///
    /// Registers routing closures with Transport, then starts the underlying communication channel.
    /// On success, state transitions to `Connected`, subsequent calls to `establish` can create logical sessions.
    pub async fn connect(&self) -> Result<(), XacppError> {
        let sessions = Arc::clone(&self.sessions);
        let establish_handler = Arc::clone(&self.establish_handler);
        let transport = Arc::clone(&self.transport);

        self.transport.on_request(Arc::new(move |session_id, payload| {
            let sessions = Arc::clone(&sessions);
            let establish_handler = Arc::clone(&establish_handler);
            let transport = Arc::clone(&transport);
            Box::pin(async move {
                match (session_id, payload) {
                    // Pre-session Establish request
                    (None, XacppRequest::Command(XacppCommand::Establish { credentials })) => {
                        match establish_handler.on_establish(transport, credentials).await {
                            Ok(decision) => match decision {
                                crate::handler::EstablishDecision::ChallengeRequired { challenge } => {
                                    Ok(XacppResponse::EstablishPrepare { challenge })
                                }
                                crate::handler::EstablishDecision::Established { session_id, handler, credentials } => {
                                    sessions.write().await.insert(session_id.clone(), handler);
                                    Ok(XacppResponse::Established {
                                        session_id,
                                        credentials,
                                    })
                                }
                            },
                            Err(e) => Err(e),
                        }
                    }
                    // Pre-session EstablishConfirm request
                    (None, XacppRequest::Command(XacppCommand::EstablishConfirm)) => {
                        match establish_handler.on_establish_confirm(transport).await {
                            Ok((sid, handler, creds)) => {
                                sessions.write().await.insert(sid.clone(), handler);
                                Ok(XacppResponse::Established {
                                    session_id: sid,
                                    credentials: creds,
                                })
                            }
                            Err(e) => Err(e),
                        }
                    }
                    // Other requests without session_id are invalid
                    (None, _) => Err(XacppError::InvalidRequest(
                        "missing session_id".into(),
                    )),
                    // Route to Session handler
                    (Some(sid), XacppRequest::Command(cmd)) => {
                        let handler = {
                            sessions.read().await.get(&sid).cloned()
                        };
                        match handler {
                            Some(h) => h.on_command(cmd).await,
                            None => Err(XacppError::Internal(format!(
                                "unknown session: {sid}"
                            ))),
                        }
                    }
                    (Some(sid), XacppRequest::Event(evt)) => {
                        let handler = {
                            sessions.read().await.get(&sid).cloned()
                        };
                        match handler {
                            Some(h) => h.on_event(evt).await,
                            None => Err(XacppError::Internal(format!(
                                "unknown session: {sid}"
                            ))),
                        }
                    }
                }
            })
        }))?;

        self.transport.connect().await?;
        let mut inner = self.inner.lock().await;
        if inner.state == PeerState::Disconnected {
            inner.state = PeerState::Connected;
        }
        Ok(())
    }

    /// Establishes a logical session.
    ///
    /// Sends Establish command to the peer with optional authentication credentials and session handler.
    /// Handler is registered to Peer routing table, Session is responsible for sending.
    ///
    /// If the responder returns `EstablishPrepare` (challenge path), `verify_challenge` is invoked
    /// to validate the challenge; on success, an `EstablishConfirm` is sent to complete the handshake.
    pub async fn establish(
        &self,
        credentials: Option<String>,
        handler: Arc<dyn XacppSessionHandler>,
        verify_challenge: impl FnOnce(String) -> Result<(), XacppError>,
    ) -> Result<XacppSession, XacppError> {
        let response = self
            .transport
            .send(
                None,
                XacppRequest::Command(XacppCommand::Establish { credentials }),
            )
            .await?;

        match response {
            XacppResponse::Established {
                session_id,
                credentials,
            } => {
                self.sessions
                    .write()
                    .await
                    .insert(session_id.clone(), Arc::clone(&handler));
                Ok(XacppSession::new(
                    Arc::clone(&self.transport),
                    session_id,
                    credentials,
                ))
            }
            XacppResponse::EstablishPrepare { challenge } => {
                verify_challenge(challenge)?;
                let confirm_response = self
                    .transport
                    .send(
                        None,
                        XacppRequest::Command(XacppCommand::EstablishConfirm),
                    )
                    .await?;
                match confirm_response {
                    XacppResponse::Established {
                        session_id,
                        credentials,
                    } => {
                        self.sessions
                            .write()
                            .await
                            .insert(session_id.clone(), Arc::clone(&handler));
                        Ok(XacppSession::new(
                            Arc::clone(&self.transport),
                            session_id,
                            credentials,
                        ))
                    }
                    XacppResponse::EstablishReject { reason } => {
                        Err(XacppError::EstablishReject { reason })
                    }
                    XacppResponse::Error { code, message } => {
                        Err(XacppError::Application { code, message })
                    }
                    other => Err(XacppError::Internal(format!(
                        "unexpected response to establish_confirm: {other:?}"
                    ))),
                }
            }
            XacppResponse::EstablishReject { reason } => {
                Err(XacppError::EstablishReject { reason })
            }
            XacppResponse::Error { code, message } => {
                Err(XacppError::Application { code, message })
            }
            other => Err(XacppError::Internal(format!(
                "unexpected response to establish: {other:?}"
            ))),
        }
    }

    /// Disconnects.
    pub async fn disconnect(&self) -> Result<(), XacppError> {
        self.transport.disconnect().await?;
        let mut inner = self.inner.lock().await;
        inner.state = PeerState::Disconnected;
        self.sessions.write().await.clear();
        Ok(())
    }

    // ---- Outgoing Requests ----

    /// Sends a command and waits for a response (no session context).
    pub async fn request_command(
        &self,
        session_id: Option<&str>,
        command: XacppCommand,
    ) -> Result<XacppResponse, XacppError> {
        self.transport
            .send(session_id, XacppRequest::Command(command))
            .await
    }

    /// Sends an interactive event and waits for a response (no session context).
    pub async fn request_event(
        &self,
        session_id: Option<&str>,
        event: XacppActivityEvent,
    ) -> Result<XacppResponse, XacppError> {
        self.transport
            .send(session_id, XacppRequest::Event(event))
            .await
    }
}
