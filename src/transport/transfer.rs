//! Transfer session (SPEC §5): glues the pure transfer components
//! ([`TransactionSender`], [`TransactionReassembler`]) into the transport
//! lifecycle.
//!
//! ## Ownership model
//!
//! A [`TransferSession`] is created and held by its logical session
//! ([`crate::session::XacppSession`]) for the session's whole lifetime and
//! lives across connections. While a connection is up, the session is
//! *wired* to the transport (`XacppSession::wire`): the session's clock is
//! self-driven (a tick task started at wire, aborted at unwire) and the
//! transport routes inbound `DATA_BINARY` / control frames into it through
//! the narrow [`TransferWiring`] surface. When the connection dies the
//! wiring detaches — transaction state stays with the session, so
//! reconnecting (the same session wired into the new transport) resumes
//! incomplete receive transactions (`RESUME` handshake): the peer holder
//! answers and retransmits, completing the transfer across the connection
//! boundary. Breakpoint resume is the natural consequence of session-owned
//! state, not a separate binding ceremony.
//!
//! ## Clock driving
//!
//! The pure components take injected time; the session owns the clock.
//! While wired, a periodic tick task drives: batched ACK export (§5: every
//! `ack_batch_frames` frames or `ack_batch_interval_ms`), receiver NACK
//! feedback for gap lists, sender retransmissions, heartbeats, silent-peer
//! detection (dead-connection path: the session calls the transport's
//! `disconnect` itself), retention sweeps (`expire_stale` /
//! `expire_copies`; tombstoned copies notify the peer with `ERROR
//! transaction_expired`), and deferred delivery of payloads that completed
//! before a binary handler was registered (see
//! [`TransferSession::on_binary`]).
//!
//! ## Envelope channel
//!
//! The envelope channel (`DATA_JSON`, request-response) is untouched by
//! the transfer layer: it stays single-frame, never enters transactions,
//! and is not subject to backpressure (it is self-reliable via
//! request-response semantics).
//!
//! ## Backpressure form
//!
//! [`crate::session::XacppSession::send_binary`] never blocks the caller:
//! when the unacknowledged-bytes cap (§5) is reached it returns
//! `Err(XacppError::Application { code: "backpressure_stall", .. })`
//! immediately; the caller retries later. Recovery is automatic as final
//! ACKs release copies. In-flight frames (retransmissions) keep flowing —
//! the cap bounds retransmission storms per §5.

use std::collections::HashMap;
use std::sync::Arc;

use crate::control::{
    AckEntry, AckPayload, ErrorPayload, HeartbeatPayload, NackEntry, NackPayload,
    ResumeRequestPayload, ResumeResponsePayload, ResumeStatus, error_codes,
};
use crate::frame::{Frame, FrameType};
use crate::reassemble::{ReceiveOutcome, TransactionReassembler};
use crate::transaction::{BeginError, TransactionSender};

use super::XacppError;
use super::XacppTransport;

/// Terminal outcome of a `send_binary` transaction, delivered to its
/// completion callback exactly once (mirrors the TS implementation's
/// `BinarySendResult`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BinarySendResult {
    /// Transaction the result belongs to.
    pub transaction_id: [u8; 16],
    /// `true`: the final ACK confirmed delivery (send copy released).
    /// `false`: the copy was released without confirmation (the holder
    /// rejected the RESUME request, or the send-copy retention expired).
    pub confirmed: bool,
}

/// Callback invoked when a `send_binary` transaction reaches a terminal
/// state: confirmed delivery (final ACK received) or unconfirmed copy
/// release (RESUME rejected / retention expiry). Fired exactly once per
/// transaction, always outside the state lock. Must return quickly; long
/// work is the callback's own to spawn.
pub type CompletionCallback = Arc<dyn Fn(BinarySendResult) + Send + Sync>;

/// Callback invoked when a transaction is fully reassembled and delivered.
/// Receives the transaction id and the complete payload. Must return
/// quickly; long work is the callback's own to spawn.
pub type BinaryHandler = Arc<dyn Fn([u8; 16], Vec<u8>) + Send + Sync>;

/// Boxed future returned by the activity hooks (async hook signatures).
pub type ActivityFuture = std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>;

/// Inbound hooks a transfer session registers on a wired transport
/// ([`TransferWiring::on_transfer_activity`]).
#[derive(Clone)]
pub struct TransferActivityHandlers {
    /// Envelope (`DATA_JSON`) frame arrived — the peer is alive (silence
    /// clock refresh). The envelope payload itself never reaches the
    /// transfer layer (the transport forwards it to the envelope dispatch).
    pub on_activity: Arc<dyn Fn() -> ActivityFuture + Send + Sync>,
    /// Non-envelope frame arrived (`DATA_BINARY` / control) — transfer
    /// channel (reassembly + control dispatch).
    pub on_frame: Arc<dyn Fn(Frame) -> ActivityFuture + Send + Sync>,
}

