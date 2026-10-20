//! Streaming binary transfer surface (docs/streams.md): sender-side
//! [`BinarySink`] (incremental writes with ACK-driven backpressure,
//! pause/resume/abort) and receiver-side [`BinaryStream`] (in-order chunk
//! flow, pause/abort visible to the consumer).
//!
//! Wire cost: only the two SPEC §5 control frame types (TRANSACTION_PAUSE /
//! TRANSACTION_ABORT); framing, ACK/RESUME and backpressure semantics are
//! the 1.0 transfer layer's, unchanged.
//!
//! Lock discipline: `StreamInner` is lock-free — state lives in atomics.
//! The chunk channel's sender is owned by the session's stream-table entry
//! (not by `StreamInner`): pushing happens on the inbound dispatch path
//! which already holds the session state lock, and the channel closes by
//! ownership — removing/dropping the entry drops the sender, so terminal
//! states and session death need no locking anywhere (including `Drop`).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};

use crate::transfer::{BinarySendResult, TransferSession};
use crate::wire::error::XacppError;

/// Metadata for an outgoing binary transaction: the declared `total_len`
/// (carried on every frame header; the receiver's reassembly criterion,
/// SPEC §3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BinaryMeta {
    /// Total transaction byte count.
    pub total_len: u32,
}

/// Terminal / flow state of a receive-side [`BinaryStream`].
///
/// Terminal states are exactly three — [`Complete`](Self::Complete),
/// [`Cancelled`](Self::Cancelled) and [`Incomplete`](Self::Incomplete)
/// (docs/streams.md: a stream is only closed by completion,
/// cancellation, or a declared completion whose bytes fell short).
/// [`Stalled`](Self::Stalled) is a *flow* state: the connection dropped
/// while the transaction is still alive with the session; the stream object
/// survives and resumes after the next wire-up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum StreamState {
    /// Receiving (or waiting for) chunks.
    Active = 0,
    /// The holder paused this transaction (TRANSACTION_PAUSE captured);
    /// distinct from a stalled link — connection liveness is unaffected.
    Paused = 1,
    /// The connection dropped before completion: chunks stop flowing, the
    /// object and its buffer stay alive with the session, and the state
    /// clears back to Active/Paused when the connection is re-wired
    /// (the recovery handshake re-arms the holder's transmission). The
    /// holder's pause intent survives a disconnect.
    Stalled = 4,
    /// The transaction completed under §5 semantics; all bytes delivered.
    /// Terminal.
    Complete = 2,
    /// The transaction was cancelled: holder TRANSACTION_ABORT, the
    /// recovery cross-check (a RESUME declaration that did not cover this
    /// still-open transaction), a peer ERROR `transaction_expired`, or
    /// session death. Partial bytes are discarded, no further chunks
    /// arrive. Terminal.
    Cancelled = 3,
    /// The holder declared the transaction completed (TRANSACTION_COMPLETED)
    /// but the received bytes fall short of the declared total: the stream
    /// ends explicitly instead of waiting for bytes that will never come.
    /// The gap is readable through [`BinaryStream::incomplete_detail`].
    /// Terminal.
    Incomplete = 5,
}

impl StreamState {
    fn from_u8(v: u8) -> Self {
        // Mirrors the explicit discriminants above (single source of truth
        // for this internal, non-wire encoding).
        match v {
            1 => Self::Paused,
            2 => Self::Complete,
            3 => Self::Cancelled,
            4 => Self::Stalled,
            5 => Self::Incomplete,
            _ => Self::Active,
        }
    }

    /// Terminal three: `Complete` / `Cancelled` / `Incomplete` — the only
    /// states that close the stream (docs/streams.md).
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Complete | Self::Cancelled | Self::Incomplete)
    }
}

/// Byte gap of a stream that ended short of its declared total.
///
/// Read through [`BinaryStream::incomplete_detail`] when the state is
/// [`StreamState::Incomplete`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IncompleteDetail {
    /// Bytes actually received.
    pub received: u64,
    /// Declared total byte count.
    pub expected: u32,
}

/// One chunk of a [`BinaryStream`]: a contiguous-prefix frame's bytes, in
/// `seq` order across successive reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamChunk {
    /// Frame sequence within the transaction.
    pub seq: u32,
    /// Frame payload bytes.
    pub bytes: Vec<u8>,
}

