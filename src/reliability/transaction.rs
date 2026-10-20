//! Transaction send-side management (SPEC §5 transfer reliability semantics).
//!
//! [`TransactionSender`] is the copy holder: it splits payloads into
//! `DATA_BINARY` working frames, retains the frame copies until the final
//! ACK or a cancellation (SPEC §5 v3: copy lifetime = transaction
//! lifetime), computes retransmission lists on timeout
//! (exponential backoff) and on NACK, enforces the unacknowledged-bytes
//! backpressure cap, and drives the holder side of the recovery
//! discipline: declaring pending sends on wire-up (`declare_pending`,
//! which gates them against retransmission) and applying the receiver's
//! per-transaction verdicts (`apply_resume_verdicts`).
//!
//! Pure synchronous component with injected time: every method takes the
//! current instant in milliseconds (`now_ms`) — no internal timers. The
//! integration layer drives the clock and the egress. Peer-reported
//! numbers (`highest_contiguous_seq`, `received_frames`, NACK seqs) are
//! treated as adversarial: out-of-range values are clamped or ignored,
//! never trusted for indexing.

use std::collections::HashMap;

use crate::wire::control::{AckEntry, ResumeStatus, ResumeStatusEntry};
use crate::wire::fetch::format_transaction_id;
use crate::wire::frame::{Frame, FrameType};

/// Sender tunables (SPEC §5 RECOMMENDED defaults).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SenderConfig {
    /// Working-frame payload size (§4: implementations SHOULD use ~1 MiB
    /// `DATA_BINARY` working frames; hard ceiling 16 MiB). Values below 1
    /// are clamped to 1 at construction.
    pub frame_size: usize,
    /// Initial retransmission timeout (ms; RECOMMENDED 2 s).
    pub rto_initial_ms: u64,
    /// Retransmission timeout ceiling in ms (backoff ×2, RECOMMENDED
    /// ceiling 30 s).
    pub rto_max_ms: u64,
    /// Backpressure cap on the sum of unacknowledged transaction bytes
    /// (RECOMMENDED default 64 MiB, §5).
    pub backpressure_cap_bytes: u64,
}

impl Default for SenderConfig {
    fn default() -> Self {
        Self {
            frame_size: 1024 * 1024,
            rto_initial_ms: 2_000,
            rto_max_ms: 30_000,
            backpressure_cap_bytes: 64 * 1024 * 1024,
        }
    }
}

/// `begin_transaction` failure.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BeginError {
    /// The payload would push unacknowledged bytes past the backpressure
    /// cap (§5: senders pause DATA frames while over the cap).
    #[error("backpressured: unacked {unacked_bytes} + incoming {incoming_bytes} > cap {cap_bytes}")]
    Backpressured {
        /// Currently unacknowledged bytes.
        unacked_bytes: u64,
        /// Bytes of the rejected payload.
        incoming_bytes: u64,
        /// Configured cap.
        cap_bytes: u64,
    },
    /// Empty payload: a zero-byte transaction cannot exist on the wire
    /// (every frame carries at least one payload byte).
    #[error("empty payload: transactions must carry at least one byte")]
    EmptyPayload,
    /// Payload exceeds the u32 `total_len` field (§3: larger content is
    /// split into multiple transactions by the message layer).
    #[error("payload too large for one transaction: {len} bytes (u32 max {max})")]
    PayloadTooLarge {
        /// Payload byte count.
        len: u64,
        /// `u32::MAX`.
        max: u32,
    },
    /// The transaction id is already held (active copy). Transaction ids
    /// are globally unique UUIDs; reuse is a caller bug.
    #[error("transaction already registered")]
    DuplicateTransaction,
    /// Incremental API used on a transaction id that has no open slot.
    #[error("unknown transaction")]
    UnknownTransaction,
    /// Incremental write would exceed the declared `total_len`.
    #[error("write would exceed declared total_len {total}")]
    TotalLenExceeded {
        /// Declared transaction size.
        total: u32,
    },
    /// `finish` with fewer bytes written than the declared `total_len`.
    #[error("incomplete payload: {written} of {declared} declared bytes written")]
    IncompletePayload {
        /// Bytes appended so far.
        written: u64,
        /// Declared transaction size.
        declared: u32,
    },
}

/// Current retransmission timeout for a frame with `attempts` failed
/// sends: `rto_initial × 2^attempts`, capped at `rto_max`.
fn current_rto(rto_initial_ms: u64, rto_max_ms: u64, attempts: u32) -> u64 {
    let shift = attempts.min(62);
    rto_initial_ms.saturating_mul(1u64 << shift).min(rto_max_ms)
}

/// Per-frame send bookkeeping.
#[derive(Debug, Clone, Copy)]
struct FrameState {
    confirmed: bool,
    last_sent_ms: u64,
    attempts: u32,
}