/// Write/activity surface a transport provides for transfer-session
/// attachment (SPEC §5). Implemented by `SocketTransport` and
/// `StdioTransport`; consumed through `TransferSession::wire` (reached via
/// `XacppSession::wire`). `XacppTransport: TransferWiring` expresses the
/// TS `XacppTransport & TransferWiring` intersection.
pub trait TransferWiring: Send + Sync {
    /// Write one encoded control frame (egress scheduling priority, §5
    /// invariant 4). `Err` = connection unavailable.
    fn write_control_frame(&self, bytes: Vec<u8>) -> Result<(), XacppError>;

    /// Write one data frame, encoded (plain FIFO). `Err` = connection
    /// unavailable.
    fn write_data_frame(&self, frame: Frame) -> Result<(), XacppError>;

    /// Register the connection-activity / inbound-transfer-frame handlers
    /// (`None` unregisters). Registering replaces any previous handlers.
    fn on_transfer_activity(&self, handlers: Option<TransferActivityHandlers>);
}

/// Tunables for the transfer integration (component defaults per SPEC §5
/// RECOMMENDED values; transport-level knobs added on top).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransferConfig {
    /// Send-side component config.
    pub sender: crate::transaction::SenderConfig,
    /// Receive-side component config.
    pub reassembler: crate::reassemble::ReassemblerConfig,
    /// Heartbeat send period (§5 liveness). `u64::MAX` disables sending.
    pub heartbeat_interval_ms: u64,
    /// Connection is declared dead after this much time without any inbound
    /// frame from the peer (any frame refreshes the clock).
    pub silent_timeout_ms: u64,
    /// Batched ACK export: export once this many new frames accumulated.
    pub ack_batch_frames: u32,
    /// Batched ACK export: or once this long since the last export,
    /// whichever first (§5: RECOMMENDED every 64 frames or 500 ms).
    pub ack_batch_interval_ms: u64,
    /// Receiver NACK feedback period for transactions with gaps (§5:
    /// receivers return NACK feedback; the wire has no push event for it).
    pub nack_feedback_interval_ms: u64,
    /// Clock tick granularity (internal scheduling, not a SPEC value).
    pub tick_interval_ms: u64,
}

impl Default for TransferConfig {
    fn default() -> Self {
        Self {
            sender: Default::default(),
            reassembler: Default::default(),
            heartbeat_interval_ms: 30_000,
            silent_timeout_ms: 90_000,
            ack_batch_frames: 64,
            ack_batch_interval_ms: 500,
            nack_feedback_interval_ms: 500,
            tick_interval_ms: 100,
        }
    }
}

/// Shared session core: consumer-held, lives across connections.
struct SessionCore {
    config: TransferConfig,
    state: tokio::sync::Mutex<SessionState>,
    // Deliberately a std lock (not tokio): `on_binary` is a sync-signature
    // setter that must register the handler without a runtime, and the
    // critical section is a pure nanosecond-scale Option clone — blocking
    // the async runtime here is impossible in practice.
    binary_handler: std::sync::RwLock<Option<BinaryHandler>>,
    /// Wired transport (present while wired): write surface plus the
    /// `disconnect` handle for the silent-timeout path. Strong here — the
    /// transport's hooks and the clock task hold the session only weakly,
    /// so no reference cycle forms (the session is the long-lived side).
    wiring: std::sync::RwLock<Option<Arc<dyn XacppTransport>>>,
    /// Clock task handle (present while wired), aborted on unwire.
    clock: std::sync::RwLock<Option<tokio::task::JoinHandle<()>>>,
}

/// Mutable session state — components plus clock bookkeeping.
struct SessionState {
    sender: TransactionSender,
    reassembler: TransactionReassembler,
    /// `send_binary` completion callbacks, fired exactly once with the
    /// outcome (confirmed delivery or unconfirmed release).
    completions: HashMap<[u8; 16], CompletionCallback>,
    /// Newly received frames per transaction since the last ACK export.
    pending_ack_frames: HashMap<[u8; 16], u32>,
    last_ack_ms: u64,
    last_nack_ms: u64,
    heartbeat_seq: u32,
    last_heartbeat_ms: u64,
    /// Last instant any inbound frame arrived from the peer (all frames
    /// refresh it — liveness is proven by traffic, heartbeats make it
    /// explicit on quiet links).
    last_peer_activity_ms: u64,
}

/// Consumer-held transfer session: reassembler + sender + config, bound
/// optionally into a transport and living across connections (RESUME
/// anchor). Cloneable handle over shared state.
#[derive(Clone)]
pub struct TransferSession {
    core: Arc<SessionCore>,
}

