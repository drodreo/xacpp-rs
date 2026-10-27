//! Session layer: logical sessions, peer lifecycle orchestration, session
//! egress scheduling, and consumer-facing handler traits.

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
//! and the recovery handshake runs at wire time (the holder declares its
//! pending sends; the receiver adjudicates per transaction).

pub mod egress;
pub mod handler;
pub mod peer;

use std::sync::Arc;

use tokio::sync::RwLock;

use crate::carrier::XacppTransport;
use crate::reliability::stream::{BinaryMeta, BinarySink, StreamHandler};
use crate::transfer::{BinaryHandler, CompletionCallback, TransferConfig, TransferSession};
use crate::wire::commands::XacppCommand;
use crate::wire::error::XacppError;
use crate::wire::events::XacppActivityEvent;
use crate::wire::fetch::FetchReadyMeta;
use crate::wire::message::{XacppRequest, XacppResponse};

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
    /// responder: set by the consumer inside `on_establish`). Lock:
    /// identity is written once during construction / handshake.
    session_id: tokio::sync::RwLock<String>,
    /// Credentials issued by the responder (same lifecycle as `session_id`).
    credentials: tokio::sync::RwLock<String>,
    /// Negotiated emit_events snapshot (initiator: backfilled by the peer
    /// after negotiation; responder: optional, consumer-injected). Empty
    /// disables event-name validation.
    emit_events: RwLock<Vec<String>>,
    /// Negotiated fetch availability (SPEC §8.7): `true` only when both
    /// sides declared the fetch extension at the supported version. A
    /// negotiated fact — initiator: backfilled by the peer at establish;
    /// responder: consumer-injected from the negotiated effective
    /// capabilities. Default `false`: fetch-family outbound commands fail
    /// loudly with `fetch_unavailable` instead of reaching the wire.
    fetch_enabled: RwLock<bool>,
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
            session_id: tokio::sync::RwLock::new(String::new()),
            credentials: tokio::sync::RwLock::new(String::new()),
            emit_events: RwLock::new(Vec::new()),
            fetch_enabled: RwLock::new(false),
            transport: RwLock::new(None),
        }
    }

    /// Sets the session identity (session_id + credentials). Used by the
    /// responder side inside `on_establish`, where the consumer generates
    /// both before wiring.
    pub async fn with_identity(self, session_id: String, credentials: String) -> Self {
        *self.session_id.write().await = session_id;
        *self.credentials.write().await = credentials;
        self
    }

    /// Sets the negotiated emit_events snapshot used to validate outbound
    /// events (empty = no validation). Initiator: backfilled automatically
    /// by `XacppPeer`. Responder: optional, consumer-injected from the
    /// negotiated effective capabilities.
    pub async fn set_emit_events(&self, emit_events: Vec<String>) {
        *self.emit_events.write().await = emit_events;
    }

    /// Sets the negotiated fetch availability (SPEC §8.7). Responder side:
    /// the consumer injects the value from the negotiated effective
    /// capabilities (`NegotiateHandler.on_negotiate`) — it is the negotiated
    /// fact, not an assembly decision. Initiator side: backfilled
    /// automatically by the peer at establish.
    pub async fn set_fetch_enabled(&self, enabled: bool) {
        *self.fetch_enabled.write().await = enabled;
    }

    /// Negotiated fetch availability snapshot (SPEC §8.7).
    pub async fn fetch_enabled(&self) -> bool {
        *self.fetch_enabled.read().await
    }

    /// Wires the session to a **live** transport for the connection's
    /// lifetime: the session's transfer clock starts (self-driven) and the
    /// transport routes inbound binary/control frames into this session;
    /// the recovery handshake (RESUME declaration / adjudication) runs at
    /// wire time.
    /// Surviving streams stalled by the previous disconnect return to their
    /// flow state (Active/Paused) and the same objects keep
    /// producing/consuming — docs/streams.md ("session 不死，流不灭").
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
        // Recovery discipline: a consumer-driven wire (the responder side
        // inside on_establish) runs when the peer's transfer plane is
        // already live — declare pending sends now. The peer's own
        // initiator-side path wires at connect via `wire_without_declaration`
        // and declares only after the establish handshake completes.
        self.transfer.declare_pending_sends().await;
        Ok(())
    }

    /// Wire WITHOUT the recovery declaration: used by the peer at connect
    /// time on the initiator side — the responder's session does not exist
    /// until the upcoming establish, so a declaration now would be dropped
    /// by the not-yet-wired peer, deadlocking the holder. The peer fires
    /// `declare_pending_sends` once establish completes.
    pub(crate) async fn wire_without_declaration(
        &self,
        transport: Arc<dyn XacppTransport>,
    ) -> Result<(), XacppError> {
        self.transfer.unwire().await;
        *self.transport.write().await = None;
        self.transfer.wire(Arc::clone(&transport)).await?;
        *self.transport.write().await = Some(transport);
        Ok(())
    }

    /// Recovery discipline trigger for the peer: send the RESUME
    /// declaration for every pending send. Fired when the establish
    /// handshake completes — the only moment both transfer planes are
    /// wired on an establish topology. Idempotent.
    pub(crate) async fn declare_pending_sends(&self) {
        self.transfer.declare_pending_sends().await;
    }

    /// Detaches the session from its transport (clock stopped, inbound
    /// routing disabled, receive-side streams stalled — alive, not closed).
    /// Transaction state stays with the session; streams cancel only when
    /// the session itself is dropped.
    pub async fn unwire(&self) {
        self.transfer.unwire().await;
        *self.transport.write().await = None;
    }

    /// Ensures the transfer mount is attached to the given transport
    /// (idempotent no-op while attached). Used by the peer when an
    /// establish re-binds this session — `connect` performs the initial
    /// mount.
    ///
    /// The transfer mount (binary frames, control frames, the session
    /// clock) is a separate concern from the envelope mount (the
    /// command/event path): this touches the transfer side only, the
    /// session's envelope identity and its transport field stay as they
    /// are.
    pub(crate) async fn attach_transfer(
        &self,
        transport: Arc<dyn XacppTransport>,
    ) -> Result<(), XacppError> {
        if self.transfer.is_wired().await {
            return Ok(());
        }
        self.transfer.wire(transport).await
    }

    /// Detaches ONLY the transfer mount (clock stopped, single-slot
    /// handler registration released): the envelope identity stays
    /// wired — the envelope channel routes by `session_id`. Used by the
    /// peer when an establish yields a different session identity on
    /// this connection: an orphaned transfer clock would never see
    /// inbound activity again and its silence timer would tear the
    /// shared connection down.
    pub(crate) async fn detach_transfer(&self) {
        self.transfer.unwire().await;
    }

    /// Whether the transfer mount is currently attached to a transport.
    #[doc(hidden)]
    pub async fn transfer_wired(&self) -> bool {
        self.transfer.is_wired().await
    }

    /// Session identifier.
    pub async fn session_id(&self) -> String {
        self.session_id.read().await.clone()
    }

    /// Credentials issued by the responder.
    ///
    /// Caller can save them and pass them in during next `establish`.
    pub async fn credentials(&self) -> String {
        self.credentials.read().await.clone()
    }

    /// Sends a command and waits for a response.
    ///
    /// Fetch-family commands are gated by the negotiated fetch availability
    /// (SPEC §8.7): while fetch is not enabled, they fail loudly with
    /// `fetch_unavailable` (the same error shape the peer's inbound gate
    /// answers) and never reach the wire.
    pub async fn request_command(
        &self,
        command: XacppCommand,
    ) -> Result<XacppResponse, XacppError> {
        if command.is_fetch_family() && !*self.fetch_enabled.read().await {
            return Err(XacppError::Application {
                code: crate::wire::fetch::error_codes::FETCH_UNAVAILABLE.into(),
                message:
                    "fetch unavailable on this end (fetch extension not enabled by negotiation)"
                        .into(),
            });
        }
        let transport = self.transport().await?;
        transport
            .send(
                Some(&self.session_id().await),
                XacppRequest::Command(command),
            )
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
            .send(Some(&self.session_id().await), XacppRequest::Event(event))
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
            .send_faf(Some(&self.session_id().await), XacppRequest::Event(event))
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
    /// confirmation (the receiver's resume verdict failed the transaction
    /// after a reconnect, or caller abort).
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
        crate::transfer::send_binary(&self.transfer, transport.as_ref(), payload, on_complete).await
    }

    /// Opens a streaming send transaction (docs/streams.md): declare the
    /// total length, then write chunks through the returned [`BinarySink`]
    /// — writes park while the backpressure cap is reached (recovery is
    /// automatic as ACKs release per-frame copies) and while the sink is
    /// paused; `finish()` waits for the final ACK; `pause()`/`resume()`/
    /// `abort()` map to the SPEC §5 transaction control frames.
    ///
    /// `txid`: `None` = library-generated. A caller-supplied id must be
    /// unique among in-flight transactions and must not have been aborted
    /// earlier (SPEC §8.2: the fetch responder passes the id it announced
    /// in `fetch_ready`).
    pub async fn open_binary_sink(
        &self,
        meta: BinaryMeta,
        txid: Option<[u8; 16]>,
    ) -> Result<BinarySink, XacppError> {
        crate::transfer::open_binary_sink(&self.transfer, meta, txid).await
    }

    /// Register the delivery callback for fully reassembled binary
    /// transactions (single handler; a later registration replaces the
    /// earlier one). Transactions completed while no handler was registered
    /// are retained and delivered exactly once once a handler is present.
    pub async fn on_binary(&self, handler: BinaryHandler) {
        self.transfer.on_binary(handler).await;
    }

    /// Register the receive-side stream handler (docs/streams.md): the
    /// first frame of each new transaction opens a
    /// [`crate::transport::BinaryStream`]; chunks flow in `seq` order and
    /// the stream ends on completion, holder abort (TRANSACTION_ABORT), or
    /// connection death. Meta (`txid` / `total_len`) is readable on the
    /// first frame — the fetch requester claims its stream by matching the
    /// `fetch_ready` txid (SPEC §8).
    ///
    /// While a stream handler is registered, new transactions belong to the
    /// stream surface; `on_binary` keeps receiving only transactions that
    /// completed before registration (or sessions without stream
    /// registration).
    pub async fn on_binary_stream(&self, handler: StreamHandler) {
        self.transfer.on_binary_stream(handler).await;
    }

    /// Fetch a resource's metadata from the peer (SPEC §8 kernel shell):
    /// sends the `fetch` command and returns the `fetch_ready` metadata —
    /// the bytes follow as a `DATA_BINARY` transaction the consumer claims
    /// via `on_binary_stream` by matching the returned txid. The kernel
    /// carries no bytes and keeps no state: buffering, integrity
    /// verification, and progress are application-layer concerns (§8.6).
    ///
    /// Responder-side errors surface through the generic error channel
    /// (e.g. `fetch_not_found` / `fetch_refused` / `fetch_unavailable`).
    pub async fn fetch(&self, uri: &str) -> Result<FetchReadyMeta, XacppError> {
        let response = self
            .request_command(XacppCommand::Fetch {
                uri: uri.to_string(),
            })
            .await?;
        crate::wire::fetch::response_to_meta(response)
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

    /// Validates an outbound event name against the negotiated snapshot.
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

    /// Backfills identity + negotiated facts after the establish handshake
    /// completes (initiator side; peer-internal).
    pub(crate) async fn backfill(
        &self,
        session_id: String,
        credentials: String,
        emit_events: Vec<String>,
        fetch_enabled: bool,
    ) {
        *self.session_id.write().await = session_id;
        *self.credentials.write().await = credentials;
        *self.emit_events.write().await = emit_events;
        *self.fetch_enabled.write().await = fetch_enabled;
    }
}
