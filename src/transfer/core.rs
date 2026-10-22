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
//! answers per transaction and retransmits, completing the transfer across
//! the connection boundary. Breakpoint resume is the natural consequence
//! of session-owned state, not a separate binding ceremony. Receive-side
//! streams are *session-bound* (docs/streams.md): a disconnect stalls
//! them, only completion, cancellation, or session death closes one.
//!
//! ## Clock driving
//!
//! The pure components take injected time; the session owns the clock.
//! While wired, a periodic tick task drives: batched ACK export (§5: every
//! `ack_batch_frames` frames or `ack_batch_interval_ms`), receiver NACK
//! feedback for gap lists, sender retransmissions, heartbeats, silent-peer
//! detection (dead-connection path: the session calls the transport's
//! `disconnect` itself), and deferred delivery of payloads that completed
//! before a binary handler was registered (see
//! [`TransferSession::on_binary`]). There is no time-based expiry: copy
//! lifetime equals transaction lifetime (SPEC §5 revised).
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

use crate::carrier::XacppTransport;
use crate::reliability::reassemble::TransactionReassembler;
use crate::reliability::transaction::{BeginError, TransactionSender};
use crate::wire::error::XacppError;
use crate::wire::frame::{Frame, FrameType};

use super::clock::{run_clock, send_resume_declaration};
use super::inbound::handle_inbound;
use super::sink::backpressure_error;

/// Terminal outcome of a `send_binary` transaction, delivered to its
/// completion callback exactly once (mirrors the TS implementation's
/// `BinarySendResult`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BinarySendResult {
    /// Transaction the result belongs to.
    pub transaction_id: [u8; 16],
    /// `true`: the final ACK confirmed delivery (send copy released).
    /// `false`: the copy was released without confirmation (the receiver's
    /// resume verdict failed the transaction, or the caller aborted).
    pub confirmed: bool,
}

/// Callback invoked when a `send_binary` transaction reaches a terminal
/// state: confirmed delivery (final ACK received) or unconfirmed copy
/// release (resume verdict `unknown`/`rejected`, or caller abort). Fired
/// exactly once per
/// transaction — the type says so: `FnOnce`, taken out of the completions
/// table by the firing path — always outside the state lock. Must return
/// quickly; long work is the callback's own to spawn.
pub type CompletionCallback = Box<dyn FnOnce(BinarySendResult) + Send>;

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
#[async_trait::async_trait]
pub trait TransferWiring: Send + Sync {
    /// Write one encoded control frame (egress scheduling priority, §5
    /// invariant 4). `Err` = connection unavailable.
    async fn write_control_frame(&self, bytes: Vec<u8>) -> Result<(), XacppError>;

    /// Write one data frame, encoded (plain FIFO). `Err` = connection
    /// unavailable.
    async fn write_data_frame(&self, frame: Frame) -> Result<(), XacppError>;

    /// Register the connection-activity / inbound-transfer-frame handlers
    /// (`None` unregisters). Registering replaces any previous handlers.
    async fn on_transfer_activity(&self, handlers: Option<TransferActivityHandlers>);
}

/// Tunables for the transfer integration (component defaults per SPEC §5
/// RECOMMENDED values; transport-level knobs added on top).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransferConfig {
    /// Send-side component config.
    pub sender: crate::reliability::transaction::SenderConfig,
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
    /// RESUME declaration verdict timeout (§5 recovery discipline): a
    /// declaration that stays unanswered past this timeout is
    /// retransmitted — the handshake is the sole recovery entry, so its
    /// own reliability is mandatory.
    pub resume_verdict_timeout_ms: u64,
    /// Clock tick granularity (internal scheduling, not a SPEC value).
    pub tick_interval_ms: u64,
}

impl Default for TransferConfig {
    fn default() -> Self {
        Self {
            sender: Default::default(),
            heartbeat_interval_ms: 30_000,
            silent_timeout_ms: 90_000,
            ack_batch_frames: 64,
            ack_batch_interval_ms: 500,
            nack_feedback_interval_ms: 500,
            resume_verdict_timeout_ms: 2_000,
            tick_interval_ms: 100,
        }
    }
}