impl TransferSession {
    /// Create a session with the given configuration.
    pub fn new(config: TransferConfig) -> Self {
        let now = now_ms();
        Self {
            core: Arc::new(SessionCore {
                state: tokio::sync::Mutex::new(SessionState {
                    sender: TransactionSender::new(config.sender),
                    reassembler: TransactionReassembler::new(config.reassembler),
                    completions: HashMap::new(),
                    pending_ack_frames: HashMap::new(),
                    last_ack_ms: now,
                    last_nack_ms: now,
                    heartbeat_seq: 0,
                    last_heartbeat_ms: now,
                    last_peer_activity_ms: now,
                }),
                binary_handler: std::sync::RwLock::new(None),
                wiring: std::sync::RwLock::new(None),
                clock: std::sync::RwLock::new(None),
                config,
            }),
        }
    }

    /// Borrowed configuration.
    pub fn config(&self) -> &TransferConfig {
        &self.core.config
    }

    /// Register the delivery callback for fully reassembled transactions
    /// (single handler; a later registration replaces the earlier one).
    ///
    /// Transactions that already completed while no handler was registered
    /// keep their payload retained (the final ACK is sent regardless — §5:
    /// "a final ACK on completion"); once a handler is present, the clock
    /// path hands every retained payload over exactly once (next tick).
    pub fn on_binary(&self, handler: BinaryHandler) {
        *self
            .core
            .binary_handler
            .write()
            .expect("on_binary: poisoned handler lock") = Some(handler);
    }

    fn deliver(&self, txid: [u8; 16], payload: Vec<u8>) {
        let handler = self.core.binary_handler.read().expect("poisoned").clone();
        if let Some(handler) = handler {
            handler(txid, payload);
        }
    }

    /// Wire the session to a **live** transport: starts the self-driven
    /// clock, registers the inbound hooks, and emits `RESUME` for every
    /// incomplete receive (§5) through the write surface — the TS
    /// attach-to-a-live-connection semantics.
    pub(crate) async fn wire(&self, transport: Arc<dyn XacppTransport>) -> Result<(), XacppError> {
        if self
            .core
            .wiring
            .read()
            .expect("poisoned wiring lock")
            .is_some()
        {
            return Err(XacppError::InvalidState(
                "transfer session already wired to a transport".into(),
            ));
        }
        // Reopen the liveness window: the silent-timeout anchor
        // (`last_peer_activity_ms`) travels with the session across
        // connections (transaction state does), but the observation window
        // belongs to the connection being attached — a session re-wired
        // after a silent-timeout disconnect must not inherit the previous
        // connection's accumulated silence and die on the new connection's
        // first tick (reconnect death loop). Send-scheduling anchors
        // (heartbeat / ACK batch / NACK) are deliberately not reset: the
        // first tick then resumes their cadence immediately — flush pending
        // ACKs, request gaps, announce liveness — which is exactly what a
        // reconnection wants. Must run before the clock spawns (step 1) so
        // the first tick judges the fresh anchor.
        self.core.state.lock().await.last_peer_activity_ms = now_ms();
        // 1. Clock first: ACK batches / NACK / retransmissions / heartbeat /
        //    silent-peer detection, self-driven (unwire aborts the task).
        let clock = tokio::spawn(run_clock(self.clone(), Arc::clone(&transport)));
        *self.core.clock.write().expect("poisoned clock lock") = Some(clock);
        // 2. Inbound hooks: the transport's reader routes `DATA_BINARY` /
        //    control frames here (held weakly — no session↔transport cycle).
        transport.on_transfer_activity(Some(build_activity_handlers(
            Arc::downgrade(&self.core),
            Arc::downgrade(&transport),
        )));
        // 3. Store the wiring: write surface + silent-timeout disconnect.
        *self.core.wiring.write().expect("poisoned wiring lock") = Some(transport);
        // 4. Auto-RESUME (§5): declare this side's incomplete receive
        //    transactions to the peer holder (through the write surface —
        //    requires the live connection `wire` attaches to).
        let transport = self
            .core
            .wiring
            .read()
            .expect("poisoned wiring lock")
            .clone();
        if let Some(transport) = transport {
            send_resume_requests(self, transport.as_ref()).await;
        }
        Ok(())
    }

    /// Detach from the current connection: hooks unregistered, clock
    /// aborted and awaited (no zombie tick), wiring dropped. Transaction
    /// state stays with the session. Idempotent.
    pub(crate) async fn unwire(&self) {
        self.detach_connection();
        let handle = self.core.clock.write().expect("poisoned clock lock").take();
        if let Some(handle) = handle {
            handle.abort();
            let _ = handle.await;
        }
    }

    /// Drop the registration/write side of the wiring without stopping the
    /// clock: used by the clock task itself when the connection is known
    /// dead (the task exits right after, so its handle needs no abort).
    fn detach_connection(&self) {
        let transport = self
            .core
            .wiring
            .read()
            .expect("poisoned wiring lock")
            .clone();
        if let Some(transport) = transport {
            transport.on_transfer_activity(None);
        }
        *self.core.wiring.write().expect("poisoned wiring lock") = None;
    }
}