pub(crate) type ChunkTx = tokio::sync::mpsc::UnboundedSender<StreamChunk>;
type ChunkRx = tokio::sync::mpsc::UnboundedReceiver<StreamChunk>;

/// Shared stream core: state atomics + the chunk channel. The channel
/// sender is taken **only** on terminal states so `read` yields `None` —
/// the stream object lives with the session and is closed exclusively by
/// completion or cancellation (docs/streams.md). A disconnect stalls
/// the flow (flag below) without touching the channel, the buffer, or the
/// stored reassembly copy; a re-wire clears the stall and the same object
/// keeps flowing.
pub(crate) struct StreamInner {
    txid: [u8; 16],
    total_len: u32,
    /// Terminal two + Active/Paused. `Stalled` is *not* stored here: it is
    /// the orthogonal `stalled` flag (a disconnect must not erase the
    /// holder's paused intent, and a pause must not erase a stall).
    state: AtomicU8,
    /// Connection-down flag (orthogonal to `state`). Set by the transport
    /// layer on any disconnect path; cleared on wire-up.
    stalled: AtomicBool,
    /// Consumer end of the chunk channel. The sender is owned by the
    /// session's stream-table entry: the channel closes when the entry is
    /// removed/dropped (terminal state or session death) — no lock, no
    /// explicit close.
    chunk_rx: tokio::sync::Mutex<ChunkRx>,
    /// Byte gap of an `Incomplete` terminal (bytes received / declared
    /// total). Written once, before the terminal state is published.
    incomplete_received: AtomicU64,
    incomplete_expected: AtomicU32,
}

impl StreamInner {
    /// Creates the inner core and returns it with the chunk-channel sender
    /// (the caller installs the sender into the session's stream-table
    /// entry — its ownership IS the channel's lifecycle).
    pub(crate) fn new(txid: [u8; 16], total_len: u32) -> (Arc<Self>, ChunkTx) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        (
            Arc::new(Self {
                txid,
                total_len,
                state: AtomicU8::new(StreamState::Active as u8),
                stalled: AtomicBool::new(false),
                chunk_rx: tokio::sync::Mutex::new(rx),
                incomplete_received: AtomicU64::new(0),
                incomplete_expected: AtomicU32::new(0),
            }),
            tx,
        )
    }

    /// Terminal state: flips the state atom. The channel closes separately
    /// by ownership (the session's table entry holding the sender is
    /// removed/dropped on the same path), so this is a pure atomic store —
    /// safe from any context, including `Drop`.
    pub(crate) fn finish(&self, state: StreamState) {
        debug_assert!(state.is_terminal(), "finish is for terminal states only");
        self.state.store(state as u8, Ordering::SeqCst);
    }

    /// Terminal state for a declared completion whose bytes fell short:
    /// records the gap, then ends the stream like any other terminal state.
    /// The gap is published before the terminal state, so a consumer that
    /// observes `Incomplete` always sees the matching counts.
    pub(crate) fn finish_incomplete(&self, received: u64, expected: u32) {
        self.incomplete_received.store(received, Ordering::SeqCst);
        self.incomplete_expected.store(expected, Ordering::SeqCst);
        self.finish(StreamState::Incomplete);
    }

    /// Pause without terminating (consumer-visible; heartbeat unaffected).
    /// Only while active: a terminal state wins over a late pause frame.
    pub(crate) fn set_paused(&self) {
        let _ = self.state.compare_exchange(
            StreamState::Active as u8,
            StreamState::Paused as u8,
            Ordering::SeqCst,
            Ordering::SeqCst,
        );
    }

    /// Clear the paused flag (holder resumed — next data frame arrives).
    pub(crate) fn clear_paused(&self) {
        let _ = self.state.compare_exchange(
            StreamState::Paused as u8,
            StreamState::Active as u8,
            Ordering::SeqCst,
            Ordering::SeqCst,
        );
    }

    /// Mark the flow stalled (connection dropped). Terminal states win —
    /// a completed/cancelled stream never reads Stalled.
    pub(crate) fn set_stalled(&self) {
        if !StreamState::from_u8(self.state.load(Ordering::SeqCst)).is_terminal() {
            self.stalled.store(true, Ordering::SeqCst);
        }
    }

    /// Clear the stall (connection re-wired): the flow returns to whatever
    /// Active/Paused state it held — the pause intent survives disconnects.
    pub(crate) fn clear_stalled(&self) {
        self.stalled.store(false, Ordering::SeqCst);
    }
}