/// One held transaction copy.
#[derive(Debug)]
struct PendingTxn {
    /// Declared total payload size (`total_len` on every frame header).
    total_len: u32,
    /// Working frames; `None` once the frame's copy was released by a
    /// contiguous-prefix ACK (per-frame copy retention — streaming keeps
    /// memory at O(unacked frames), not O(payload)).
    frames: Vec<Option<Frame>>,
    states: Vec<FrameState>,
    /// Sub-frame tail of a streaming transaction (`append_chunk` holds
    /// bytes here until a full working frame accumulates; `finish`
    /// flushes it as the tail frame).
    partial: Vec<u8>,
    /// Total bytes appended so far (including `partial`).
    written: u64,
    /// Unacknowledged bytes still held (frames + partial).
    bytes: u64,
    /// Holder paused: new frames are not sent and the retransmission clock
    /// skips this transaction (SPEC §5 pause).
    paused: bool,
    /// Reconnect gate (recovery discipline): set when the holder declares
    /// its pending sends at wire-up; the transaction sends and retransmits
    /// nothing until the receiver's verdict arrives (`Resumed` lifts the
    /// gate, `Unknown`/`Rejected` fails the transaction out). Blind
    /// retransmission of retained copies is retired — the receiver's
    /// verdict is the only lawful trigger.
    awaiting_resume: bool,
    /// Highest contiguous progress reported by the peer (`0..=seq` all
    /// received); clamped to `frame_count - 1`.
    acked_contiguous: Option<u32>,
    /// Highest total-frames-received reported by the peer (sparse residue
    /// detector, §5).
    acked_received: u32,
    /// Last peer-driven progress instant (ACK / NACK / RESUME). Sender-side
    /// retransmits do NOT refresh it (they are not peer activity). With the
    /// retention mechanism retired (SPEC §5: copy lifetime = transaction
    /// lifetime) this is bookkeeping only.
    last_progress_ms: u64,
}

impl PendingTxn {
    fn frame_count(&self) -> u32 {
        self.frames.len() as u32
    }

    /// Complete per §5: every declared byte is framed (written covers
    /// `total_len`, nothing held in the partial buffer — no more frames
    /// can exist) AND the peer's contiguous progress covers the final
    /// frame with no sparse residue. Tombstones (empty frames) never read
    /// complete.
    fn is_fully_acked(&self) -> bool {
        if self.written != self.total_len as u64 || !self.partial.is_empty() {
            return false;
        }
        let last = self.frame_count().checked_sub(1);
        self.acked_contiguous == last && last.is_some() && self.acked_received >= self.frame_count()
    }
}

/// Outcome of applying the receiver's resume verdicts (recovery
/// discipline: after a reconnect the receiver's verdict is the only lawful
/// retransmission trigger).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ResumeVerdicts {
    /// Frames to retransmit (every unconfirmed frame of each `Resumed`
    /// transaction; the holder's own ACK bookkeeping is the watermark).
    pub retransmit: Vec<Frame>,
    /// Transactions the receiver did not recognize (completed and
    /// released, aborted, or never seen — no tombstone exists): the
    /// transfer fails as a whole; the holder released their copies.
    pub failed: Vec<[u8; 16]>,
}

/// Send-side transaction manager (copy holder).
#[derive(Debug, Default)]
pub struct TransactionSender {
    config: SenderConfig,
    pending: HashMap<[u8; 16], PendingTxn>,
}

impl TransactionSender {
    /// Create a sender with the given configuration.
    pub fn new(config: SenderConfig) -> Self {
        let mut config = config;
        config.frame_size = config.frame_size.max(1);
        Self {
            config,
            pending: HashMap::new(),
        }
    }

    /// Whether the transaction has an open slot (active copy).
    pub fn contains_transaction(&self, transaction_id: [u8; 16]) -> bool {
        self.pending.contains_key(&transaction_id)
    }

    /// Borrowed configuration.
    pub fn config(&self) -> &SenderConfig {
        &self.config
    }

    /// Sum of unacknowledged transaction bytes still held (tombstones hold
    /// zero bytes).
    pub fn unacked_bytes(&self) -> u64 {
        self.pending.values().map(|txn| txn.bytes).sum()
    }

    /// Backpressure state (§5): `true` while unacknowledged bytes sit at
    /// or above the cap. Recovers automatically as ACKs release copies.
    pub fn is_paused(&self) -> bool {
        self.unacked_bytes() >= self.config.backpressure_cap_bytes
    }

    /// Split `payload` into `DATA_BINARY` working frames and start holding
    /// the transaction copy.
    ///
    /// Whole-payload convenience form of [`Self::open_transaction`] +
    /// [`Self::append_chunk`] + [`Self::finish_transaction`]: identical
    /// checks (duplicate / aborted / empty / backpressure over the entire
    /// payload) and identical frame sequence.
    ///
    /// Returns the frame sequence for the integration layer to write to
    /// egress, in order (`seq 0..n`). All frames are marked sent at
    /// `now_ms`.
    pub fn begin_transaction(
        &mut self,
        transaction_id: [u8; 16],
        payload: &[u8],
        now_ms: u64,
    ) -> Result<Vec<Frame>, BeginError> {
        let incoming = payload.len() as u64;
        let unacked = self.unacked_bytes();
        if unacked + incoming > self.config.backpressure_cap_bytes {
            return Err(BeginError::Backpressured {
                unacked_bytes: unacked,
                incoming_bytes: incoming,
                cap_bytes: self.config.backpressure_cap_bytes,
            });
        }
        self.open_transaction(transaction_id, payload.len() as u32, now_ms)?;
        let mut frames = self.append_chunk(transaction_id, payload, now_ms)?;
        frames.extend(self.finish_transaction(transaction_id, now_ms)?);
        Ok(frames)
    }

