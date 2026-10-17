//! XACPP logical session.
//!
//! Created via `XacppPeer::establish` on the initiator side; on the responder
//! side, constructed directly inside `on_establish` from the transport
//! parameter (see [`crate::handler::EstablishHandler`]).
//! Multiple Sessions under the same Peer share the same connection: the
//! envelope channel routes by `session_id` (each envelope carries it), while
//! the binary transfer mount point on a transport is a single slot — wiring
//! multiple sessions' transfers onto one transport concurrently is not
//! supported (per-transaction routing is future evolution).
//!
//! ## Ownership model
//!
//! The session owns its [`TransferSession`] (SPEC §5) for its whole
//! lifetime: transaction state (send copies, receive slots, resume anchors)
//! lives here and survives connection loss. While a connection is up the
//! session is *wired* to the transport (`wire` / `unwire`) — the session's
//! transfer clock is self-driven while wired, and the transport routes
//! inbound binary/control frames into the session through the narrow
//! `TransferWiring` hook surface. Breakpoint resume across reconnections is
//! the natural consequence: wire the same session into the new transport
//! and the RESUME handshake declares its incomplete receive transactions to
//! the peer at wire time.

use std::sync::Arc;

use tokio::sync::RwLock;

use crate::commands::XacppCommand;
use crate::error::XacppError;
use crate::events::XacppActivityEvent;
use crate::message::{XacppRequest, XacppResponse};
use crate::transport::{
    BinaryHandler, CompletionCallback, TransferConfig, TransferSession, XacppTransport,
};

/// XACPP logical session.
///
/// Created via `XacppPeer::establish`, holds an independent session_id and credentials.
/// Multiple Sessions under the same Peer share the same connection.
pub struct XacppSession {
    /// Transfer state (SPEC §5): send copies, receive slots, resume
    /// anchors. Session-owned, survives connection loss.
    transfer: TransferSession,
    /// Session identifier. Empty until the identity is set (initiator:
    /// backfilled by `XacppPeer::establish` from the handshake response;
    /// responder: set by the consumer inside `on_establish`). Plain lock:
    /// identity is written once during construction / handshake.
    session_id: std::sync::RwLock<String>,
    /// Credentials issued by the responder (same lifecycle as `session_id`).
    credentials: std::sync::RwLock<String>,
    /// Negotiated emit_events snapshot (initiator: backfilled by the peer
    /// after negotiation; responder: optional, consumer-injected). Empty
    /// disables event-name validation.
    emit_events: RwLock<Vec<String>>,
    /// Wired transport (present while a connection is attached).
    transport: RwLock<Option<Arc<dyn XacppTransport>>>,
}

impl XacppSession {
    /// Creates a session handle with its own transfer state.
    ///
    /// `config`: transfer configuration (SPEC §5); `None` uses the
    /// defaults. The identity (`session_id` / `credentials`) starts empty
    /// and is backfilled when the establish handshake completes.
    pub fn new(config: Option<TransferConfig>) -> Self {
        Self {
            transfer: TransferSession::new(config.unwrap_or_default()),
            session_id: std::sync::RwLock::new(String::new()),
            credentials: std::sync::RwLock::new(String::new()),
            emit_events: RwLock::new(Vec::new()),
            transport: RwLock::new(None),
        }
    }

    /// Sets the session identity (session_id + credentials). Used by the
    /// responder side inside `on_establish`, where the consumer generates
    /// both before wiring.
    pub fn with_identity(self, session_id: String, credentials: String) -> Self {
        *self.session_id.write().expect("poisoned session_id lock") = session_id;
        *self.credentials.write().expect("poisoned credentials lock") = credentials;
        self
    }

    /// Sets the negotiated emit_events snapshot used to validate outbound
    /// events (empty = no validation). Initiator: backfilled automatically
    /// by `XacppPeer`. Responder: optional, consumer-injected from the
    /// negotiated effective capabilities.
    pub async fn set_emit_events(&self, emit_events: Vec<String>) {
        *self.emit_events.write().await = emit_events;
    }

    /// Wires the session to a **live** transport for the connection's
    /// lifetime: the session's transfer clock starts (self-driven) and the
    /// transport routes inbound binary/control frames into this session;
    /// the RESUME handshake for incomplete receives runs at wire time.
    ///
    /// Any previous wiring (e.g. the dead connection's transport after a
    /// reconnect) is detached first: the mount is idempotent-replacing.
    /// This mirrors the TS implementation's observable behavior, where the
    /// session auto-detaches on connection close before the next wire.
    pub async fn wire(&self, transport: Arc<dyn XacppTransport>) -> Result<(), XacppError> {
        self.transfer.unwire().await;
        *self.transport.write().await = None;
        self.transfer.wire(Arc::clone(&transport)).await?;
        *self.transport.write().await = Some(transport);
        Ok(())
    }

    /// Detaches the session from its transport (clock stopped, inbound
    /// routing disabled). Transaction state stays with the session.
    pub async fn unwire(&self) {
        self.transfer.unwire().await;
        *self.transport.write().await = None;
    }

