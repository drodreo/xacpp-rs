//! Transaction send-side management (SPEC §5 transfer reliability semantics).
//!
//! [`TransactionSender`] is the copy holder: it splits payloads into
//! `DATA_BINARY` working frames, retains the frame copies until the final
//! ACK (or copy retention expiry), computes retransmission lists on timeout
//! (exponential backoff) and on NACK, enforces the unacknowledged-bytes
//! backpressure cap, and answers RESUME requests from the holder's
//! perspective.
//!
//! Pure synchronous component with injected time: every method takes the
//! current instant in milliseconds (`now_ms`) — no internal timers. The
//! integration layer drives the clock and the egress. Peer-reported
//! numbers (`highest_contiguous_seq`, `received_frames`, NACK seqs) are
//! treated as adversarial: out-of-range values are clamped or ignored,
//! never trusted for indexing.

use std::collections::HashMap;

use crate::control::{AckEntry, ResumeStatus, ResumeStatusEntry};
use crate::frame::{Frame, FrameType};

/// Sender tunables (SPEC §5 RECOMMENDED defaults).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SenderConfig {
    /// Working-frame payload size (§4: implementations SHOULD use ~1 MiB
    /// `DATA_BINARY` working frames; hard ceiling 16 MiB). Values below 1
    /// are clamped to 1 at construction.
    pub frame_size: usize,
    /// Copy retention after the last peer progress (ms): expired copies
    /// answer RESUME with `rejected` (SPEC §5: RECOMMENDED 120 s).
    pub copy_retention_ms: u64,
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
            copy_retention_ms: 120_000,
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
    /// The transaction id is already held (active copy or expired
    /// tombstone). Transaction ids are globally unique UUIDs; reuse is a
    /// caller bug.
    #[error("transaction already registered")]
    DuplicateTransaction,
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
    frames: Vec<Frame>,
    states: Vec<FrameState>,
    bytes: u64,
    /// Highest contiguous progress reported by the peer (`0..=seq` all
    /// received); clamped to `frame_count - 1`.
    acked_contiguous: Option<u32>,
    /// Highest total-frames-received reported by the peer (sparse residue
    /// detector, §5).
    acked_received: u32,
    /// Last peer-driven progress instant (ACK / NACK / RESUME) — the copy
    /// retention clock. Sender-side retransmits do NOT refresh it (they
    /// are not peer activity).
    last_progress_ms: u64,
    /// `Some(expired_at)` once retention expired: frame bytes released,
    /// tombstone kept so future RESUME requests answer `rejected`.
    expired_at: Option<u64>,
}

impl PendingTxn {
    fn frame_count(&self) -> u32 {
        self.frames.len() as u32
    }

    /// Complete per §5: the peer's contiguous progress covers the final
    /// frame and it received every frame (no sparse residue). Tombstones
    /// (empty frames) never read complete.
    fn is_fully_acked(&self) -> bool {
        let last = self.frame_count().checked_sub(1);
        self.acked_contiguous == last && last.is_some() && self.acked_received >= self.frame_count()
    }
}