    /// Open a streaming transaction slot: declares `total_len` (carried on
    /// every frame header; the receiver's reassembly criterion) and starts
    /// holding the copy. Bytes follow via [`Self::append_chunk`]; the frame
    /// sequence is closed by [`Self::finish_transaction`].
    pub fn open_transaction(
        &mut self,
        transaction_id: [u8; 16],
        total_len: u32,
        now_ms: u64,
    ) -> Result<(), BeginError> {
        if self.pending.contains_key(&transaction_id) {
            return Err(BeginError::DuplicateTransaction);
        }
        if total_len == 0 {
            return Err(BeginError::EmptyPayload);
        }
        self.pending.insert(
            transaction_id,
            PendingTxn {
                total_len,
                frames: Vec::new(),
                states: Vec::new(),
                partial: Vec::new(),
                written: 0,
                bytes: 0,
                paused: false,
                awaiting_resume: false,
                acked_contiguous: None,
                acked_received: 0,
                last_progress_ms: now_ms,
            },
        );
        Ok(())
    }

    /// Append streaming bytes; returns the complete working frames that
    /// accumulated from this chunk (zero or more — chunks may straddle
    /// frame boundaries; the sub-frame tail is held in the transaction's
    /// partial buffer until more bytes or `finish` arrive).
    ///
    /// Backpressure (§5): when the held unacknowledged bytes plus this
    /// chunk would exceed the cap, `Backpressured` is returned and nothing
    /// is buffered — the caller parks and retries (recovery is automatic
    /// as ACKs release copies).
    pub fn append_chunk(
        &mut self,
        transaction_id: [u8; 16],
        chunk: &[u8],
        now_ms: u64,
    ) -> Result<Vec<Frame>, BeginError> {
        let unacked = self.unacked_bytes();
        let txn = self
            .pending
            .get_mut(&transaction_id)
            .ok_or(BeginError::UnknownTransaction)?;
        if unacked + chunk.len() as u64 > self.config.backpressure_cap_bytes {
            return Err(BeginError::Backpressured {
                unacked_bytes: unacked,
                incoming_bytes: chunk.len() as u64,
                cap_bytes: self.config.backpressure_cap_bytes,
            });
        }
        if txn.written + chunk.len() as u64 > txn.total_len as u64 {
            return Err(BeginError::TotalLenExceeded {
                total: txn.total_len,
            });
        }
        txn.partial.extend_from_slice(chunk);
        txn.written += chunk.len() as u64;
        txn.bytes += chunk.len() as u64;
        let mut out = Vec::new();
        while txn.partial.len() >= self.config.frame_size {
            let payload: Vec<u8> = txn.partial.drain(..self.config.frame_size).collect();
            // partial → frame: equal-amount transfer, the bytes stay held
            // (the frame copy is the retained unacked payload).
            let seq = txn.frames.len() as u32;
            txn.frames.push(Some(Frame::new(
                FrameType::DataBinary,
                transaction_id,
                txn.total_len,
                seq,
                payload.clone(),
            )));
            txn.states.push(FrameState {
                confirmed: false,
                last_sent_ms: now_ms,
                attempts: 0,
            });
            out.push(Frame::new(
                FrameType::DataBinary,
                transaction_id,
                txn.total_len,
                seq,
                payload,
            ));
        }
        Ok(out)
    }

    /// Flush the partial buffer as the tail frame (when a sub-frame
    /// remainder is held) and validate that the declared `total_len` was
    /// written exactly. Returns the tail frames to write to egress
    /// (possibly none — an exact frame-multiple payload has no tail).
    /// Idempotent: a repeat call with nothing left to flush returns no
    /// frames.
    pub fn finish_transaction(
        &mut self,
        transaction_id: [u8; 16],
        now_ms: u64,
    ) -> Result<Vec<Frame>, BeginError> {
        let txn = self
            .pending
            .get_mut(&transaction_id)
            .ok_or(BeginError::UnknownTransaction)?;
        let mut out = Vec::new();
        if !txn.partial.is_empty() {
            let payload = std::mem::take(&mut txn.partial);
            // partial → frame: equal-amount transfer, the bytes stay held.
            let seq = txn.frames.len() as u32;
            txn.frames.push(Some(Frame::new(
                FrameType::DataBinary,
                transaction_id,
                txn.total_len,
                seq,
                payload.clone(),
            )));
            txn.states.push(FrameState {
                confirmed: false,
                last_sent_ms: now_ms,
                attempts: 0,
            });
            out.push(Frame::new(
                FrameType::DataBinary,
                transaction_id,
                txn.total_len,
                seq,
                payload,
            ));
        }
        if txn.written != txn.total_len as u64 {
            return Err(BeginError::IncompletePayload {
                written: txn.written,
                declared: txn.total_len,
            });
        }
        Ok(out)
    }