/// Build the inbound hook pair for a wired transport. Both closures hold
/// the session core (and the transport) only weakly: the transport out-
/// living the session must not keep it alive, and the session holding its
/// transport strongly must not be kept alive by the transport's hooks.
fn build_activity_handlers(
    core: std::sync::Weak<SessionCore>,
    transport: std::sync::Weak<dyn XacppTransport>,
) -> TransferActivityHandlers {
    let on_activity_core = core.clone();
    TransferActivityHandlers {
        on_activity: Arc::new(move || {
            let core = on_activity_core.clone();
            Box::pin(async move {
                if let Some(core) = core.upgrade() {
                    core.state.lock().await.last_peer_activity_ms = now_ms();
                }
            }) as ActivityFuture
        }),
        on_frame: Arc::new(move |frame: Frame| {
            let core = core.clone();
            let transport = transport.clone();
            Box::pin(async move {
                let (Some(core), Some(transport)) = (core.upgrade(), transport.upgrade()) else {
                    // Session or transport already gone: nothing to route to.
                    return;
                };
                handle_inbound(&TransferSession { core }, frame, transport.as_ref()).await;
            }) as ActivityFuture
        }),
    }
}

/// Wall-clock milliseconds (integration layer owns real time; the pure
/// components stay time-injected).
pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

/// Assemble a control wire frame. Header identity fields carry no meaning
/// for control payloads: zeroed transaction id, `total_len = 0`, `seq = 0`.
pub(crate) fn control_frame(
    frame_type: FrameType,
    payload: Vec<u8>,
) -> Result<Vec<u8>, XacppError> {
    Frame::new(frame_type, [0u8; 16], 0, 0, payload)
        .encode()
        .map_err(|e| XacppError::Internal(e.to_string()))
}

/// Send-side state machine behind [`XacppTransport::send_binary`].
///
/// Backpressure form: never blocks. At the cap it fails immediately with
/// `backpressure_stall` (§5: senders pause DATA frames while over the cap;
/// the caller retries — recovery is automatic as ACKs release copies).
///
/// `on_complete` fires exactly once when the transaction reaches a terminal
/// state: `confirmed: true` on the final ACK; `confirmed: false` when the
/// send copy is released without confirmation (the holder rejects the
/// RESUME request, or the copy retention expires). Failure paths notify
/// too — a callback never fires zero times.
pub(crate) async fn send_binary(
    session: &TransferSession,
    wiring: &dyn TransferWiring,
    payload: Vec<u8>,
    on_complete: Option<CompletionCallback>,
) -> Result<[u8; 16], XacppError> {
    let mut st = session.core.state.lock().await;
    let now = now_ms();
    if st.sender.is_paused() {
        return Err(backpressure_error(&st));
    }
    let txid = uuid::Uuid::new_v4().into_bytes();
    let frames = st
        .sender
        .begin_transaction(txid, &payload, now)
        .map_err(|e| match e {
            BeginError::Backpressured { .. } => backpressure_error(&st),
            other => XacppError::Internal(other.to_string()),
        })?;
    if let Some(cb) = on_complete {
        st.completions.insert(txid, cb);
    }
    drop(st);
    for frame in frames {
        wiring.write_data_frame(frame)?;
    }
    Ok(txid)
}

fn backpressure_error(st: &SessionState) -> XacppError {
    XacppError::Application {
        code: "backpressure_stall".into(),
        message: format!(
            "unacknowledged bytes {} at cap {}: retry after ACKs release copies",
            st.sender.unacked_bytes(),
            st.sender.config().backpressure_cap_bytes,
        ),
    }
}

/// On (re)connect: emit RESUME requests for this side's incomplete receive
/// transactions (§5: "a receiver sends RESUME requests for each incomplete
/// transaction before accepting new data flows").
///
/// Transactions without a contiguous frame-0 prefix export no ACK entry
/// (§5: a cumulative `highest_contiguous_seq` cannot express "no prefix")
/// and therefore cannot be declared here; they expire through the stale
/// sweep instead.
pub(crate) async fn send_resume_requests(session: &TransferSession, wiring: &dyn TransferWiring) {
    let st = session.core.state.lock().await;
    let entries: Vec<AckEntry> = st
        .reassembler
        .open_transactions()
        .filter_map(|id| st.reassembler.ack_entry(id))
        .collect();
    if entries.is_empty() {
        return;
    }
    let bytes = ResumeRequestPayload { entries }.encode();
    if let Ok(bytes) = bytes {
        match control_frame(FrameType::Resume, bytes) {
            Ok(wire) => {
                if let Err(e) = wiring.write_control_frame(wire) {
                    log::warn!("transfer: failed to send RESUME request: {e}");
                }
            }
            Err(e) => log::warn!("transfer: failed to encode RESUME request: {e}"),
        }
    }
}