/// Receive-side handle over an incoming binary transaction: chunks flow in
/// `seq` order via [`BinaryStream::read`]; the stream ends only on
/// transaction completion or cancellation (docs/streams.md) — a
/// disconnect merely stalls the flow (`read` suspends, the object and its
/// progress stay alive with the session, and the same object resumes after
/// the connection is re-wired). Meta is readable from the first frame
/// (SPEC §8.4: metadata is observed before any byte — the fetch responder
/// completes `fetch_ready` first over the ordered channel).
///
/// `Clone` shares the same underlying stream (single-reader convention:
/// concurrent `read` callers compete for chunks; `state()` polling needs no
/// reader at all).
#[derive(Clone)]
pub struct BinaryStream {
    pub(crate) inner: Arc<StreamInner>,
}

impl BinaryStream {
    /// Transaction id of the stream.
    pub fn txid(&self) -> [u8; 16] {
        self.inner.txid
    }

    /// Declared total byte count (from the frame headers).
    pub fn total_len(&self) -> u32 {
        self.inner.total_len
    }

    /// Current flow state. Read priority: terminal > stalled > paused >
    /// active — a disconnect is visible as [`StreamState::Stalled`] while
    /// the transaction lives; [`StreamState::Paused`] is consumer-visible
    /// and distinct from a stall (poll this while `read` is idle to
    /// distinguish a holder pause from a dead link).
    pub fn state(&self) -> StreamState {
        let state = StreamState::from_u8(self.inner.state.load(Ordering::SeqCst));
        if state.is_terminal() {
            return state;
        }
        if self.inner.stalled.load(Ordering::SeqCst) {
            return StreamState::Stalled;
        }
        state
    }

    /// Next chunk in `seq` order; `None` = the stream reached a terminal
    /// state (check [`Self::state`]). While the connection is down the
    /// read *suspends* (no chunks flow; the channel stays open) — it does
    /// not return `None`. Single-reader convention: concurrent readers
    /// compete for chunks.
    pub async fn read(&self) -> Option<StreamChunk> {
        self.inner.chunk_rx.lock().await.recv().await
    }

    /// Byte gap of an [`StreamState::Incomplete`] terminal: the bytes
    /// actually received and the declared total. `None` for every other
    /// state (the stream did not end short of its declared total).
    pub fn incomplete_detail(&self) -> Option<IncompleteDetail> {
        if self.state() != StreamState::Incomplete {
            return None;
        }
        Some(IncompleteDetail {
            received: self.inner.incomplete_received.load(Ordering::SeqCst),
            expected: self.inner.incomplete_expected.load(Ordering::SeqCst),
        })
    }
}

/// Receiver-side stream handler (single registration; a later registration
/// replaces the earlier one). Invoked on the first frame of a new
/// transaction while streaming is enabled; transactions that completed
/// before registration follow the aggregate `on_binary` semantics
/// unchanged.
pub type StreamHandler = Arc<dyn Fn(BinaryStream) + Send + Sync>;

/// Sender-side handle over an outgoing binary transaction: write chunks
/// (backpressure parks the writer until ACKs release copies), finish for
/// confirmation, or pause/resume/abort the transaction (SPEC §5).
pub struct BinarySink {
    pub(crate) session: TransferSession,
    pub(crate) txid: [u8; 16],
}

impl BinarySink {
    /// Transaction id of the stream.
    pub fn txid(&self) -> [u8; 16] {
        self.txid
    }

    /// Write one chunk. Parks while the connection-level backpressure cap
    /// (§5) is reached — recovery is automatic as ACKs release copies —
    /// and while the transaction is paused (`pause()`).
    pub async fn write(&self, chunk: &[u8]) -> Result<(), XacppError> {
        self.session.sink_write(self.txid, chunk).await
    }