/// Holder's answer to a RESUME request batch.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ResumeAnswers {
    /// Per-entry statuses, in request order.
    pub statuses: Vec<ResumeStatusEntry>,
    /// Frames to retransmit (all frames beyond the receiver's contiguous
    /// progress for every `resumed` entry).
    pub retransmit: Vec<Frame>,
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
    /// Returns the frame sequence for the integration layer to write to
    /// egress, in order (`seq 0..n`). All frames are marked sent at
    /// `now_ms`.
    pub fn begin_transaction(
        &mut self,
        transaction_id: [u8; 16],
        payload: &[u8],
        now_ms: u64,
    ) -> Result<Vec<Frame>, BeginError> {
        if self.pending.contains_key(&transaction_id) {
            return Err(BeginError::DuplicateTransaction);
        }
        if payload.is_empty() {
            return Err(BeginError::EmptyPayload);
        }
        let len = payload.len() as u64;
        if len > u32::MAX as u64 {
            return Err(BeginError::PayloadTooLarge { len, max: u32::MAX });
        }
        let incoming = len;
        let unacked = self.unacked_bytes();
        if unacked + incoming > self.config.backpressure_cap_bytes {
            return Err(BeginError::Backpressured {
                unacked_bytes: unacked,
                incoming_bytes: incoming,
                cap_bytes: self.config.backpressure_cap_bytes,
            });
        }
        let total_len = len as u32;
        let frames: Vec<Frame> = payload
            .chunks(self.config.frame_size)
            .enumerate()
            .map(|(seq, chunk)| {
                Frame::new(
                    FrameType::DataBinary,
                    transaction_id,
                    total_len,
                    seq as u32,
                    chunk.to_vec(),
                )
            })
            .collect();
        let states = vec![
            FrameState {
                confirmed: false,
                last_sent_ms: now_ms,
                attempts: 0,
            };
            frames.len()
        ];
        let txn = PendingTxn {
            bytes: incoming,
            frames: frames.clone(),
            states,
            acked_contiguous: None,
            acked_received: 0,
            last_progress_ms: now_ms,
            expired_at: None,
        };
        self.pending.insert(transaction_id, txn);
        Ok(frames)
    }

    /// Apply one cumulative ACK entry; returns `true` when the entry
    /// completes the transaction and its copy was released.
    pub fn handle_ack(&mut self, entry: &AckEntry, now_ms: u64) -> bool {
        let Some(txn) = self.pending.get_mut(&entry.transaction_id) else {
            return false;
        };
        if txn.expired_at.is_some() {
            return false;
        }
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
        if txn.is_fully_acked() {
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
        if txn.expired_at.is_some() {
            return Vec::new();
        }
        txn.last_progress_ms = now_ms;
        let mut out = Vec::with_capacity(missing_seqs.len());
        for &seq in missing_seqs {
            let Some(state) = txn.states.get_mut(seq as usize) else {
                continue; // out-of-range seq: ignore
            };
            if state.confirmed {
                continue; // peer self-corrected; do not resurrect
            }
            state.last_sent_ms = now_ms;
            out.push(txn.frames[seq as usize].clone());
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
            if txn.expired_at.is_some() {
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
                    out.push(frame.clone());
                }
            }
        }
        out
    }

    /// Answer a RESUME request batch from the holder's perspective (§5):
    ///
    /// - active copy → `resumed`, retransmitting every frame beyond the
    ///   receiver's contiguous progress (its `received_frames` may hint at
    ///   sparse residue, but frame identities beyond `highest` are
    ///   unknowable to the holder — the safe superset is resent);
    /// - expired copy (retention elapsed, tombstoned by
    ///   [`Self::expire_copies`]) → `rejected` (receiver discards);
    /// - unknown transaction → `unknown`.
    ///
    /// A receiver reporting full progress releases the copy immediately
    /// (the final ACK may still be in flight; further retransmission is
    /// pointless).
    pub fn handle_resume_request(&mut self, entries: &[AckEntry], now_ms: u64) -> ResumeAnswers {
        let mut answers = ResumeAnswers::default();
        for entry in entries {
            let Some(txn) = self.pending.get_mut(&entry.transaction_id) else {
                answers.statuses.push(ResumeStatusEntry {
                    transaction_id: entry.transaction_id,
                    status: ResumeStatus::Unknown,
                });
                continue;
            };
            if txn.expired_at.is_some() {
                answers.statuses.push(ResumeStatusEntry {
                    transaction_id: entry.transaction_id,
                    status: ResumeStatus::Rejected,
                });
                continue;
            }
            txn.last_progress_ms = now_ms;
            txn.acked_contiguous = txn
                .acked_contiguous
                .map_or(Some(entry.highest_contiguous_seq), |h| {
                    Some(h.max(entry.highest_contiguous_seq))
                });
            txn.acked_received = txn.acked_received.max(entry.received_frames);
            // Clamp adversarial progress reports to the real frame range.
            let last = txn.frame_count().saturating_sub(1);
            txn.acked_contiguous = txn.acked_contiguous.map(|h| h.min(last));
            let fully_acked = txn.is_fully_acked();
            let from = (txn.acked_contiguous.unwrap_or(0) as usize + 1).min(txn.frames.len());
            let mut retransmit = Vec::new();
            if !fully_acked {
                for state in &mut txn.states[from..] {
                    state.last_sent_ms = now_ms; // retransmission scheduled now
                }
                retransmit = txn.frames[from..].to_vec();
            }
            // Peer already holds everything → release the copy.
            if fully_acked {
                self.pending.remove(&entry.transaction_id);
            }
            answers.statuses.push(ResumeStatusEntry {
                transaction_id: entry.transaction_id,
                status: ResumeStatus::Resumed,
            });
            answers.retransmit.extend(retransmit);
        }
        answers
    }

    /// Apply a RESUME response batch received from a peer we requested
    /// progress from. `rejected` answers release our held copies (§5:
    /// "Senders release transaction copies on final ACK, or on RESUME
    /// rejection by the peer"); returns the released transaction ids.
    pub fn handle_resume_response(&mut self, entries: &[ResumeStatusEntry]) -> Vec<[u8; 16]> {
        let mut released = Vec::new();
        for entry in entries {
            if entry.status == ResumeStatus::Rejected
                && self.pending.remove(&entry.transaction_id).is_some()
            {
                released.push(entry.transaction_id);
            }
        }
        released
    }

    /// Retention sweep: active copies whose last peer progress is older
    /// than `copy_retention_ms` are tombstoned (frame bytes released;
    /// they answer future RESUME with `rejected`); tombstones older than
    /// the same retention are dropped entirely. Returns the ids
    /// tombstoned by this sweep (the integration layer may notify peers
    /// with ERROR `transaction_expired`).
    pub fn expire_copies(&mut self, now_ms: u64) -> Vec<[u8; 16]> {
        let retention = self.config.copy_retention_ms;
        let ids: Vec<[u8; 16]> = self.pending.keys().copied().collect();
        let mut tombstoned = Vec::new();
        for id in ids {
            let Some(txn) = self.pending.get_mut(&id) else {
                continue;
            };
            if let Some(expired_at) = txn.expired_at {
                // Tombstone layer: drop once it is itself past retention.
                if now_ms.saturating_sub(expired_at) > retention {
                    self.pending.remove(&id);
                }
            } else if now_ms.saturating_sub(txn.last_progress_ms) > retention {
                txn.expired_at = Some(now_ms);
                txn.frames = Vec::new();
                txn.states = Vec::new();
                txn.bytes = 0;
                tombstoned.push(id);
            }
        }
        tombstoned
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
            copy_retention_ms: 120_000,
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
        // Partial progress: nothing released.
        assert!(!sender.handle_ack(&ack(id, 0, 1), 100));
        assert_eq!(sender.unacked_bytes(), 6);
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
        assert!(!sender.handle_ack(&ack(id, 2, 1), 100));
        assert_eq!(sender.unacked_bytes(), 6);
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
        // to the last frame but the residue count (1 < 2) blocks
        // completion.
        let id2 = txid(2);
        sender
            .begin_transaction(id2, &[1, 2, 3, 4, 5, 6], 0)
            .unwrap();
        assert!(!sender.handle_ack(&ack(id2, u32::MAX, 1), 100));
        assert_eq!(sender.unacked_bytes(), 6);
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
    fn resume_holder_resumed_lists_frames_beyond_progress() {
        let mut sender = TransactionSender::new(small_config());
        let id = txid(1);
        // 3 frames.
        sender
            .begin_transaction(id, &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10], 0)
            .unwrap();
        let answers = sender.handle_resume_request(&[ack(id, 0, 1)], 1_000);
        assert_eq!(answers.statuses.len(), 1);
        assert_eq!(answers.statuses[0].status, ResumeStatus::Resumed);
        assert_eq!(answers.statuses[0].transaction_id, id);
        assert_eq!(answers.retransmit.len(), 2);
        assert_eq!(answers.retransmit[0].seq, 1);
        assert_eq!(answers.retransmit[1].seq, 2);
    }

    #[test]
    fn resume_holder_unknown_for_foreign_ids() {
        let mut sender = TransactionSender::new(small_config());
        let answers = sender.handle_resume_request(&[ack(txid(9), 0, 0)], 0);
        assert_eq!(answers.statuses[0].status, ResumeStatus::Unknown);
        assert!(answers.retransmit.is_empty());
    }

    #[test]
    fn resume_holder_reports_full_progress_releases_copy() {
        let mut sender = TransactionSender::new(small_config());
        let id = txid(1);
        // 2 frames: partial progress leaves room for a retransmission.
        sender
            .begin_transaction(id, &[1, 2, 3, 4, 5, 6], 0)
            .unwrap();
        let answers = sender.handle_resume_request(&[ack(id, 0, 1)], 1_000);
        assert_eq!(answers.statuses[0].status, ResumeStatus::Resumed);
        assert_eq!(answers.retransmit.len(), 1);
        assert_eq!(answers.retransmit[0].seq, 1);
        // Peer now reports completion — copy released, no frames resent.
        let answers = sender.handle_resume_request(&[ack(id, 1, 2)], 2_000);
        assert_eq!(answers.retransmit.len(), 0);
        assert_eq!(sender.unacked_bytes(), 0);
        // Further requests see an unknown transaction.
        let answers = sender.handle_resume_request(&[ack(id, 1, 2)], 3_000);
        assert_eq!(answers.statuses[0].status, ResumeStatus::Unknown);
    }

    #[test]
    fn resume_holder_expired_copy_rejected() {
        let mut sender = TransactionSender::new(small_config());
        let id = txid(1);
        sender.begin_transaction(id, &[1, 2], 0).unwrap();
        // No progress ever; retention is 120 s.
        let tombstoned = sender.expire_copies(120_001);
        assert_eq!(tombstoned, vec![id]);
        assert_eq!(sender.unacked_bytes(), 0); // bytes released
        let answers = sender.handle_resume_request(&[ack(id, 0, 0)], 130_000);
        assert_eq!(answers.statuses[0].status, ResumeStatus::Rejected);
        assert!(answers.retransmit.is_empty());
        // Second sweep drops the tombstone entirely.
        let tombstoned = sender.expire_copies(240_002);
        assert!(tombstoned.is_empty());
        let answers = sender.handle_resume_request(&[ack(id, 0, 0)], 240_003);
        assert_eq!(answers.statuses[0].status, ResumeStatus::Unknown);
    }

    #[test]
    fn expire_ignores_recently_progressed_copies() {
        let mut sender = TransactionSender::new(small_config());
        let id = txid(1);
        // 2 frames: a partial ACK keeps the copy alive.
        sender
            .begin_transaction(id, &[1, 2, 3, 4, 5, 6], 0)
            .unwrap();
        sender.handle_ack(&ack(id, 0, 1), 100_000); // progress refresh
        let tombstoned = sender.expire_copies(120_001);
        assert!(tombstoned.is_empty());
        let tombstoned = sender.expire_copies(100_000 + 120_001);
        assert_eq!(tombstoned, vec![id]);
    }

    #[test]
    fn resume_response_rejected_releases_copy() {
        let mut sender = TransactionSender::new(small_config());
        let id = txid(1);
        sender.begin_transaction(id, &[1, 2], 0).unwrap();
        let released = sender.handle_resume_response(&[ResumeStatusEntry {
            transaction_id: id,
            status: ResumeStatus::Rejected,
        }]);
        assert_eq!(released, vec![id]);
        assert_eq!(sender.unacked_bytes(), 0);
        // Resumed / Unknown answers release nothing.
        sender.begin_transaction(id, &[1, 2], 0).unwrap();
        assert!(
            sender
                .handle_resume_response(&[ResumeStatusEntry {
                    transaction_id: id,
                    status: ResumeStatus::Resumed,
                }])
                .is_empty()
        );
        assert_eq!(sender.unacked_bytes(), 2);
    }

    #[test]
    fn tombstones_do_not_count_toward_backpressure() {
        let mut sender = TransactionSender::new(small_config()); // cap 1000
        let id = txid(1);
        sender.begin_transaction(id, &vec![0u8; 900], 0).unwrap();
        sender.expire_copies(120_001);
        assert_eq!(sender.unacked_bytes(), 0);
        assert!(!sender.is_paused());
        // The same id is still registered (tombstone): duplicate refused.
        assert_eq!(
            sender.begin_transaction(id, &[1], 0),
            Err(BeginError::DuplicateTransaction)
        );
        // A fresh transaction fits — tombstone bytes are not counted.
        sender
            .begin_transaction(txid(2), &vec![0u8; 900], 0)
            .unwrap();
    }

    #[test]
    fn retransmit_does_not_refresh_retention_clock() {
        let mut sender = TransactionSender::new(small_config());
        let id = txid(1);
        sender.begin_transaction(id, &[1, 2], 0).unwrap();
        // Lone sender keeps retrying with zero peer feedback…
        for t in 0..60u64 {
            sender.due_retransmissions(2_000 * (t + 1));
        }
        // …yet retention counts from the last peer progress (begin).
        let tombstoned = sender.expire_copies(120_001);
        assert_eq!(tombstoned, vec![id]);
    }
}