    /// Session identifier.
    pub fn session_id(&self) -> String {
        self.session_id
            .read()
            .expect("poisoned session_id lock")
            .clone()
    }

    /// Credentials issued by the responder.
    ///
    /// Caller can save them and pass them in during next `establish`.
    pub fn credentials(&self) -> String {
        self.credentials
            .read()
            .expect("poisoned credentials lock")
            .clone()
    }

    /// Sends a command and waits for a response.
    pub async fn request_command(
        &self,
        command: XacppCommand,
    ) -> Result<XacppResponse, XacppError> {
        let transport = self.transport().await?;
        transport
            .send(Some(&self.session_id()), XacppRequest::Command(command))
            .await
    }

    /// Sends an event and waits for a response.
    ///
    /// Protocol layer validates that the event name is in the negotiated
    /// emit_events snapshot (when non-empty).
    pub async fn request_event(
        &self,
        event: XacppActivityEvent,
    ) -> Result<XacppResponse, XacppError> {
        self.validate_event(&event).await?;
        let transport = self.transport().await?;
        transport
            .send(Some(&self.session_id()), XacppRequest::Event(event))
            .await
    }

    /// Sends an event fire-and-forget: returns as soon as the frame is
    /// queued for writing, never waits for the ack.
    ///
    /// Ordering against other outbound frames is preserved by the
    /// transport's single FIFO egress. `Err` = connection already known dead.
    pub async fn send_event(&self, event: XacppActivityEvent) -> Result<(), XacppError> {
        self.validate_event(&event).await?;
        let transport = self.transport().await?;
        transport
            .send_faf(Some(&self.session_id()), XacppRequest::Event(event))
            .await
    }

    /// Sends a raw binary payload as a multi-frame `DATA_BINARY` transaction
    /// (SPEC §5): split into working frames by the session's
    /// `TransactionSender`, frames queued on the egress, copy retained until
    /// the peer's final ACK. Never waits for the ACK.
    ///
    /// Returns the transaction id (also handed to `on_complete` context and
    /// visible to the peer's `on_binary` handler). `on_complete` fires
    /// exactly once when the transaction reaches a terminal state, with a
    /// [`BinarySendResult`]: `confirmed: true` once the final ACK releases
    /// the copy, `confirmed: false` when the copy is released without
    /// confirmation (RESUME rejected by the holder, or retention expiry).
    ///
    /// Backpressure (§5): when the unacknowledged-bytes cap is reached this
    /// returns `Err(XacppError::Application { code: "backpressure_stall",
    /// .. })` immediately — the caller retries later; recovery is automatic
    /// as ACKs release copies.
    pub async fn send_binary(
        &self,
        payload: Vec<u8>,
        on_complete: Option<CompletionCallback>,
    ) -> Result<[u8; 16], XacppError> {
        let transport = self.transport().await?;
        crate::transport::transfer::send_binary(
            &self.transfer,
            transport.as_ref(),
            payload,
            on_complete,
        )
        .await
    }

    /// Register the delivery callback for fully reassembled binary
    /// transactions (single handler; a later registration replaces the
    /// earlier one). Transactions completed while no handler was registered
    /// are retained and delivered exactly once once a handler is present.
    pub fn on_binary(&self, handler: BinaryHandler) {
        self.transfer.on_binary(handler);
    }

    /// Subscribe to the underlying connection's close notification.
    ///
    /// Check `*rx.borrow()` after subscribing: the connection may already be
    /// closed before the subscription. `Err(NotConnected)` when no transport
    /// is wired.
    pub async fn closed(&self) -> Result<tokio::sync::watch::Receiver<bool>, XacppError> {
        let transport = self.transport().await?;
        Ok(transport.closed())
    }

    /// Borrowed wired transport (`Err(NotConnected)` when unwired).
    async fn transport(&self) -> Result<Arc<dyn XacppTransport>, XacppError> {
        self.transport
            .read()
            .await
            .clone()
            .ok_or(XacppError::NotConnected)
    }

    /// Event-name validation against the negotiated emit_events snapshot
    /// (empty snapshot disables validation).
    async fn validate_event(&self, event: &XacppActivityEvent) -> Result<(), XacppError> {
        let emit_events = self.emit_events.read().await.clone();
        if !emit_events.is_empty() && !emit_events.contains(&event.event.name) {
            return Err(XacppError::Internal(format!(
                "event '{}' not in negotiated emit_events capability",
                event.event.name
            )));
        }
        Ok(())
    }

    /// Backfills identity + negotiated emit_events after the establish
    /// handshake completes (initiator side; peer-internal).
    pub(crate) async fn backfill(
        &self,
        session_id: String,
        credentials: String,
        emit_events: Vec<String>,
    ) {
        *self.session_id.write().expect("poisoned session_id lock") = session_id;
        *self.credentials.write().expect("poisoned credentials lock") = credentials;
        *self.emit_events.write().await = emit_events;
    }
}