    /// Close the frame sequence (flushes a partial tail frame), then wait
    /// for the final ACK. `Ok(result)` with `confirmed: true` = confirmed
    /// delivery; `confirmed: false` = the transaction was cancelled without
    /// confirmation (caller abort, or the receiver's resume verdict failed
    /// it after a reconnect). The wait spans disconnects: it resolves once
    /// the session is re-wired and the final ACK (or a cancellation)
    /// arrives.
    pub async fn finish(self) -> Result<BinarySendResult, XacppError> {
        self.session.sink_finish(self.txid).await
    }

    /// Pause the transaction: new frames are not sent, the retransmission
    /// clock skips it, and the peer is notified (TRANSACTION_PAUSE, SPEC §5
    /// MUST emit). Already-sent frames' ACKs still count.
    pub async fn pause(&self) -> Result<(), XacppError> {
        self.session.sink_pause(self.txid).await
    }

    /// Resume the transaction: the retransmission clock restarts and parked
    /// writes drain. No explicit frame — resumption is implicit (SPEC §5);
    /// the peer observes it via the next data frame.
    pub async fn resume(&self) -> Result<(), XacppError> {
        self.session.sink_resume(self.txid).await
    }

    /// Abort the transaction: release the retained copy, notify the peer
    /// (TRANSACTION_ABORT, SPEC §5 MUST emit) and fire the transaction's
    /// terminal state as unconfirmed. The id cannot be reused afterwards.
    pub async fn abort(self) -> Result<(), XacppError> {
        self.session.sink_abort(self.txid).await
    }
}

/// Build the TRANSACTION_PAUSE frame (zeroed header identity per the
/// control-frame convention; the txid lives in the payload, SPEC §5).
///
/// Transaction control frames (pause/abort/completed) all travel the
/// **data** FIFO channel: the control channel is drained with priority
/// and would let them overtake the transaction's own queued data frames —
/// a completion would fake a byte shortfall, an abort would be ignored by
/// a peer that never saw the stream (the late first frame would then open
/// a stream that never terminates). The wire frame types are unchanged;
/// only the local scheduling path is the FIFO. (The sender MAY expedite a
/// control frame only when nothing of the transaction is queued ahead of
/// it; the unconditional FIFO is the always-correct default.)
pub(crate) fn pause_frame(txid: [u8; 16]) -> Result<crate::wire::frame::Frame, XacppError> {
    let payload = crate::wire::control::TransactionPausePayload {
        transaction_id: txid,
    }
    .encode()
    .map_err(|e| XacppError::Internal(e.to_string()))?;
    Ok(crate::wire::frame::Frame::new(
        crate::wire::frame::FrameType::TransactionPause,
        [0u8; 16],
        0,
        0,
        payload,
    ))
}

/// Build the TRANSACTION_ABORT frame (same FIFO discipline as
/// [`pause_frame`]).
pub(crate) fn abort_frame(txid: [u8; 16]) -> Result<crate::wire::frame::Frame, XacppError> {
    let payload = crate::wire::control::TransactionAbortPayload {
        transaction_id: txid,
    }
    .encode()
    .map_err(|e| XacppError::Internal(e.to_string()))?;
    Ok(crate::wire::frame::Frame::new(
        crate::wire::frame::FrameType::TransactionAbort,
        [0u8; 16],
        0,
        0,
        payload,
    ))
}

/// Build the TRANSACTION_COMPLETED frame: the holder declares the
/// transaction closed (every declared byte sent).
///
/// The frame travels the **data** FIFO channel, not the control channel:
/// the control channel is drained with priority and would let the
/// completion overtake already-queued data frames — the peer would then
/// see "bytes not yet complete + completed" and wrongly end the stream as
/// `Incomplete`. The wire frame type stays `0x000B` (the peer dispatches
/// on the frame type); only the local scheduling path differs.
pub(crate) fn completed_frame(txid: [u8; 16]) -> Result<crate::wire::frame::Frame, XacppError> {
    let payload = crate::wire::control::TransactionCompletedPayload {
        transaction_id: txid,
    }
    .encode()
    .map_err(|e| XacppError::Internal(e.to_string()))?;
    Ok(crate::wire::frame::Frame::new(
        crate::wire::frame::FrameType::TransactionCompleted,
        [0u8; 16],
        0,
        0,
        payload,
    ))
}