/// Inbound frame dispatch for transfer-wired transports. Every frame
/// refreshes the peer-activity clock. Single-component errors (hostile or
/// corrupted frames) are logged and dropped: reassembly slots and copies are
/// isolated per transaction, the connection stays up.
pub(crate) async fn handle_inbound(
    session: &TransferSession,
    frame: Frame,
    wiring: &dyn TransferWiring,
) {
    log::debug!("transfer: inbound frame {:?}", frame.frame_type);
    let now = now_ms();
    // One handler-presence read per inbound batch: completed payloads are
    // taken for immediate delivery only while a binary handler is
    // registered; otherwise the completed slot is retained (final ACK is
    // unaffected) until the clock path delivers it after registration.
    let handler_present = session
        .core
        .binary_handler
        .read()
        .expect("poisoned handler lock")
        .is_some();
    let mut st = session.core.state.lock().await;
    st.last_peer_activity_ms = now;
    let mut delivered: Vec<([u8; 16], Vec<u8>)> = Vec::new();
    let mut completions: Vec<(CompletionCallback, BinarySendResult)> = Vec::new();

    match frame.frame_type {
        // The envelope channel never reaches the transfer layer (the
        // reader forwards `DATA_JSON` payloads to the envelope dispatch
        // before this dispatch point); defensive no-op.
        FrameType::DataJson => {}
        FrameType::DataBinary => {
            on_data_frame(&mut st, frame, wiring, &mut delivered, handler_present, now)
        }
        FrameType::Ack => on_ack_frame(&mut st, &frame, &mut completions, now),
        FrameType::Nack => on_nack_frame(&mut st, &frame, wiring, now),
        // Two directions, two frame types (§5): dispatch on frame_type,
        // never on payload shape.
        FrameType::Resume => on_resume_request_frame(&mut st, &frame, wiring, now),
        FrameType::ResumeResponse => on_resume_response_frame(&mut st, &frame, &mut completions),
        FrameType::Heartbeat => {
            // Liveness already recorded above; validate the payload so
            // malformed heartbeats are surfaced.
            if let Err(e) = HeartbeatPayload::decode(&frame.payload) {
                log::warn!("transfer: malformed HEARTBEAT: {e}");
            }
        }
        FrameType::Error => on_error_frame(&mut st, &frame),
        FrameType::Negotiate | FrameType::SecurityUpgrade | FrameType::Unknown(_) => {
            log::debug!("transfer: ignoring frame type {:?}", frame.frame_type);
        }
    }

    drop(st);
    // Deliver outside the state lock: callbacks may re-enter the session.
    for (txid, payload) in delivered {
        session.deliver(txid, payload);
    }
    for (cb, result) in completions {
        cb(result);
    }
}

/// DATA_BINARY frame → reassembler. On completion: immediate final ACK
/// (§5: "a final ACK on completion") regardless of handler presence; the
/// payload is taken for delivery only while a binary handler is registered
/// — without one the completed slot is retained until the handler arrives
/// (the clock path delivers it then, exactly once).
fn on_data_frame(
    st: &mut SessionState,
    frame: Frame,
    wiring: &dyn TransferWiring,
    delivered: &mut Vec<([u8; 16], Vec<u8>)>,
    handler_present: bool,
    now: u64,
) {
    match st.reassembler.receive_frame(
        frame.transaction_id,
        frame.seq,
        frame.total_len,
        frame.payload.len(),
        &frame.payload,
        now,
    ) {
        Ok(ReceiveOutcome::Accepted { complete }) => {
            *st.pending_ack_frames
                .entry(frame.transaction_id)
                .or_insert(0) += 1;
            if complete {
                if let Some(entry) = st.reassembler.ack_entry(frame.transaction_id) {
                    send_ack_entries(wiring, [entry]);
                }
                st.pending_ack_frames.remove(&frame.transaction_id);
                if handler_present
                    && let Some(payload) = st.reassembler.take_completed(frame.transaction_id)
                {
                    delivered.push((frame.transaction_id, payload));
                }
            }
        }
        // Retransmission already stored: idempotent.
        Ok(ReceiveOutcome::Duplicate) => {}
        Err(e) => {
            log::warn!(
                "transfer: rejected DATA_BINARY frame (tx {:?} seq {}): {e}",
                uuid::Uuid::from_bytes(frame.transaction_id),
                frame.seq
            );
        }
    }
}

/// Encode and send an ACK control frame (control-priority channel).
fn send_ack_entries(wiring: &dyn TransferWiring, entries: impl IntoIterator<Item = AckEntry>) {
    let entries: Vec<AckEntry> = entries.into_iter().collect();
    if entries.is_empty() {
        return;
    }
    let bytes = AckPayload { entries }.encode();
    match bytes {
        Ok(bytes) => match control_frame(FrameType::Ack, bytes) {
            Ok(wire) => {
                if let Err(e) = wiring.write_control_frame(wire) {
                    log::warn!("transfer: failed to send ACK: {e}");
                }
            }
            Err(e) => log::warn!("transfer: failed to encode ACK: {e}"),
        },
        Err(e) => log::warn!("transfer: failed to encode ACK: {e}"),
    }
}

