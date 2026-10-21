//! XACPP Peer — Protocol layer endpoint.
//!
//! ## Responsibility
//!
//! Peer is a protocol layer entity, representing one endpoint in a communication link. Core responsibilities:
//!
//! - **Lifecycle orchestration**: connect / negotiate / establish / disconnect / state (pure
//!   lifecycle — outgoing communication goes through the session)
//! - **Protocol state machine**: Manages connection state
//! - **Session routing**: Routes inbound requests to corresponding Session handler based on session_id
//!
//! ## Boundary with Transport and Session
//!
//! Peer holds `Arc<dyn XacppTransport>` (composition), all underlying IO is delegated to Transport.
//! Peer is unaware of envelope id, encoding/decoding, request-response correlation and other details (all encapsulated by Transport).
//! The initiator-side [`XacppSession`] is handed in at construction and wired automatically:
//! `connect` wires it to the transport, `establish` backfills the identity, `disconnect` unwires.
//! The responder side constructs and wires its sessions inside `on_establish` (the library does
//! not force an identity-matching strategy).

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::{Mutex, RwLock};

use crate::carrier::XacppTransport;
use crate::session::XacppSession;
use crate::session::handler::{EstablishHandler, NegotiateHandler, XacppSessionHandler};
use crate::wire::capability::{Capabilities, EffectiveCapabilities};
use crate::wire::commands::XacppCommand;
use crate::wire::error::XacppError;
use crate::wire::message::{XacppRequest, XacppResponse};

/// Peer protocol state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerState {
    /// Not connected / connection closed.
    Disconnected,
    /// Communication channel established, negotiation pending.
    Connected,
    /// Capabilities negotiated, logical session can be created.
    Negotiated,
}

/// XacppPeer shared state.
struct PeerInner {
    state: PeerState,
    remote_capabilities: Capabilities,
    /// negotiate 后缓存的交集结果（local.produce_events ∩ remote.accept_events）
    emit_events: Vec<String>,
}

/// XACPP protocol endpoint.
///
/// Each communication party holds a `XacppPeer` instance and exchanges messages through a shared Transport.
#[derive(Clone)]
pub struct XacppPeer {
    transport: Arc<dyn XacppTransport>,
    /// Initiator-side logical session (handed in at construction, wired
    /// automatically by `connect` / `establish` / `disconnect`). Responder
    /// side: `None` — sessions are constructed and wired by the consumer
    /// inside `on_establish`.
    session: Option<Arc<XacppSession>>,
    inner: Arc<Mutex<PeerInner>>,
    /// Shared emit_events cache for Responder-side negotiation in connect closure.
    /// The closure cannot access `inner` directly, so we use this Arc to share state.
    emit_events: Arc<Mutex<Vec<String>>>,
    sessions: Arc<RwLock<HashMap<String, Arc<dyn XacppSessionHandler>>>>,
    establish_handler: Arc<dyn EstablishHandler>,
    local_capabilities: Capabilities,
    negotiate_handler: Arc<dyn NegotiateHandler>,
}

impl XacppPeer {
    /// Creates a new Peer instance.
    ///
    /// `session`: initiator-side logical session (`Some`); the peer wires it
    /// to the transport on `connect`, backfills identity + negotiated
    /// emit_events on `establish`, and unwires on `disconnect`.
    /// Responder side: pass `None`.
    ///
    /// Initial state is `Disconnected`; call `connect` to establish a connection.
    pub fn new(
        capabilities: Capabilities,
        transport: Arc<dyn XacppTransport>,
        session: Option<Arc<XacppSession>>,
        negotiate_handler: Arc<dyn NegotiateHandler>,
        establish_handler: Arc<dyn EstablishHandler>,
    ) -> Self {
        Self {
            transport,
            session,
            inner: Arc::new(Mutex::new(PeerInner {
                state: PeerState::Disconnected,
                remote_capabilities: Capabilities {
                    commands: Vec::new(),
                    produce_events: Vec::new(),
                    accept_events: Vec::new(),
                },
                emit_events: Vec::new(),
            })),
            emit_events: Arc::new(Mutex::new(Vec::new())),
            sessions: Arc::new(RwLock::new(HashMap::new())),
            establish_handler,
            local_capabilities: capabilities,
            negotiate_handler,
        }
    }