/// Shared session core: consumer-held, lives across connections.
pub(super) struct SessionCore {
    pub(super) config: TransferConfig,
    pub(super) state: tokio::sync::Mutex<SessionState>,
    // Handler registration: write-rarely (register/replace), read-per-frame.
    // A watch channel's sender doubles as a lock-free synchronous read
    // surface (`borrow` — latest value wins, exactly the registration
    // semantics), so no lock appears on either side.
    pub(super) binary_handler: tokio::sync::RwLock<Option<BinaryHandler>>,
    // Same pattern as `binary_handler` (docs/streams.md: the
    // first-frame callback fires from the inbound dispatch path).
    pub(super) stream_handler:
        tokio::sync::RwLock<Option<crate::reliability::stream::StreamHandler>>,
    /// Backpressure wake surface: parked `BinarySink::write` loops await a
    /// change of this watch; ACK-driven copy releases (and aborts / resume)
    /// bump it. Watch semantics — a bump between a parked writer's cap
    /// check and its wait registration is never lost.
    pub(super) backpressure_watch: tokio::sync::watch::Sender<u64>,
    /// Wired transport (present while wired): write surface plus the
    /// `disconnect` handle for the silent-timeout path. Strong here — the
    /// transport's hooks and the clock task hold the session only weakly,
    /// so no reference cycle forms (the session is the long-lived side).
    /// Same watch pattern as the handlers: wire/unwire replace, the
    /// per-frame read path borrows without a lock.
    pub(super) wiring: tokio::sync::RwLock<Option<Arc<dyn XacppTransport>>>,
    /// Clock task handle (present while wired), aborted on unwire.
    pub(super) clock: tokio::sync::RwLock<Option<tokio::task::JoinHandle<()>>>,
}

/// Receive-side streaming transactions (first frame opened a stream):
/// pause/abort control frames and completion resolve these. The entry owns
/// the chunk-channel sender — removing/dropping the entry closes the
/// consumer's channel by ownership (no lock, safe in `Drop`).
pub(super) struct StreamEntry {
    pub(super) inner: Arc<crate::reliability::stream::StreamInner>,
    pub(super) chunk_tx: crate::reliability::stream::ChunkTx,
}

/// Mutable session state — components plus clock bookkeeping.
pub(super) struct SessionState {
    pub(super) sender: TransactionSender,
    pub(super) reassembler: TransactionReassembler,
    /// `send_binary` completion callbacks, fired exactly once with the
    /// outcome (confirmed delivery or unconfirmed release).
    pub(super) completions: HashMap<[u8; 16], CompletionCallback>,
    /// Receive-side streaming transactions (first frame opened a stream):
    /// pause/abort control frames and completion resolve these.
    pub(super) streams: HashMap<[u8; 16], StreamEntry>,
    /// Transactions whose first frame was delivered to a stream handler —
    /// they belong to the stream surface (the aggregate `on_binary` surface
    /// never sees them, avoiding double delivery).
    pub(super) stream_owned: std::collections::HashSet<[u8; 16]>,
    /// Newly received frames per transaction since the last ACK export.
    pub(super) pending_ack_frames: HashMap<[u8; 16], u32>,
    pub(super) last_ack_ms: u64,
    pub(super) last_nack_ms: u64,
    pub(super) heartbeat_seq: u32,
    pub(super) last_heartbeat_ms: u64,
    /// Last instant any inbound frame arrived from the peer (all frames
    /// refresh it — liveness is proven by traffic, heartbeats make it
    /// explicit on quiet links).
    pub(super) last_peer_activity_ms: u64,
}

/// Session death cancels every non-terminal stream (docs/streams.md:
/// streams live with the session — "session 不死，流不灭"; the flip side is
/// that the session's death is the one remaining cancel source besides
/// completion and explicit cancellation). Runs when the last
/// `TransferSession` handle (and with it the tokio mutex guarding this
/// state) is dropped. Lock-free: `finish` is a pure atomic store and the
/// chunk channel closes by ownership when the drained entry drops.
impl Drop for SessionState {
    fn drop(&mut self) {
        for (_, entry) in self.streams.drain() {
            entry
                .inner
                .finish(crate::reliability::stream::StreamState::Cancelled);
        }
        self.stream_owned.clear();
    }
}

/// Consumer-held transfer session: reassembler + sender + config, bound
/// optionally into a transport and living across connections (RESUME
/// anchor). Cloneable handle over shared state.
#[derive(Clone)]
pub struct TransferSession {
    pub(super) core: Arc<SessionCore>,
}