fn on_ack_frame(
    st: &mut SessionState,
    frame: &Frame,
    completions: &mut Vec<(CompletionCallback, BinarySendResult)>,
    now: u64,
) {
    let payload = match AckPayload::decode(&frame.payload) {
        Ok(payload) => payload,
        Err(e) => {
            log::warn!("transfer: malformed ACK: {e}");
            return;
        }
    };
    for entry in &payload.entries {
        // Final ACK: copy released, fire the send_binary completion as
        // confirmed delivery.
        if st.sender.handle_ack(entry, now)
            && let Some(cb) = st.completions.remove(&entry.transaction_id)
        {
            completions.push((
                cb,
                BinarySendResult {
                    transaction_id: entry.transaction_id,
                    confirmed: true,
                },
            ));
        }
    }
}

fn on_nack_frame(st: &mut SessionState, frame: &Frame, wiring: &dyn TransferWiring, now: u64) {
    let payload = match NackPayload::decode(&frame.payload) {
        Ok(payload) => payload,
        Err(e) => {
            log::warn!("transfer: malformed NACK: {e}");
            return;
        }
    };
    for entry in &payload.entries {
        // §5: "a NACK triggers immediate retransmission" (data channel —
        // these are DATA_BINARY frames).
        for wire_frame in st
            .sender
            .handle_nack(entry.transaction_id, &entry.missing_seqs, now)
        {
            if let Err(e) = wiring.write_data_frame(wire_frame) {
                log::warn!("transfer: failed to send NACK retransmission: {e}");
                return;
            }
        }
    }
}

/// RESUME request (`0x0004`, receiver → holder): answer as the copy
/// holder. Malformed payloads are logged and dropped — never guessed at.
/// A zero-entry request ("nothing to resume") is **not** answered, which
/// prevents empty-frame ping-pong between two sides with nothing in
/// flight.
fn on_resume_request_frame(
    st: &mut SessionState,
    frame: &Frame,
    wiring: &dyn TransferWiring,
    now: u64,
) {
    match ResumeRequestPayload::decode(&frame.payload) {
        Ok(req) => answer_resume_request(st, req, wiring, now),
        Err(e) => log::error!("transfer: malformed RESUME request dropped: {e}"),
    }
}

/// RESUME response (`0x0008`, holder → receiver): apply the statuses for
/// transactions we requested progress on.
fn on_resume_response_frame(
    st: &mut SessionState,
    frame: &Frame,
    completions: &mut Vec<(CompletionCallback, BinarySendResult)>,
) {
    match ResumeResponsePayload::decode(&frame.payload) {
        Ok(resp) => {
            for entry in &resp.entries {
                match entry.status {
                    // Holder's copy is gone: drop our send-side copy and our
                    // receive-side slot (nothing more will arrive for this
                    // transaction). A pending send completion is released as
                    // unconfirmed — the terminal state is reached, the
                    // caller must hear about it.
                    ResumeStatus::Rejected => {
                        st.sender
                            .handle_resume_response(std::slice::from_ref(entry));
                        st.reassembler.discard(entry.transaction_id);
                        if let Some(cb) = st.completions.remove(&entry.transaction_id) {
                            completions.push((
                                cb,
                                BinarySendResult {
                                    transaction_id: entry.transaction_id,
                                    confirmed: false,
                                },
                            ));
                        }
                    }
                    // Retransmission is already on its way.
                    ResumeStatus::Resumed => {}
                    // Holder never knew the transaction: keep local state
                    // until the stale/expiry sweeps decide.
                    ResumeStatus::Unknown => {}
                }
            }
        }
        Err(e) => log::error!("transfer: malformed RESUME_RESPONSE dropped: {e}"),
    }
}

fn answer_resume_request(
    st: &mut SessionState,
    req: ResumeRequestPayload,
    wiring: &dyn TransferWiring,
    now: u64,
) {
    if req.entries.is_empty() {
        // Nothing to resume: not answered (see `on_resume_frame`).
        return;
    }
    let answers = st.sender.handle_resume_request(&req.entries, now);
    // Retransmissions first (data channel), then the status answer
    // (control channel) — the receiver can act on the statuses as soon as
    // the missing frames are queued behind them on the wire.
    for wire_frame in &answers.retransmit {
        if let Err(e) = wiring.write_data_frame(wire_frame.clone()) {
            log::warn!("transfer: failed to send RESUME retransmission: {e}");
            return;
        }
    }
    let response = ResumeResponsePayload {
        entries: answers.statuses,
    };
    match response.encode() {
        Ok(bytes) => match control_frame(FrameType::ResumeResponse, bytes) {
            Ok(wire) => {
                if let Err(e) = wiring.write_control_frame(wire) {
                    log::warn!("transfer: failed to send RESUME response: {e}");
                }
            }
            Err(e) => log::warn!("transfer: failed to encode RESUME response: {e}"),
        },
        Err(e) => log::warn!("transfer: failed to encode RESUME response: {e}"),
    }
}