    /// Pause the transaction: new frames are not sent and the
    /// retransmission clock skips it until [`Self::set_paused`] clears the
    /// flag (wire counterpart: TRANSACTION_PAUSE, SPEC §5). Returns whether
    /// the transaction exists and is active.
    pub fn set_paused(&mut self, transaction_id: [u8; 16], paused: bool, now_ms: u64) -> bool {
        let Some(txn) = self.pending.get_mut(&transaction_id) else {
            return false;
        };
        txn.paused = paused;
        if !paused {
            // Resume: the retransmission clock restarts from now — a pause
            // window must not collapse the RTO of frames that aged through
            // it.
            for state in &mut txn.states {
                if !state.confirmed {
                    state.last_sent_ms = now_ms;
                }
            }
        }
        true
    }

    /// Abort the transaction: release the retained copy immediately (wire
    /// counterpart: TRANSACTION_ABORT, SPEC §5). Idempotent. Returns
    /// whether the copy existed (callers use this to fire send completions
    /// / notify the peer).
    pub fn abort_transaction(&mut self, transaction_id: [u8; 16]) -> bool {
        self.pending.remove(&transaction_id).is_some()
    }

    /// Whether the transaction is currently paused ([`Self::set_paused`]).
    /// Unknown transactions read as not paused (the caller's append fails
    /// with `UnknownTransaction` anyway).
    pub fn is_transaction_paused(&self, transaction_id: [u8; 16]) -> bool {
        self.pending
            .get(&transaction_id)
            .is_some_and(|txn| txn.paused)
    }

    /// Apply one cumulative ACK entry; returns `true` when the entry
    /// completes the transaction and its copy was released.
    pub fn handle_ack(&mut self, entry: &AckEntry, now_ms: u64) -> bool {
        let Some(txn) = self.pending.get_mut(&entry.transaction_id) else {
            return false;
        };
        txn.acked_contiguous = txn
            .acked_contiguous
            .map_or(Some(entry.highest_contiguous_seq), |h| {
                Some(h.max(entry.highest_contiguous_seq))
            });
        txn.acked_received = txn.acked_received.max(entry.received_frames);
        txn.last_progress_ms = now_ms;
        // Clamp adversarial progress reports to the real frame range.
        let last = txn.frame_count().saturating_sub(1);
        txn.acked_contiguous = txn.acked_contiguous.map(|h| h.min(last));
        let confirmed_up_to =
            (txn.acked_contiguous.unwrap_or(0) as usize + 1).min(txn.states.len());
        for state in &mut txn.states[..confirmed_up_to] {
            state.confirmed = true;
        }
        // Per-frame copy release: the contiguous prefix is confirmed, so
        // its frame copies are dropped (bytes freed). Streaming keeps
        // memory at O(unacked frames) instead of O(payload).
        for frame in &mut txn.frames[..confirmed_up_to] {
            if let Some(released) = frame.take() {
                txn.bytes = txn.bytes.saturating_sub(released.payload.len() as u64);
            }
        }
        if txn.is_fully_acked() {
            // Terminal confirmed release: the transaction is done, the
            // copy is dropped — nothing is kept about it.
            self.pending.remove(&entry.transaction_id);
            return true;
        }
        false
    }

    /// Apply a NACK: returns the listed frames for immediate
    /// retransmission (§5: "a NACK triggers immediate retransmission").
    ///
    /// NACK-driven resends do not advance the backoff attempt counter —
    /// they refresh `last_sent` so the timeout clock restarts at the
    /// current RTO.
    pub fn handle_nack(
        &mut self,
        transaction_id: [u8; 16],
        missing_seqs: &[u32],
        now_ms: u64,
    ) -> Vec<Frame> {
        let Some(txn) = self.pending.get_mut(&transaction_id) else {
            return Vec::new();
        };
        txn.last_progress_ms = now_ms;
        // Paused (SPEC §5): retransmission of this transaction is
        // suspended — NACKs are neither answered nor do they advance the
        // backoff clock while paused.
        if txn.paused {
            return Vec::new();
        }
        let mut out = Vec::with_capacity(missing_seqs.len());
        for &seq in missing_seqs {
            let Some(state) = txn.states.get_mut(seq as usize) else {
                continue; // out-of-range seq: ignore
            };
            if state.confirmed {
                continue; // peer self-corrected; do not resurrect
            }
            state.last_sent_ms = now_ms;
            // An unconfirmed frame keeps its copy (confirmed frames are
            // released) — the copy is therefore present. Degrade to a skip
            // instead of a panic if the invariant is ever broken.
            let Some(frame) = txn.frames[seq as usize].clone() else {
                log::error!(
                    "reliability: unconfirmed frame {seq} of transaction {} has no retained copy",
                    format_transaction_id(transaction_id)
                );
                continue;
            };
            out.push(frame);
        }
        out
    }