impl TransferSession {
    /// Create a session with the given configuration.
    pub fn new(config: TransferConfig) -> Self {
        let now = now_ms();
        Self {
            core: Arc::new(SessionCore {
                state: tokio::sync::Mutex::new(SessionState {
                    sender: TransactionSender::new(config.sender),
                    reassembler: TransactionReassembler::new(),
                    completions: HashMap::new(),
                    streams: HashMap::new(),
                    stream_owned: std::collections::HashSet::new(),
                    pending_ack_frames: HashMap::new(),
                    last_ack_ms: now,
                    last_nack_ms: now,
                    heartbeat_seq: 0,
                    last_heartbeat_ms: now,
                    last_peer_activity_ms: now,
                }),
                binary_handler: tokio::sync::RwLock::new(None),
                stream_handler: tokio::sync::RwLock::new(None),
                backpressure_watch: tokio::sync::watch::channel(0u64).0,
                wiring: tokio::sync::RwLock::new(None),
                clock: tokio::sync::RwLock::new(None),
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
    pub async fn on_binary(&self, handler: BinaryHandler) {
        *self.core.binary_handler.write().await = Some(handler);
    }

    /// Register the receive-side stream handler (docs/streams.md): the
    /// first frame of each new transaction builds a [`super::stream::
    /// BinaryStream`] and hands it to the handler; chunks flow in `seq`
    /// order as the contiguous prefix advances. Single handler — a later
    /// registration replaces the earlier one. Transactions that completed
    /// before registration keep the aggregate `on_binary` semantics
    /// (retained and delivered exactly once once a handler is present).
    ///
    /// While a stream handler is registered, new transactions belong to the
    /// stream surface: the aggregate `on_binary` handler no longer receives
    /// them (no double delivery). Consumers pick one of the two surfaces
    /// per stream.
    pub async fn on_binary_stream(&self, handler: crate::reliability::stream::StreamHandler) {
        *self.core.stream_handler.write().await = Some(handler);
    }

    pub(super) async fn deliver(&self, txid: [u8; 16], payload: Vec<u8>) {
        let handler = self.core.binary_handler.read().await.clone();
        if let Some(handler) = handler {
            handler(txid, payload);
        }
    }

    /// Whether the transfer mount is currently attached to a transport
    /// (the wiring slot is occupied). Read-only judgement for the
    /// session's attach/detach idempotence.
    pub(crate) async fn is_wired(&self) -> bool {
        self.core.wiring.read().await.is_some()
    }

    /// Wire the session to a **live** transport: starts the self-driven
    /// clock, registers the inbound hooks, and emits `RESUME` for every
    /// incomplete receive (§5) through the write surface — the TS
    /// attach-to-a-live-connection semantics.
    pub(crate) async fn wire(&self, transport: Arc<dyn XacppTransport>) -> Result<(), XacppError> {
        if self.core.wiring.read().await.is_some() {
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
        *self.core.clock.write().await = Some(clock);
        // 2. Inbound hooks: the transport's reader routes `DATA_BINARY` /
        //    control frames here (held weakly — no session↔transport cycle).
        transport
            .on_transfer_activity(Some(build_activity_handlers(
                Arc::downgrade(&self.core),
                Arc::downgrade(&transport),
            )))
            .await;
        // 3. Store the wiring: write surface + silent-timeout disconnect.
        *self.core.wiring.write().await = Some(transport);
        // 4. Recovery discipline: arm the reconnect gate on every pending
        //    send — they retransmit nothing until the receiver's per-
        //    transaction verdict arrives (Resumed = continue, Unknown =
        //    fail out). The RESUME declaration frame itself is NOT sent
        //    here: at wire-up the peer's transfer plane may not exist yet
        //    (the initiator wires at connect, BEFORE the responder builds
        //    its session inside on_establish), and a dropped declaration
        //    deadlocks the holder awaiting a verdict that never arrives.
        //    Sending happens in `declare_pending_sends`, fired by the peer
        //    once the establish handshake completes (or by a consumer-
        //    driven `XacppSession::wire`, which by contract runs when the
        //    peer is already wired — the responder's on_establish case).
        {
            let mut st = self.core.state.lock().await;
            st.sender.declare_pending(now_ms());
        }
        // 5. Re-wire recovery (docs/streams.md): clear the flow stall on
        //    every surviving stream (back to its Active/Paused state — the
        //    pause intent survived the disconnect) and wake parked
        //    writers/readers: sink writes park across disconnects and drain
        //    now that the wiring is back.
        {
            let st = self.core.state.lock().await;
            for entry in st.streams.values() {
                entry.inner.clear_stalled();
            }
        }
        self.wake_writers();
        Ok(())
    }

    /// Recovery discipline: send the RESUME declaration for every pending
    /// send (declare_pending is idempotent — re-declaring re-arms the same
    /// gate). Fired by the peer when the establish handshake completes (the
    /// only moment BOTH transfer planes are live on an establish topology),
    /// or by a consumer-driven `XacppSession::wire` — which by contract
    /// happens when the peer is already wired (responder's on_establish).
    pub(crate) async fn declare_pending_sends(&self) {
        let transport = self.core.wiring.read().await.clone();
        if let Some(transport) = transport {
            send_resume_declaration(self, transport.as_ref()).await;
        }
    }

    /// Detach from the current connection: hooks unregistered, clock
    /// aborted and awaited (no zombie tick), wiring dropped, receive-side
    /// streams *stalled* (docs/streams.md: a disconnect never kills a
    /// stream — the objects stay with the session and resume at the next
    /// wire; only session death cancels them, via `SessionState::drop`).
    /// Transaction state stays with the session. Idempotent.
    pub(crate) async fn unwire(&self) {
        self.detach_connection().await;
        self.stall_streams().await;
        let handle = self.core.clock.write().await.take();
        if let Some(handle) = handle {
            handle.abort();
            let _ = handle.await;
        }
    }

    /// Drop the registration/write side of the wiring without stopping the
    /// clock: used by the clock task itself when the connection is known
    /// dead (the task exits right after, so its handle needs no abort).
    pub(super) async fn detach_connection(&self) {
        let transport = self.core.wiring.read().await.clone();
        if let Some(transport) = transport {
            transport.on_transfer_activity(None).await;
        }
        *self.core.wiring.write().await = None;
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
/// Whole-payload convenience form of [`open_binary_sink`] + `write` +
/// `finish`-without-waiting. Backpressure form unchanged (never blocks): at
/// the cap it fails immediately with `backpressure_stall` (§5: senders
/// pause DATA frames while over the cap; the caller retries — recovery is
/// automatic as ACKs release copies). Copy retention is per-frame (single
/// contiguous-ACK prefix releases its frames' bytes).
///
/// `on_complete` fires exactly once when the transaction reaches a terminal
/// state: `confirmed: true` on the final ACK; `confirmed: false` when the
/// send copy is released without confirmation (the receiver's resume
/// verdict failed the transaction after a reconnect, or the caller
/// aborted).
/// Failure paths notify too — a callback never fires zero times.
pub(crate) async fn send_binary(
    session: &TransferSession,
    wiring: &dyn TransferWiring,
    payload: Vec<u8>,
    on_complete: Option<CompletionCallback>,
) -> Result<[u8; 16], XacppError> {
    let txid = uuid::Uuid::new_v4().into_bytes();
    let mut st = session.core.state.lock().await;
    let now = now_ms();
    if st.sender.is_paused() {
        return Err(backpressure_error(&st));
    }
    st.sender
        .open_transaction(txid, payload.len() as u32, now)
        .map_err(|e| XacppError::Internal(e.to_string()))?;
    let mut frames = st
        .sender
        .append_chunk(txid, &payload, now)
        .map_err(|e| match e {
            BeginError::Backpressured { .. } => backpressure_error(&st),
            other => XacppError::Internal(other.to_string()),
        })?;
    frames.extend(
        st.sender
            .finish_transaction(txid, now)
            .map_err(|e| XacppError::Internal(e.to_string()))?,
    );
    if let Some(cb) = on_complete {
        st.completions.insert(txid, cb);
    }
    drop(st);
    for frame in frames {
        wiring.write_data_frame(frame).await?;
    }
    // Every completion path announces: the whole-payload path is a
    // completion too — the contract has no carve-out for it. The
    // announcement rides the data FIFO, keeping its place behind the
    // transaction's data frames (a completion that overtakes them would
    // fake a byte shortfall on the receiving side).
    let completed = crate::reliability::stream::completed_frame(txid)
        .map_err(|e| XacppError::Internal(e.to_string()))?;
    wiring.write_data_frame(completed).await?;
    Ok(txid)
}