fn on_error_frame(st: &mut SessionState, frame: &Frame) {
    let payload = match ErrorPayload::decode(&frame.payload) {
        Ok(payload) => payload,
        Err(e) => {
            log::warn!("transfer: malformed ERROR: {e}");
            return;
        }
    };
    log::warn!(
        "transfer: peer ERROR 0x{:04x}: {}",
        payload.code,
        payload.message
    );
    if payload.code == error_codes::TRANSACTION_EXPIRED {
        // SPEC §5: for `0x0002` the message field starts with the affected
        // transaction id as 32 lowercase hex chars (the remainder is
        // free-form, not parsed). Our receive slot will never be satisfied
        // — drop it now instead of waiting out the stale sweep.
        if let Some(id) = parse_hex_txid_prefix(&payload.message) {
            st.reassembler.discard(*id.as_bytes());
        } else {
            log::warn!(
                "transfer: transaction_expired without a 32-hex id prefix; cannot discard a slot"
            );
        }
    }
}

/// Parse the structured `0x0002` prefix: 32 lowercase hex chars at the head
/// of the message; anything after is ignored.
fn parse_hex_txid_prefix(message: &str) -> Option<uuid::Uuid> {
    let prefix = message.get(..32)?;
    let is_hex = prefix
        .chars()
        .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c));
    if !is_hex {
        return None;
    }
    uuid::Uuid::parse_str(prefix).ok()
}

/// The session's self-driven clock task: started at wire, aborted at
/// unwire, exits when the connection closes (watch) or when a tick
/// declares the peer silent (then tears the connection down through the
/// transport's own `disconnect`).
async fn run_clock(session: TransferSession, transport: Arc<dyn XacppTransport>) {
    let tick_ms = session.core.config.tick_interval_ms.max(1);
    let mut interval = tokio::time::interval(std::time::Duration::from_millis(tick_ms));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut closed = transport.closed();
    loop {
        tokio::select! {
            _ = interval.tick() => {}
            // Connection dead (peer disconnect / write failure / local
            // disconnect): drop the wiring side, then exit — the session's
            // transaction state survives for the next wire.
            _ = closed.changed() => {
                session.detach_connection();
                break;
            }
        }
        let now = now_ms();
        let (silent, completions) = clock_tick(&session, transport.as_ref(), now).await;
        // Fire outside the state lock, before any teardown: a dead
        // connection still owes its callers the unconfirmed releases.
        for (cb, result) in completions {
            cb(result);
        }
        if silent {
            // Silent peer: self-driven dead-connection path — detach
            // (hooks off, wiring dropped), then tear the connection down
            // through the transport's `disconnect`.
            log::info!(
                "transfer: silent timeout after {} ms without peer frames",
                session.core.config.silent_timeout_ms
            );
            session.detach_connection();
            if let Err(e) = transport.disconnect().await {
                log::debug!("transfer: silent-timeout disconnect: {e}");
            }
            break;
        }
        // Deferred delivery: payloads completed before a binary handler was
        // registered are handed over here (take-on-drain → exactly once).
        // Retained payloads survive disconnections with the session and are
        // delivered once the clock runs again.
        for (txid, payload) in drain_completed(&session).await {
            session.deliver(txid, payload);
        }
    }
}

/// Collect completed-but-undelivered payloads while a binary handler is
/// registered: the handler-presence check happens outside the state lock,
/// the take inside it; delivery happens at the call site, outside both
/// locks (handlers may re-enter the session). [`TransactionReassembler::
/// take_completed`] empties each slot exactly once, so repeated ticks
/// never redeliver.
async fn drain_completed(session: &TransferSession) -> Vec<([u8; 16], Vec<u8>)> {
    if session
        .core
        .binary_handler
        .read()
        .expect("poisoned handler lock")
        .is_none()
    {
        return Vec::new();
    }
    let mut st = session.core.state.lock().await;
    let completed: Vec<[u8; 16]> = st
        .reassembler
        .open_transactions()
        .filter(|id| st.reassembler.is_complete(*id))
        .collect();
    completed
        .into_iter()
        .filter_map(|id| st.reassembler.take_completed(id).map(|p| (id, p)))
        .collect()
}