    /// Current protocol state.
    pub async fn state(&self) -> PeerState {
        self.inner.lock().await.state
    }

    /// Queries the remote peer's negotiated capabilities.
    pub async fn remote_capabilities(&self) -> Capabilities {
        self.inner.lock().await.remote_capabilities.clone()
    }

    /// Queries the negotiated emit_events (local.produce_events ∩ remote.accept_events).
    pub async fn emit_events(&self) -> Vec<String> {
        self.inner.lock().await.emit_events.clone()
    }

    // ---- Negotiation ----

    /// Initiates capability negotiation.
    ///
    /// Must be called after `connect()` and before `establish()`.
    /// Sends local capabilities to the peer, processes the peer's capabilities
    /// via the registered `NegotiateHandler`, and transitions to `Negotiated` state.
    pub async fn negotiate(&self) -> Result<(), XacppError> {
        let state = self.state().await;
        if state != PeerState::Connected {
            return Err(XacppError::Internal(format!(
                "negotiate requires Connected state, current: {:?}",
                state
            )));
        }

        let response = self
            .transport
            .send(
                None,
                XacppRequest::Command(XacppCommand::Negotiate {
                    capabilities: self.local_capabilities.clone(),
                }),
            )
            .await?;

        match response {
            XacppResponse::Negotiated { capabilities } => {
                // 协议层计算交集
                let effective = EffectiveCapabilities::from_capabilities(
                    &self.local_capabilities,
                    &capabilities,
                );
                self.negotiate_handler
                    .on_negotiate(effective.clone())
                    .await?;
                let mut inner = self.inner.lock().await;
                inner.remote_capabilities = capabilities;
                inner.emit_events = effective.emit_events;
                inner.state = PeerState::Negotiated;
                Ok(())
            }
            XacppResponse::Error { code, message } => {
                Err(XacppError::Application { code, message })
            }
            other => Err(XacppError::Internal(format!(
                "unexpected response to negotiate: {:?}",
                other
            ))),
        }
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
        let negotiate_handler = Arc::clone(&self.negotiate_handler);
        let local_capabilities = self.local_capabilities.clone();
        let emit_events = Arc::clone(&self.emit_events);

        self.transport
            .on_request(Arc::new(move |session_id, payload| {
                let sessions = Arc::clone(&sessions);
                let establish_handler = Arc::clone(&establish_handler);
                let transport = Arc::clone(&transport);
                let negotiate_handler = Arc::clone(&negotiate_handler);
                let local_capabilities = local_capabilities.clone();
                let emit_events = Arc::clone(&emit_events);
                Box::pin(async move {
                    match (session_id, payload) {
                        // Pre-session Negotiate request
                        (None, XacppRequest::Command(XacppCommand::Negotiate { capabilities })) => {
                            // 协议层计算交集
                            let effective = EffectiveCapabilities::from_capabilities(
                                &local_capabilities,
                                &capabilities,
                            );
                            negotiate_handler.on_negotiate(effective.clone()).await?;
                            // 缓存 emit_events 交集结果（Responder 端无法直接访问 inner）
                            *emit_events.lock().await = effective.emit_events;
                            Ok(XacppResponse::Negotiated {
                                capabilities: local_capabilities,
                            })
                        }
                        // Pre-session Establish request
                        (None, XacppRequest::Command(XacppCommand::Establish { credentials })) => {
                            match establish_handler.on_establish(transport, credentials).await {
                                Ok(decision) => match decision {
                                    crate::session::handler::EstablishDecision::ChallengeRequired {
                                        challenge,
                                    } => Ok(XacppResponse::EstablishPrepare { challenge }),
                                    crate::session::handler::EstablishDecision::Established {
                                        session_id,
                                        handler,
                                        credentials,
                                    } => {
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
                        (None, _) => Err(XacppError::InvalidRequest("missing session_id".into())),
                        // Route to Session handler. Fetch-family commands are
                        // gated (SPEC §8.2): with no loaded fetch responder
                        // the library answers `fetch_unavailable` loudly —
                        // never a silent fall-through to the consumer. A
                        // loaded responder receives the commands untouched
                        // (the fetch kernel stays stateless).
                        (Some(sid), XacppRequest::Command(cmd)) => {
                            let handler = { sessions.read().await.get(&sid).cloned() };
                            match handler {
                                Some(h) => {
                                    if cmd.is_fetch_family() && !h.is_fetch_responder() {
                                        Ok(XacppResponse::Error {
                                            code: crate::wire::fetch::error_codes::FETCH_UNAVAILABLE
                                                .into(),
                                            message:
                                                "fetch unsupported on this end (no fetch responder loaded)"
                                                    .into(),
                                        })
                                    } else {
                                        h.on_command(cmd).await
                                    }
                                }
                                None => {
                                    Err(XacppError::Internal(format!("unknown session: {sid}")))
                                }
                            }
                        }
                        (Some(sid), XacppRequest::Event(evt)) => {
                            let handler = { sessions.read().await.get(&sid).cloned() };
                            match handler {
                                Some(h) => h.on_event(evt).await,
                                None => {
                                    Err(XacppError::Internal(format!("unknown session: {sid}")))
                                }
                            }
                        }
                    }
                })
            }))?;

        self.transport.connect().await?;

        // Wire the initiator-side session now that the connection is live:
        // the session's transfer clock starts (self-driven) and inbound
        // routing attaches. The recovery DECLARATION is deliberately not
        // sent yet — the responder builds and wires its session only inside
        // the upcoming establish, so a declaration at connect would be
        // dropped by the not-yet-wired peer and deadlock the holder; the
        // peer fires it when establish completes. Idempotent: a retry after
        // a failed `connect` attempt replaces the stale wiring.
        if let Some(session) = &self.session {
            session
                .wire_without_declaration(Arc::clone(&self.transport))
                .await?;
        }

        let mut inner = self.inner.lock().await;
        if inner.state == PeerState::Disconnected {
            inner.state = PeerState::Connected;
        }
        Ok(())
    }

    /// Establishes a logical session.
    ///
    /// Sends Establish command to the peer with optional authentication credentials and session handler.
    /// Handler is registered to Peer routing table; the initiator-side session (handed in at
    /// construction) gets its identity backfilled and is returned.
    ///
    /// If the responder returns `EstablishPrepare` (challenge path), `verify_challenge` is invoked
    /// to validate the challenge; on success, an `EstablishConfirm` is sent to complete the handshake.
    pub async fn establish(
        &self,
        credentials: Option<String>,
        handler: Arc<dyn XacppSessionHandler>,
        verify_challenge: impl FnOnce(String) -> Result<(), XacppError>,
    ) -> Result<Arc<XacppSession>, XacppError> {
        // The peer must hold an initiator-side session (the handshake is
        // driven from it). Which session gets BOUND is decided by the
        // responder's answer — see `session_for_establish`.
        if self.session.is_none() {
            return Err(XacppError::Internal(
                "establish requires an initiator-side session (pass Some at XacppPeer::new)".into(),
            ));
        }

        let state = self.state().await;
        if state != PeerState::Negotiated {
            return Err(XacppError::Internal(format!(
                "establish requires Negotiated state, current: {:?}",
                state
            )));
        }

        log::debug!(
            "establish: sending Establish (credentials: {})",
            if credentials.is_some() {
                "present"
            } else {
                "none"
            }
        );

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
                log::debug!(
                    "establish: received Established (session_id: {}, credentials: {})",
                    session_id,
                    if credentials.is_empty() {
                        "empty"
                    } else {
                        "present"
                    }
                );
                self.sessions
                    .write()
                    .await
                    .insert(session_id.clone(), Arc::clone(&handler));
                let session = self.session_for_establish(&session_id).await?;
                session
                    .backfill(
                        session_id,
                        credentials,
                        self.inner.lock().await.emit_events.clone(),
                    )
                    .await;
                // Recovery discipline: the handshake is complete and the
                // responder wired its transfer plane inside on_establish —
                // declare pending sends now (the declaration at connect
                // would have been dropped by the not-yet-wired peer).
                session.declare_pending_sends().await;
                Ok(session)
            }
            XacppResponse::EstablishPrepare { challenge } => {
                log::debug!(
                    "establish: received EstablishPrepare (challenge: {})",
                    challenge
                );
                verify_challenge(challenge)?;
                log::debug!("establish: sending EstablishConfirm");
                let confirm_response = self
                    .transport
                    .send(None, XacppRequest::Command(XacppCommand::EstablishConfirm))
                    .await?;
                match confirm_response {
                    XacppResponse::Established {
                        session_id,
                        credentials,
                    } => {
                        log::debug!(
                            "establish: received Established after confirm (session_id: {}, credentials: {})",
                            session_id,
                            if credentials.is_empty() {
                                "empty"
                            } else {
                                "present"
                            }
                        );
                        self.sessions
                            .write()
                            .await
                            .insert(session_id.clone(), Arc::clone(&handler));
                        let session = self.session_for_establish(&session_id).await?;
                        session
                            .backfill(
                                session_id,
                                credentials,
                                self.inner.lock().await.emit_events.clone(),
                            )
                            .await;
                        // Recovery discipline: handshake complete (challenge
                        // path) — declare pending sends now that both
                        // transfer planes are wired.
                        session.declare_pending_sends().await;
                        Ok(session)
                    }
                    XacppResponse::EstablishReject { reason } => {
                        log::debug!(
                            "establish: received EstablishReject after confirm (reason: {reason})"
                        );
                        Err(XacppError::EstablishReject { reason })
                    }
                    XacppResponse::Error { code, message } => {
                        log::debug!(
                            "establish: received Error after confirm (code: {code}, message: {message})"
                        );
                        Err(XacppError::Application { code, message })
                    }
                    other => {
                        log::debug!(
                            "establish: received unexpected response after confirm: {other:?}"
                        );
                        Err(XacppError::Internal(format!(
                            "unexpected response to establish_confirm: {other:?}"
                        )))
                    }
                }
            }
            XacppResponse::EstablishReject { reason } => {
                log::debug!("establish: received EstablishReject (reason: {reason})");
                Err(XacppError::EstablishReject { reason })
            }
            XacppResponse::Error { code, message } => {
                log::debug!("establish: received Error (code: {code}, message: {message})");
                Err(XacppError::Application { code, message })
            }
            other => {
                log::debug!("establish: received unexpected response: {other:?}");
                Err(XacppError::Internal(format!(
                    "unexpected response to establish: {other:?}"
                )))
            }
        }
    }

    /// Session to bind for an establish success, driven by the
    /// responder's answer (identity continuity is the responder's
    /// verdict — credentials are the anchor, the answer's `session_id`
    /// is the ruling):
    ///
    /// - The answer names the held session's bound identity — or the
    ///   held session is not bound yet: the held session is reused, its
    ///   transfer mount re-attached idempotently. Its transaction
    ///   state, receive slots and send copies survive the re-establish.
    /// - The answer names a different identity: a new logical session is
    ///   born. The held session's transfer mount is detached first — the
    ///   mount is a single slot, and an orphaned transfer clock never
    ///   sees inbound activity again: its silence timer would declare
    ///   the peer dead and tear THIS connection down ~90s later. The
    ///   held session's envelope identity stays wired — the envelope
    ///   channel routes by `session_id`, so coexisting sessions on one
    ///   connection keep their command/event paths.
    async fn session_for_establish(
        &self,
        session_id: &str,
    ) -> Result<Arc<XacppSession>, XacppError> {
        if let Some(held) = &self.session {
            let bound = held.session_id().await;
            if bound.is_empty() || bound == session_id {
                held.attach_transfer(Arc::clone(&self.transport)).await?;
                return Ok(Arc::clone(held));
            }
            held.detach_transfer().await;
        }
        let fresh = Arc::new(XacppSession::new(None));
        fresh.wire(Arc::clone(&self.transport)).await?;
        Ok(fresh)
    }

    /// Disconnects.
    pub async fn disconnect(&self) -> Result<(), XacppError> {
        // Unwire the initiator-side session first (clock stops, inbound
        // routing detaches — transaction state stays with the session).
        if let Some(session) = &self.session {
            session.unwire().await;
        }
        self.transport.disconnect().await?;
        let mut inner = self.inner.lock().await;
        inner.state = PeerState::Disconnected;
        inner.remote_capabilities = Capabilities {
            commands: Vec::new(),
            produce_events: Vec::new(),
            accept_events: Vec::new(),
        };
        inner.emit_events.clear();
        self.sessions.write().await.clear();
        Ok(())
    }
}