    /// Frames whose retransmission timeout elapsed: not yet confirmed and
    /// unsent for at least the current RTO. Each returned frame's clock
    /// restarts and its backoff attempt advances (next timeout ×2).
    pub fn due_retransmissions(&mut self, now_ms: u64) -> Vec<Frame> {
        let (rto_initial, rto_max) = (self.config.rto_initial_ms, self.config.rto_max_ms);
        let ids: Vec<[u8; 16]> = self.pending.keys().copied().collect();
        let mut out = Vec::new();
        for id in ids {
            let Some(txn) = self.pending.get_mut(&id) else {
                continue;
            };
            // Paused (SPEC §5): the retransmission clock skips this
            // transaction entirely (already-sent frames' ACKs still count).
            // Awaiting the receiver's resume verdict after a reconnect:
            // same skip — blind retransmission of retained copies is
            // retired; only the receiver's verdict re-arms the clock.
            if txn.paused || txn.awaiting_resume {
                continue;
            }
            for (state, frame) in txn.states.iter_mut().zip(txn.frames.iter()) {
                if state.confirmed {
                    continue;
                }
                let rto = current_rto(rto_initial, rto_max, state.attempts);
                if now_ms.saturating_sub(state.last_sent_ms) >= rto {
                    state.last_sent_ms = now_ms;
                    state.attempts += 1;
                    // Same invariant as above: an unconfirmed frame keeps
                    // its copy. Degrade to a skip instead of a panic.
                    let Some(frame) = frame.clone() else {
                        log::error!(
                            "reliability: unconfirmed frame of transaction {} has no retained copy",
                            format_transaction_id(id)
                        );
                        continue;
                    };
                    out.push(frame);
                }
            }
        }
        out
    }

    /// Declare every pending send transaction at wire-up and arm the
    /// reconnect gate on each (recovery discipline): the holder announces
    /// its pending set, then sends and retransmits nothing for these
    /// transactions until the receiver's per-transaction verdict arrives.
    /// The AckEntry progress fields carry the holder's written progress
    /// (informational; the receiver adjudicates on the txid alone — its
    /// own ACK stream is the watermark, which is why the resume
    /// instruction carries no offset).
    pub fn declare_pending(&mut self) -> Vec<AckEntry> {
        self.pending
            .iter_mut()
            .map(|(id, txn)| {
                txn.awaiting_resume = true;
                AckEntry {
                    transaction_id: *id,
                    highest_contiguous_seq: txn.frame_count().saturating_sub(1),
                    received_frames: txn.frame_count(),
                }
            })
            .collect()
    }

    /// Whether the transaction is gated awaiting the receiver's resume
    /// verdict (writers park on this exactly like they park on pause).
    pub fn is_awaiting_resume(&self, transaction_id: [u8; 16]) -> bool {
        self.pending
            .get(&transaction_id)
            .map(|txn| txn.awaiting_resume)
            .unwrap_or(false)
    }