/// One clock tick: sweeps, batched ACK export, NACK feedback,
/// retransmissions, heartbeat, silent-peer check. Returns the silent-peer
/// verdict plus send completions released by this tick (retention expiry):
/// the caller fires them outside the state lock (callbacks may re-enter
/// the session).
async fn clock_tick(
    session: &TransferSession,
    wiring: &dyn TransferWiring,
    now: u64,
) -> (bool, Vec<(CompletionCallback, BinarySendResult)>) {
    let cfg = &session.core.config;
    let mut st = session.core.state.lock().await;
    let mut released: Vec<(CompletionCallback, BinarySendResult)> = Vec::new();

    // 1. Retention sweeps. Tombstoned send copies notify the peer (§5:
    //    "the integration layer may notify peers with ERROR
    //    transaction_expired"); receive-side stales just expire locally.
    //    The message field carries the transaction id as 32 lowercase hex
    //    chars (SPEC §5 structured prefix). An expired copy is also a
    //    terminal state for its `send_binary` completion: released as
    //    unconfirmed (collected here, fired by the caller outside the
    //    lock).
    let _stale = st.reassembler.expire_stale(now);
    for id in st.sender.expire_copies(now) {
        if let Some(cb) = st.completions.remove(&id) {
            released.push((
                cb,
                BinarySendResult {
                    transaction_id: id,
                    confirmed: false,
                },
            ));
        }
        let payload = ErrorPayload {
            code: error_codes::TRANSACTION_EXPIRED,
            message: uuid::Uuid::from_bytes(id).simple().to_string(),
        };
        match payload.encode() {
            Ok(bytes) => match control_frame(FrameType::Error, bytes) {
                Ok(wire) => {
                    if let Err(e) = wiring.write_control_frame(wire) {
                        log::warn!("transfer: failed to send transaction_expired: {e}");
                    }
                }
                Err(e) => log::warn!("transfer: failed to encode ERROR: {e}"),
            },
            Err(e) => log::warn!("transfer: failed to encode ERROR: {e}"),
        }
    }

    // 2. Batched ACK export (§5: incremental ACKs every `ack_batch_frames`
    //    frames or `ack_batch_interval_ms`, whichever first). Transactions
    //    without a contiguous prefix export no entry — their accounting
    //    stays for the next round (final ACK / NACK cover them).
    let pending_total: u32 = st.pending_ack_frames.values().sum();
    if pending_total >= cfg.ack_batch_frames
        || (pending_total > 0 && now.saturating_sub(st.last_ack_ms) >= cfg.ack_batch_interval_ms)
    {
        let entries: Vec<AckEntry> = st
            .pending_ack_frames
            .keys()
            .copied()
            .filter_map(|id| st.reassembler.ack_entry(id))
            .collect();
        for id in st
            .pending_ack_frames
            .keys()
            .copied()
            .filter(|id| st.reassembler.ack_entry(*id).is_some())
            .collect::<Vec<_>>()
        {
            st.pending_ack_frames.remove(&id);
        }
        st.last_ack_ms = now;
        send_ack_entries(wiring, entries);
    }

    // 3. Receiver NACK feedback (§5: "Receivers ... return ACK (receipt) /
    //    NACK (gap list) feedback"). One frame per gapped transaction — a
    //    single entry's gap list is bounded by MAX_NACK_MISSING_ENTRIES so
    //    each frame fits the control cap.
    if now.saturating_sub(st.last_nack_ms) >= cfg.nack_feedback_interval_ms {
        st.last_nack_ms = now;
        for id in st.reassembler.open_transactions().collect::<Vec<_>>() {
            let missing = st
                .reassembler
                .missing_seqs(id, crate::reassemble::MAX_NACK_MISSING_ENTRIES);
            if missing.is_empty() {
                continue;
            }
            let payload = NackPayload {
                entries: vec![NackEntry {
                    transaction_id: id,
                    missing_seqs: missing,
                }],
            };
            match payload.encode() {
                Ok(bytes) => match control_frame(FrameType::Nack, bytes) {
                    Ok(wire) => {
                        if let Err(e) = wiring.write_control_frame(wire) {
                            log::warn!("transfer: failed to send NACK: {e}");
                        }
                    }
                    Err(e) => log::warn!("transfer: failed to encode NACK: {e}"),
                },
                Err(e) => log::warn!("transfer: failed to encode NACK: {e}"),
            }
        }
    }

    // 4. Retransmissions (in-flight frames past their RTO). Exempt from the
    //    backpressure pause: the cap itself bounds retransmission storms.
    for wire_frame in st.sender.due_retransmissions(now) {
        if let Err(e) = wiring.write_data_frame(wire_frame) {
            log::warn!("transfer: failed to send retransmission: {e}");
            return (true, released);
        }
    }

    // 5. Heartbeat (§5 liveness; `u64::MAX` interval disables).
    if now.saturating_sub(st.last_heartbeat_ms) >= cfg.heartbeat_interval_ms {
        st.last_heartbeat_ms = now;
        st.heartbeat_seq = st.heartbeat_seq.wrapping_add(1);
        let payload = HeartbeatPayload {
            monotonic_seq: st.heartbeat_seq,
            sender_epoch_ms: now,
        };
        match payload.encode() {
            Ok(bytes) => match control_frame(FrameType::Heartbeat, bytes) {
                Ok(wire) => {
                    if let Err(e) = wiring.write_control_frame(wire) {
                        log::warn!("transfer: failed to send heartbeat: {e}");
                    }
                }
                Err(e) => log::warn!("transfer: failed to encode heartbeat: {e}"),
            },
            Err(e) => log::warn!("transfer: failed to encode heartbeat: {e}"),
        }
    }

    // 6. Silent peer → dead connection.
    (
        now.saturating_sub(st.last_peer_activity_ms) >= cfg.silent_timeout_ms,
        released,
    )
}