    /// Apply the receiver's per-transaction resume verdicts:
    ///
    /// - `Resumed` — the receiver still holds the transaction open: lift
    ///   the gate and retransmit every unconfirmed frame. A user-paused
    ///   transaction only lifts the gate without emitting (the pause
    ///   intent dominates; unpausing rearms the clock and
    ///   `due_retransmissions` covers the gap).
    /// - `Unknown` / `Rejected` — the receiver does not recognize the
    ///   transaction: the transfer fails as a whole. The copy is released
    ///   and the id lands in `failed` so the caller can release its
    ///   waiters with a failure terminal — NOT a confirmation; completion
    ///   remains final-ACK-only.
    pub fn apply_resume_verdicts(
        &mut self,
        entries: &[ResumeStatusEntry],
        now_ms: u64,
    ) -> ResumeVerdicts {
        let mut out = ResumeVerdicts::default();
        for entry in entries {
            match entry.status {
                ResumeStatus::Resumed => {
                    let Some(txn) = self.pending.get_mut(&entry.transaction_id) else {
                        continue;
                    };
                    txn.awaiting_resume = false;
                    txn.last_progress_ms = now_ms;
                    if txn.paused {
                        continue;
                    }
                    for (state, frame) in txn.states.iter_mut().zip(txn.frames.iter()) {
                        if state.confirmed {
                            continue;
                        }
                        state.last_sent_ms = now_ms;
                        state.attempts += 1;
                        let Some(frame) = frame.clone() else {
                            continue;
                        };
                        out.retransmit.push(frame);
                    }
                }
                ResumeStatus::Unknown | ResumeStatus::Rejected => {
                    if self.pending.remove(&entry.transaction_id).is_some() {
                        out.failed.push(entry.transaction_id);
                    }
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn txid(seed: u8) -> [u8; 16] {
        let mut id = [0u8; 16];
        id[0] = seed;
        id[15] = seed;
        id
    }

    fn small_config() -> SenderConfig {
        SenderConfig {
            frame_size: 4,
            rto_initial_ms: 2_000,
            rto_max_ms: 30_000,
            backpressure_cap_bytes: 1_000,
        }
    }

    fn ack(id: [u8; 16], highest: u32, received: u32) -> AckEntry {
        AckEntry {
            transaction_id: id,
            highest_contiguous_seq: highest,
            received_frames: received,
        }
    }

    #[test]
    fn chunking_counts_and_tail() {
        let mut sender = TransactionSender::new(small_config());
        let id = txid(1);
        let payload: Vec<u8> = (0..10u8).collect();
        let frames = sender.begin_transaction(id, &payload, 0).unwrap();
        assert_eq!(frames.len(), 3); // 4 + 4 + 2
        for (i, frame) in frames.iter().enumerate() {
            assert_eq!(frame.frame_type, FrameType::DataBinary);
            assert_eq!(frame.transaction_id, id);
            assert_eq!(frame.total_len, 10);
            assert_eq!(frame.seq, i as u32);
        }
        assert_eq!(frames[0].payload, &[0, 1, 2, 3]);
        assert_eq!(frames[1].payload, &[4, 5, 6, 7]);
        assert_eq!(frames[2].payload, &[8, 9]); // tail remainder
    }

    #[test]
    fn chunking_exact_division_no_empty_tail() {
        let mut sender = TransactionSender::new(small_config());
        let payload = vec![7u8; 8];
        let frames = sender.begin_transaction(txid(2), &payload, 0).unwrap();
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[1].payload.len(), 4);
    }

    #[test]
    fn single_frame_transaction() {
        let mut sender = TransactionSender::new(small_config());
        let payload = vec![1u8; 3];
        let frames = sender.begin_transaction(txid(3), &payload, 0).unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].seq, 0);
        assert_eq!(frames[0].total_len, 3);
    }

    #[test]
    fn empty_payload_rejected() {
        let mut sender = TransactionSender::new(small_config());
        assert_eq!(
            sender.begin_transaction(txid(1), &[], 0),
            Err(BeginError::EmptyPayload)
        );
    }

    #[test]
    fn duplicate_transaction_rejected() {
        let mut sender = TransactionSender::new(small_config());
        let id = txid(1);
        sender.begin_transaction(id, &[1, 2], 0).unwrap();
        assert_eq!(
            sender.begin_transaction(id, &[3, 4], 0),
            Err(BeginError::DuplicateTransaction)
        );
    }

    #[test]
    fn frame_size_clamped_to_one() {
        let config = SenderConfig {
            frame_size: 0,
            ..small_config()
        };
        let mut sender = TransactionSender::new(config);
        let frames = sender.begin_transaction(txid(1), &[1, 2, 3], 0).unwrap();
        assert_eq!(frames.len(), 3);
    }

    #[test]
    fn final_ack_releases_copy() {
        let mut sender = TransactionSender::new(small_config());
        let id = txid(1);
        // 6 bytes @ frame_size 4 → 2 frames (seq 0, 1).
        sender
            .begin_transaction(id, &[1, 2, 3, 4, 5, 6], 0)
            .unwrap();
        assert_eq!(sender.unacked_bytes(), 6);
        // Partial progress: the contiguous prefix (frame 0) is confirmed
        // and its copy released immediately — per-frame retention (design
        // anchor §2.1: streaming keeps memory at O(unacked frames)).
        assert!(!sender.handle_ack(&ack(id, 0, 1), 100));
        assert_eq!(sender.unacked_bytes(), 2);
        // Final ACK (contiguity covers the last frame, no residue).
        assert!(sender.handle_ack(&ack(id, 1, 2), 200));
        assert_eq!(sender.unacked_bytes(), 0);
    }

    #[test]
    fn sparse_residue_blocks_completion() {
        let mut sender = TransactionSender::new(small_config());
        let id = txid(1);
        sender
            .begin_transaction(id, &[1, 2, 3, 4, 5, 6], 0)
            .unwrap();
        // Contiguity reported past the tail with a frame count lower than
        // ours (impossible from an honest peer; completion must not fire).
        // The clamped prefix covers every frame → both copies released
        // per-frame; the residue count only blocks the completion signal.
        assert!(!sender.handle_ack(&ack(id, 2, 1), 100));
        assert_eq!(sender.unacked_bytes(), 0);
    }

    #[test]
    fn adversarial_ack_progress_clamped() {
        let mut sender = TransactionSender::new(small_config());
        let id = txid(1);
        sender.begin_transaction(id, &[1, 2, 3, 4], 0).unwrap();
        // u32::MAX report on a 1-frame transaction clamps to
        // contiguous=0 (== last) with received ≥ 1 → reads complete, the
        // copy is released. No panic anywhere.
        assert!(sender.handle_ack(&ack(id, u32::MAX, u32::MAX), 100));
        assert_eq!(sender.unacked_bytes(), 0);
        // Same hostile report on a 2-frame transaction: contiguous clamps
        // to the last frame (both frames confirmed → both copies released)
        // but the residue count (1 < 2) blocks completion.
        let id2 = txid(2);
        sender
            .begin_transaction(id2, &[1, 2, 3, 4, 5, 6], 0)
            .unwrap();
        assert!(!sender.handle_ack(&ack(id2, u32::MAX, 1), 100));
        assert_eq!(sender.unacked_bytes(), 0);
    }

    #[test]
    fn unknown_ack_is_noop() {
        let mut sender = TransactionSender::new(small_config());
        assert!(!sender.handle_ack(&ack(txid(9), 5, 5), 0));
    }

    #[test]
    fn timeout_retransmission_and_backoff() {
        let mut sender = TransactionSender::new(small_config());
        let id = txid(1);
        // 2 frames.
        sender
            .begin_transaction(id, &[1, 2, 3, 4, 5, 6], 0)
            .unwrap();
        // t < 2000: nothing due.
        assert!(sender.due_retransmissions(1_999).is_empty());
        // t = 2000: both frames due (initial RTO 2 s).
        let due = sender.due_retransmissions(2_000);
        assert_eq!(due.len(), 2);
        // Clock restarted: nothing due immediately.
        assert!(sender.due_retransmissions(2_001).is_empty());
        // Backoff ×2 → due at 2000 + 4000 = 6000.
        assert!(sender.due_retransmissions(5_999).is_empty());
        let due = sender.due_retransmissions(6_000);
        assert_eq!(due.len(), 2);
        // Confirmed frames never come back.
        sender.handle_ack(&ack(id, 0, 1), 6_001);
        let due = sender.due_retransmissions(6_000 + 8_000);
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].seq, 1);
    }

    #[test]
    fn rto_caps_at_max() {
        let mut sender = TransactionSender::new(small_config());
        let id = txid(1);
        sender.begin_transaction(id, &[1], 0).unwrap();
        // Drive attempts far past the ceiling: every resend is due 30 s
        // after the previous one, never more.
        let mut t = 0u64;
        for _ in 0..20 {
            let due = sender.due_retransmissions(t + 30_000);
            assert_eq!(due.len(), 1);
            t += 30_000;
        }
        // Still nothing before the full 30 s window.
        assert!(sender.due_retransmissions(t + 29_999).is_empty());
    }

    #[test]
    fn nack_immediate_list_no_backoff_penalty() {
        let mut sender = TransactionSender::new(small_config());
        let id = txid(1);
        // 2 frames.
        sender
            .begin_transaction(id, &[1, 2, 3, 4, 5, 6], 0)
            .unwrap();
        let resent = sender.handle_nack(id, &[1], 500);
        assert_eq!(resent.len(), 1);
        assert_eq!(resent[0].seq, 1);
        // The NACK'd frame's clock restarted at 500: at t=2499 only seq 0
        // (sent at 0, initial RTO) is due — seq 1 is not. (Had the NACK
        // advanced the backoff, seq 1's RTO would be 4000 and it would
        // miss the 2500 deadline below too.)
        let due = sender.due_retransmissions(2_499);
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].seq, 0);
        // Exactly one initial RTO after the NACK-triggered send, seq 1 is
        // due again — while seq 0 is not (its clock restarted at 2499).
        let due = sender.due_retransmissions(2_500);
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].seq, 1);
    }

    #[test]
    fn nack_confirmed_and_out_of_range_ignored() {
        let mut sender = TransactionSender::new(small_config());
        let id = txid(1);
        // Single frame, fully confirmed.
        sender.begin_transaction(id, &[1, 2, 3, 4], 0).unwrap();
        assert!(sender.handle_ack(&ack(id, 0, 1), 100));
        // Confirmed seq, out-of-range seqs — nothing resurrected.
        assert!(sender.handle_nack(id, &[0, 4, 99], 200).is_empty());
    }

    #[test]
    fn nack_unknown_transaction_empty() {
        let mut sender = TransactionSender::new(small_config());
        assert!(sender.handle_nack(txid(9), &[0], 0).is_empty());
    }

    #[test]
    fn backpressure_pause_and_recovery() {
        let mut sender = TransactionSender::new(small_config()); // cap 1000
        assert!(!sender.is_paused());
        sender
            .begin_transaction(txid(1), &vec![0u8; 600], 0)
            .unwrap();
        assert!(!sender.is_paused());
        // Would exceed: 600 + 500 > 1000.
        let err = sender
            .begin_transaction(txid(2), &vec![0u8; 500], 0)
            .unwrap_err();
        assert_eq!(
            err,
            BeginError::Backpressured {
                unacked_bytes: 600,
                incoming_bytes: 500,
                cap_bytes: 1_000
            }
        );
        // Exactly at cap: paused.
        sender
            .begin_transaction(txid(3), &vec![0u8; 400], 0)
            .unwrap();
        assert!(sender.is_paused());
        // ACK releases copy 1 (600 B = 150 frames @ fs 4) → recovered.
        sender.handle_ack(&ack(txid(1), 149, 150), 100);
        assert!(!sender.is_paused());
        assert_eq!(sender.unacked_bytes(), 400);
        // New transactions fit again.
        sender
            .begin_transaction(txid(2), &vec![0u8; 600], 0)
            .unwrap();
    }

    #[test]
    fn declare_pending_gates_all_pending_and_only_pending() {
        let mut sender = TransactionSender::new(small_config());
        let id = txid(1);
        sender.begin_transaction(id, &[1, 2, 3, 4, 5], 0).unwrap();
        // Ungated before the declaration.
        assert!(!sender.is_awaiting_resume(id));
        assert!(!sender.due_retransmissions(130_000).is_empty());
        let entries = sender.declare_pending();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].transaction_id, id);
        // Gated: the retransmission clock skips it now.
        assert!(sender.is_awaiting_resume(id));
        assert!(sender.due_retransmissions(260_000).is_empty());
        // A transaction begun after the declaration is not gated.
        let id2 = txid(2);
        sender.begin_transaction(id2, &[9, 9], 0).unwrap();
        assert!(!sender.is_awaiting_resume(id2));
        // A repeated declaration re-arms the gate and re-lists the set.
        let entries = sender.declare_pending();
        assert_eq!(entries.len(), 2);
        assert!(sender.is_awaiting_resume(id));
        assert!(sender.is_awaiting_resume(id2));
    }

    #[test]
    fn verdict_resumed_lifts_gate_and_retransmits_unconfirmed_only() {
        let mut sender = TransactionSender::new(small_config());
        let id = txid(1);
        // 3 frames (10 bytes @ frame_size 4).
        sender
            .begin_transaction(id, &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10], 0)
            .unwrap();
        // Peer ACKed frame 0 before the disconnect: its copy is released.
        // (handle_ack returns whether the transaction completed — a partial
        // ACK returns false.)
        sender.handle_ack(&ack(id, 0, 1), 100);
        sender.declare_pending();
        let verdicts = sender.apply_resume_verdicts(
            &[ResumeStatusEntry {
                transaction_id: id,
                status: ResumeStatus::Resumed,
            }],
            1_000,
        );
        assert!(verdicts.failed.is_empty());
        // Only the unconfirmed frames go back out (seq 1 and 2).
        assert_eq!(verdicts.retransmit.len(), 2);
        assert_eq!(verdicts.retransmit[0].seq, 1);
        assert_eq!(verdicts.retransmit[1].seq, 2);
        // Gate lifted: the clock covers the transaction again.
        assert!(!sender.is_awaiting_resume(id));
        assert!(!sender.due_retransmissions(260_000).is_empty());
    }

    #[test]
    fn verdict_unknown_fails_the_transfer_and_releases_the_copy() {
        let mut sender = TransactionSender::new(small_config());
        let id = txid(1);
        sender.begin_transaction(id, &[1, 2], 0).unwrap();
        sender.declare_pending();
        let verdicts = sender.apply_resume_verdicts(
            &[ResumeStatusEntry {
                transaction_id: id,
                status: ResumeStatus::Unknown,
            }],
            1_000,
        );
        assert_eq!(verdicts.failed, vec![id]);
        assert!(verdicts.retransmit.is_empty());
        assert_eq!(sender.unacked_bytes(), 0);
        assert!(!sender.contains_transaction(id));
    }

    #[test]
    fn verdict_rejected_behaves_like_unknown() {
        let mut sender = TransactionSender::new(small_config());
        let id = txid(1);
        sender.begin_transaction(id, &[1, 2], 0).unwrap();
        sender.declare_pending();
        let verdicts = sender.apply_resume_verdicts(
            &[ResumeStatusEntry {
                transaction_id: id,
                status: ResumeStatus::Rejected,
            }],
            1_000,
        );
        assert_eq!(verdicts.failed, vec![id]);
        assert!(!sender.contains_transaction(id));
    }

    #[test]
    fn verdict_resumed_on_paused_transaction_lifts_gate_without_emitting() {
        let mut sender = TransactionSender::new(small_config());
        let id = txid(1);
        sender.begin_transaction(id, &[1, 2, 3, 4], 0).unwrap();
        assert!(sender.set_paused(id, true, 0));
        sender.declare_pending();
        let verdicts = sender.apply_resume_verdicts(
            &[ResumeStatusEntry {
                transaction_id: id,
                status: ResumeStatus::Resumed,
            }],
            1_000,
        );
        // The user pause dominates: gate lifted, nothing emitted.
        assert!(verdicts.retransmit.is_empty());
        assert!(!sender.is_awaiting_resume(id));
        assert!(sender.is_transaction_paused(id));
    }

    #[test]
    fn verdicts_for_foreign_ids_are_noops() {
        let mut sender = TransactionSender::new(small_config());
        let verdicts = sender.apply_resume_verdicts(
            &[
                ResumeStatusEntry {
                    transaction_id: txid(9),
                    status: ResumeStatus::Resumed,
                },
                ResumeStatusEntry {
                    transaction_id: txid(10),
                    status: ResumeStatus::Unknown,
                },
            ],
            0,
        );
        assert!(verdicts.retransmit.is_empty());
        assert!(verdicts.failed.is_empty());
    }
}
