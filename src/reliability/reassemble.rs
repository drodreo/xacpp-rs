//! Transaction reassembly (SPEC §3 reassembly criteria, §5 receive side).
//!
//! [`TransactionReassembler`] manages per-transaction slots: frames arrive
//! in any order (duplicates are ignored idempotently), the contiguous
//! prefix advances monotonically, completion requires both §3 criteria
//! (`seq` contiguous to the last frame AND `sum(frame_len) == total_len`),
//! and the assembled payload is handed out whole.
//!
//! Buffer discipline (§3): frames are stored as they actually arrive in a
//! `BTreeMap` — nothing is pre-allocated from the declared `total_len`.

use std::collections::{BTreeMap, HashMap};

use crate::wire::control::AckEntry;

/// Reassembly error — one frame or transaction rejected, the reassembler
/// itself stays usable (per-slot isolation).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ReassemblyError {
    /// The same transaction declared a different `total_len` than the one
    /// its slot was opened with.
    #[error("total_len conflict: first saw {first}, now {got}")]
    TotalLenConflict {
        /// `total_len` the slot was opened with.
        first: u32,
        /// Conflicting declaration on this frame.
        got: u32,
    },
    /// `frame_len` argument does not match the actual payload length.
    #[error("frame_len {declared} does not match payload length {actual}")]
    FrameLenMismatch {
        /// Declared frame length.
        declared: usize,
        /// Actual payload byte count.
        actual: usize,
    },
    /// A zero-byte transaction cannot exist on the wire (every frame
    /// carries at least one payload byte).
    #[error("total_len 0: empty transactions are not a valid wire form")]
    EmptyTotalLen,
    /// Storing this frame would exceed the transaction's declared
    /// `total_len` (hostile or corrupted peer).
    #[error("transaction would exceed declared total_len {total}")]
    TotalLenExceeded {
        /// Declared transaction size in bytes.
        total: u32,
    },
}

/// Outcome of feeding one frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReceiveOutcome {
    /// Frame stored; `complete` reports whether the §3 criteria now hold
    /// (deliver via [`TransactionReassembler::take_completed`]). `emitted`
    /// carries the frames newly covered by the contiguous prefix, in `seq`
    /// order — populated only when the caller asked for streaming emission
    /// (streaming consumers clone them out while the stored copy stays for
    /// aggregation / ACK bookkeeping).
    Accepted {
        /// Transaction became complete with this frame.
        complete: bool,
        /// Newly contiguous frames `(seq, bytes)`.
        emitted: Vec<(u32, Vec<u8>)>,
    },
    /// Same-`seq` frame already stored — duplicate (retransmission);
    /// ignored, counts and bytes unchanged.
    Duplicate,
    /// A data frame for a slotless transaction id with `seq > 0` (SPEC
    /// §3/§5: a late retransmission of a settled transaction, or an
    /// unannounced stream). Not reassembled — no slot is opened, nothing
    /// is answered: the holder's closure signal is the RESUME verdict,
    /// never a data-frame reply. Only `seq == 0` opens a slot.
    SlotlessDropped,
}

/// Per-transaction slot state.
#[derive(Debug)]
struct Slot {
    total_len: u32,
    /// `seq → payload`, stored as received (no pre-allocation from
    /// `total_len`).
    frames: BTreeMap<u32, Vec<u8>>,
    /// Distinct frames stored (`frames.len()` mirror for cheap export).
    received_frames: u32,
    /// Sum of stored payload lengths.
    received_bytes: u64,
    /// Highest `seq` such that all frames `0..=seq` are stored;
    /// `None` while frame 0 has not arrived.
    contiguous_prefix: Option<u32>,
}

impl Slot {
    /// Advance the contiguous prefix over newly-filled holes.
    fn advance_contiguous(&mut self) {
        let mut next = self.contiguous_prefix.map_or(0, |h| h + 1);
        while self.frames.contains_key(&next) {
            self.contiguous_prefix = Some(next);
            next += 1;
        }
    }

    /// §3 completion: contiguous prefix covers the last frame AND the
    /// stored byte count equals `total_len`. If bytes are complete but a
    /// hole existed beyond the prefix, the prefix could not cover the last
    /// frame — so byte equality plus prefix coverage is exact.
    fn is_complete(&self) -> bool {
        self.contiguous_prefix.is_some() && self.received_bytes == self.total_len as u64
    }
}

/// Receive-side transaction reassembler.
///
/// Pure synchronous component; all timing (staleness) is injected as
/// millisecond instants by the caller.
///
/// No terminal trace is kept (SPEC §5 v1.1 stateless kernel — no
/// tombstones): once a slot is released (completion delivery or explicit
/// `discard`) the txid simply has no slot. A txid reaches the receiver
/// only through the peer's explicit announcement (SPEC §8 causality
/// chain): a data frame for a slotless id (`seq > 0`) is NOT reassembled
/// — it is reported as [`ReceiveOutcome::SlotlessDropped`] and the caller
/// answers nothing (no ACK, no slot). The holder's closure signal is the
/// RESUME verdict, never a data-frame reply (SPEC §3/§5). Only `seq == 0`
/// (a transaction's first frame) opens a slot.
#[derive(Debug)]
pub struct TransactionReassembler {
    slots: HashMap<[u8; 16], Slot>,
}

impl Default for TransactionReassembler {
    fn default() -> Self {
        Self::new()
    }
}

impl TransactionReassembler {
    /// Create a reassembler. No tunables: the time-based sweep is retired
    /// (SPEC §5 v3 — slots live until completion or explicit `discard`).
    pub fn new() -> Self {
        Self {
            slots: HashMap::new(),
        }
    }

    /// Feed one frame into its transaction slot.
    ///
    /// `frame_len` is validated against `payload.len()` (defence against
    /// caller bugs; the wire-level check lives in the frame decoder).
    ///
    /// A slotless id with `seq > 0` is dropped ([`ReceiveOutcome::
    /// SlotlessDropped`], SPEC §3/§5) — only `seq == 0` opens a slot.
    ///
    /// `emit`: when `true`, the returned [`ReceiveOutcome::Accepted`]
    /// carries the frames newly covered by the contiguous prefix (streaming
    /// consumers); `false` skips the cloning cost (aggregate-only paths).
    /// `_now_ms` is retained for time-injection API uniformity (the retired
    /// stale-sweep was its only consumer).
    #[allow(clippy::too_many_arguments)]
    pub fn receive_frame(
        &mut self,
        transaction_id: [u8; 16],
        seq: u32,
        total_len: u32,
        frame_len: usize,
        payload: &[u8],
        _now_ms: u64,
        emit: bool,
    ) -> Result<ReceiveOutcome, ReassemblyError> {
        if frame_len != payload.len() {
            return Err(ReassemblyError::FrameLenMismatch {
                declared: frame_len,
                actual: payload.len(),
            });
        }
        if total_len == 0 {
            // A zero-byte transaction cannot exist on the wire; checked
            // before the slot is opened so no garbage slot is created.
            return Err(ReassemblyError::EmptyTotalLen);
        }
        // SPEC §3/§5: a txid reaches the receiver only through the peer's
        // explicit announcement (§8 causality chain) — by the time `seq == 0`
        // arrives. A data frame for a slotless id (`seq > 0`) is a late
        // retransmission of a settled transaction or an unannounced stream:
        // not reassembled, no slot opened, nothing answered (the holder's
        // closure signal is the RESUME verdict, never a data-frame reply).
        if seq > 0 && !self.slots.contains_key(&transaction_id) {
            return Ok(ReceiveOutcome::SlotlessDropped);
        }
        let previous_prefix_end = self
            .slots
            .get(&transaction_id)
            .and_then(|slot| slot.contiguous_prefix)
            .map_or(0, |h| h + 1);
        let slot = self.slots.entry(transaction_id).or_insert_with(|| Slot {
            total_len,
            frames: BTreeMap::new(),
            received_frames: 0,
            received_bytes: 0,
            contiguous_prefix: None,
        });
        if slot.total_len != total_len {
            return Err(ReassemblyError::TotalLenConflict {
                first: slot.total_len,
                got: total_len,
            });
        }
        // Duplicate (retransmission already stored): idempotent.
        if slot.frames.contains_key(&seq) {
            return Ok(ReceiveOutcome::Duplicate);
        }
        if slot.received_bytes + payload.len() as u64 > total_len as u64 {
            return Err(ReassemblyError::TotalLenExceeded { total: total_len });
        }
        slot.frames.insert(seq, payload.to_vec());
        slot.received_frames += 1;
        slot.received_bytes += payload.len() as u64;
        slot.advance_contiguous();
        let complete = slot.is_complete();
        let mut emitted = Vec::new();
        if emit {
            let new_prefix_end = slot
                .contiguous_prefix
                .map_or(previous_prefix_end.saturating_sub(1), |h| h + 1);
            for covered in previous_prefix_end..new_prefix_end {
                if let Some(bytes) = slot.frames.get(&covered) {
                    emitted.push((covered, bytes.clone()));
                }
            }
        }
        Ok(ReceiveOutcome::Accepted { complete, emitted })
    }

    /// Whether the transaction has an open (or completed, undelivered) slot.
    pub fn contains_transaction(&self, transaction_id: [u8; 16]) -> bool {
        self.slots.contains_key(&transaction_id)
    }

    /// Whether the §3 criteria currently hold for the transaction.
    pub fn is_complete(&self, transaction_id: [u8; 16]) -> bool {
        self.slots
            .get(&transaction_id)
            .is_some_and(Slot::is_complete)
    }

    /// Byte accounting of a transaction: the bytes stored so far and the
    /// declared total (`None` = no slot for this id).
    ///
    /// The gap `declared - received` is what a TRANSACTION_COMPLETED
    /// frame with missing bytes exposes to the consumer.
    pub fn byte_progress(&self, transaction_id: [u8; 16]) -> Option<(u64, u32)> {
        let slot = self.slots.get(&transaction_id)?;
        Some((slot.received_bytes, slot.total_len))
    }

    /// Hand out the assembled payload and release the slot.
    ///
    /// Returns `None` (slot retained) unless the transaction is complete;
    /// the bytes are the concatenation of frame payloads in `seq` order.
    pub fn take_completed(&mut self, transaction_id: [u8; 16]) -> Option<Vec<u8>> {
        if !self.slots.get(&transaction_id)?.is_complete() {
            return None;
        }
        let slot = self.slots.remove(&transaction_id)?;
        let mut out = Vec::with_capacity(slot.total_len as usize);
        for payload in slot.frames.values() {
            out.extend_from_slice(payload);
        }
        Some(out)
    }

    /// Export the ACK progress entry for a transaction (`None` = no slot).
    ///
    /// SPEC §5: a transaction whose received frames do not include a
    /// contiguous prefix from frame 0 exports **no** ACK entry — a
    /// cumulative `highest_contiguous_seq` cannot express "no prefix"
    /// (exporting 0 would falsely claim frame 0 received). Such
    /// transactions surface through NACK gap lists instead.
    pub fn ack_entry(&self, transaction_id: [u8; 16]) -> Option<AckEntry> {
        let slot = self.slots.get(&transaction_id)?;
        let highest_contiguous_seq = slot.contiguous_prefix?;
        Some(AckEntry {
            transaction_id,
            highest_contiguous_seq,
            received_frames: slot.received_frames,
        })
    }

    /// Known gap list for a transaction: missing seqs strictly between the
    /// contiguous prefix and the highest stored seq, ascending. Holes past
    /// the last stored frame are unknowable (frame sizes vary) and are not
    /// reported — the tail is covered by sender timeouts instead.
    ///
    /// The list is truncated to `max_entries` (callers bound it by the
    /// 16 KiB control-frame cap; see [`MAX_NACK_MISSING_ENTRIES`]).
    pub fn missing_seqs(&self, transaction_id: [u8; 16], max_entries: usize) -> Vec<u32> {
        let Some(slot) = self.slots.get(&transaction_id) else {
            return Vec::new();
        };
        let Some(&highest_stored) = slot.frames.keys().next_back() else {
            return Vec::new();
        };
        let prefix_end = slot.contiguous_prefix.map_or(0, |h| h + 1);
        // Candidate holes live in `prefix_end..highest_stored` (half-open:
        // the highest stored frame itself has arrived).
        (prefix_end..highest_stored)
            .filter(|seq| !slot.frames.contains_key(seq))
            .take(max_entries)
            .collect()
    }

    /// Ids of all transaction slots (open or completed-undelivered) — the
    /// integration layer drives periodic ACK emission over these, and the
    /// recovery cross-check filters them down to the incomplete ones.
    pub fn open_transactions(&self) -> impl Iterator<Item = [u8; 16]> + '_ {
        self.slots.keys().copied()
    }

    /// Drop the slot for `transaction_id` if present; returns `true` when a
    /// slot existed. Called when the transaction can never complete as far
    /// as this side is concerned: the recovery cross-check (the holder's
    /// RESUME declaration did not cover this still-open slot), a peer
    /// `ERROR transaction_expired`, or a local TRANSACTION_ABORT. There is
    /// no time-based sweep: slots live until they complete or are
    /// explicitly dropped (SPEC §5 revised — copy lifetime = transaction
    /// lifetime).
    pub fn discard(&mut self, transaction_id: [u8; 16]) -> bool {
        self.slots.remove(&transaction_id).is_some()
    }
}

/// Upper bound for a NACK missing-seq list so one entry still fits the
/// 16 KiB control payload: `1 (count) + 16 (id) + 2 (missing_count)` then
/// 4 bytes per seq.
pub const MAX_NACK_MISSING_ENTRIES: usize = (crate::wire::control::CONTROL_PAYLOAD_CAP - 19) / 4;

#[cfg(test)]
mod tests {
    use super::*;

    fn txid(seed: u8) -> [u8; 16] {
        let mut id = [0u8; 16];
        id[0] = seed;
        id[15] = seed;
        id
    }

    /// Chunk `payload` into fixed-size frames of one transaction.
    fn chunked(payload: &[u8], frame_size: usize) -> Vec<(u32, usize, Vec<u8>)> {
        payload
            .chunks(frame_size)
            .enumerate()
            .map(|(i, chunk)| (i as u32, chunk.len(), chunk.to_vec()))
            .collect()
    }

    /// Feed frames in the given order; assert final completion and byte
    /// equality of the delivered payload.
    fn feed_order_and_deliver(
        reassembler: &mut TransactionReassembler,
        id: [u8; 16],
        total_len: u32,
        frames: &[(u32, usize, Vec<u8>)],
        order: &[usize],
        expected: &[u8],
    ) {
        let mut completed = false;
        for &idx in order {
            let (seq, len, payload) = &frames[idx];
            let outcome = reassembler
                .receive_frame(id, *seq, total_len, *len, payload, 1_000, false)
                .expect("frame accepted");
            match outcome {
                ReceiveOutcome::Accepted { complete, .. } => completed |= complete,
                ReceiveOutcome::Duplicate => {}
                ReceiveOutcome::SlotlessDropped => {}
            }
        }
        assert!(completed, "transaction never completed");
        assert_eq!(reassembler.take_completed(id).as_deref(), Some(expected));
    }

    #[test]
    fn in_order_assembly() {
        let payload: Vec<u8> = (0..30u8).collect();
        let frames = chunked(&payload, 10);
        let mut r = TransactionReassembler::default();
        let id = txid(1);
        feed_order_and_deliver(&mut r, id, 30, &frames, &[0, 1, 2], &payload);
        assert!(!r.is_complete(id));
        assert!(r.open_transactions().next().is_none());
    }

    #[test]
    fn slotless_tail_frames_dropped_no_slot_no_answer() {
        let mut r = TransactionReassembler::default();
        let id = txid(7);
        let p = vec![3u8; 4];
        // SPEC §3/§5: a txid reaches the receiver only through the peer's
        // explicit announcement (§8 causality chain). Tail frames for a
        // slotless id are late retransmissions of a settled transaction or
        // an unannounced stream: dropped — no slot, no ACK entry, nothing
        // reassembled, nothing answered (the holder's closure signal is
        // the RESUME verdict, never a data-frame reply).
        for seq in [1u32, 2, 3] {
            assert_eq!(
                r.receive_frame(id, seq, 16, 4, &p, 100, false).unwrap(),
                ReceiveOutcome::SlotlessDropped,
                "slotless tail frame must be dropped"
            );
        }
        // Slotless rule: nothing opened a slot behind the drops.
        assert!(!r.contains_transaction(id));
        assert_eq!(r.ack_entry(id), None);
        assert!(r.open_transactions().next().is_none());
        // The frame that MAY open a slot is `seq == 0` — the announced
        // first frame; from there the transaction assembles normally.
        assert_eq!(
            r.receive_frame(id, 0, 16, 4, &p, 200, false).unwrap(),
            ReceiveOutcome::Accepted {
                complete: false,
                emitted: vec![]
            }
        );
        assert!(r.contains_transaction(id));
        // The earlier tail frames were NOT silently stored: byte counts
        // reflect only frame 0.
        let entry = r.ack_entry(id).unwrap();
        assert_eq!(entry.received_frames, 1);
        assert_eq!(entry.highest_contiguous_seq, 0);
    }

    #[test]
    fn out_of_order_all_permutations_of_three() {
        let payload: Vec<u8> = (0..12u8).collect();
        let frames = chunked(&payload, 4);
        // SPEC §3/§5: only `seq == 0` opens a slot. Tail frames arriving
        // before frame 0 are dropped slotless (the slotless rule: no slot,
        // no reassembly); from frame 0 on, the tail assembles in any order.
        let orders: [&[usize]; 6] = [
            &[0, 1, 2],
            &[0, 2, 1],
            &[1, 0, 2],
            &[1, 2, 0],
            &[2, 0, 1],
            &[2, 1, 0],
        ];
        for order in orders {
            let mut r = TransactionReassembler::default();
            let id = txid(2);
            let mut idx = 0;
            while order[idx] != 0 {
                let (seq, len, frame_payload) = &frames[order[idx]];
                assert_eq!(
                    r.receive_frame(id, *seq, 12, *len, frame_payload, 1_000, false)
                        .unwrap(),
                    ReceiveOutcome::SlotlessDropped
                );
                assert!(
                    !r.contains_transaction(id),
                    "a slotless tail frame must not open a slot"
                );
                idx += 1;
            }
            // Frame 0 opens the slot; the earlier-dropped tail frames are
            // late retransmissions now — replayed, they assemble in order.
            let mut replayed = order[idx..].to_vec();
            replayed.extend_from_slice(&order[..idx]);
            feed_order_and_deliver(&mut r, id, 12, &frames, &replayed, &payload);
        }
    }

    #[test]
    fn out_of_order_four_frames_interleaved() {
        let payload: Vec<u8> = (0..40u8).collect();
        let frames = chunked(&payload, 10);
        let mut r = TransactionReassembler::default();
        let id = txid(3);
        // Frame 2 arriving before frame 0: slotless, dropped (SPEC §3/§5).
        let (seq, len, frame_payload) = &frames[2];
        assert_eq!(
            r.receive_frame(id, *seq, 40, *len, frame_payload, 1_000, false)
                .unwrap(),
            ReceiveOutcome::SlotlessDropped
        );
        // Frame 0 opens the slot; 3 and 1 interleave, and the dropped
        // frame 2 arrives as a late retransmission to complete the set.
        feed_order_and_deliver(&mut r, id, 40, &frames, &[0, 3, 1, 2], &payload);
    }

    #[test]
    fn duplicate_frames_ignored() {
        let mut r = TransactionReassembler::default();
        let id = txid(4);
        let payload = vec![1u8; 4];
        assert_eq!(
            r.receive_frame(id, 0, 8, 4, &payload, 100, false).unwrap(),
            ReceiveOutcome::Accepted {
                complete: false,
                emitted: vec![]
            }
        );
        // Same seq again — ignored, no byte or count growth.
        assert_eq!(
            r.receive_frame(id, 0, 8, 4, &payload, 200, false).unwrap(),
            ReceiveOutcome::Duplicate
        );
        let entry = r.ack_entry(id).unwrap();
        assert_eq!(entry.received_frames, 1);
        assert_eq!(entry.highest_contiguous_seq, 0);
    }

    #[test]
    fn duplicate_of_complete_frame_stays_duplicate() {
        let mut r = TransactionReassembler::default();
        let id = txid(4);
        let p0 = vec![1u8; 4];
        let p1 = vec![2u8; 4];
        r.receive_frame(id, 0, 8, 4, &p0, 100, false).unwrap();
        r.receive_frame(id, 1, 8, 4, &p1, 100, false).unwrap();
        assert!(r.is_complete(id));
        // Retransmitted tail frame before the final ACK arrives.
        assert_eq!(
            r.receive_frame(id, 1, 8, 4, &p1, 150, false).unwrap(),
            ReceiveOutcome::Duplicate
        );
        assert_eq!(r.take_completed(id).unwrap(), [p0, p1].concat());
    }

    #[test]
    fn gap_detected_then_filled() {
        let mut r = TransactionReassembler::default();
        let id = txid(5);
        let p0 = vec![10u8; 4];
        let p1 = vec![11u8; 4];
        let p2 = vec![12u8; 4];
        r.receive_frame(id, 0, 12, 4, &p0, 100, false).unwrap();
        r.receive_frame(id, 2, 12, 4, &p2, 100, false).unwrap();
        let entry = r.ack_entry(id).unwrap();
        assert_eq!(entry.highest_contiguous_seq, 0);
        assert_eq!(entry.received_frames, 2);
        assert_eq!(r.missing_seqs(id, MAX_NACK_MISSING_ENTRIES), vec![1]);
        // Fill the gap — completes.
        let outcome = r.receive_frame(id, 1, 12, 4, &p1, 200, false).unwrap();
        assert_eq!(
            outcome,
            ReceiveOutcome::Accepted {
                complete: true,
                emitted: vec![]
            }
        );
        assert_eq!(r.take_completed(id).unwrap(), [p0, p1, p2].concat());
    }

    #[test]
    fn multiple_gaps_ascending() {
        let mut r = TransactionReassembler::default();
        let id = txid(6);
        let p = |b: u8| vec![b; 1];
        // Stored: 0, 3, 5 (total needs bytes only; contiguity is what matters).
        r.receive_frame(id, 0, 5, 1, &p(0), 100, false).unwrap();
        r.receive_frame(id, 3, 5, 1, &p(3), 100, false).unwrap();
        r.receive_frame(id, 5, 5, 1, &p(5), 100, false).unwrap();
        assert_eq!(r.missing_seqs(id, MAX_NACK_MISSING_ENTRIES), vec![1, 2, 4]);
    }

    #[test]
    fn missing_truncated_to_max_entries() {
        let mut r = TransactionReassembler::default();
        let id = txid(7);
        let p = |b: u8| vec![b; 1];
        // Stored: 0, 1, 4, 5 (total needs 6 bytes) → gaps at 2 and 3.
        for seq in [0u32, 1, 4, 5] {
            r.receive_frame(id, seq, 6, 1, &p(seq as u8), 100, false)
                .unwrap();
        }
        assert_eq!(r.missing_seqs(id, 1), vec![2]);
        assert_eq!(r.missing_seqs(id, 2), vec![2, 3]);
        assert_eq!(r.missing_seqs(id, MAX_NACK_MISSING_ENTRIES), vec![2, 3]);
    }

    #[test]
    fn no_prefix_gap_exports_no_ack_entry() {
        let mut r = TransactionReassembler::default();
        let id = txid(8);
        let p = vec![7u8; 4];
        // Frame 0 never arrived: slotless tail frames are dropped (SPEC
        // §3/§5) — no slot, so no contiguous prefix and no ACK entry (a
        // cumulative `highest_contiguous_seq` cannot express "no prefix";
        // h=0 would falsely claim frame 0 received). Nothing is
        // reassembled and nothing is answered.
        assert_eq!(
            r.receive_frame(id, 1, 12, 4, &p, 100, false).unwrap(),
            ReceiveOutcome::SlotlessDropped
        );
        assert_eq!(
            r.receive_frame(id, 2, 12, 4, &p, 100, false).unwrap(),
            ReceiveOutcome::SlotlessDropped
        );
        assert!(!r.contains_transaction(id));
        assert_eq!(r.ack_entry(id), None);
        assert_eq!(
            r.missing_seqs(id, MAX_NACK_MISSING_ENTRIES),
            Vec::<u32>::new()
        );
    }

    #[test]
    fn ack_entry_appears_once_prefix_forms() {
        let mut r = TransactionReassembler::default();
        let id = txid(10);
        let p = vec![7u8; 4];
        // Frame 0 opens the slot: the prefix spans 0 → entry exported
        // immediately with the true progress (h=0); the known gap list is
        // empty (seq 3 is an unknowable tail, not reported).
        r.receive_frame(id, 0, 16, 4, &p, 100, false).unwrap();
        let entry = r.ack_entry(id).unwrap();
        assert_eq!(entry.highest_contiguous_seq, 0);
        assert_eq!(entry.received_frames, 1);
        // Out-of-order tail frames fill holes without advancing the prefix
        // until the gap closes.
        r.receive_frame(id, 2, 16, 4, &p, 150, false).unwrap();
        assert_eq!(r.ack_entry(id).unwrap().highest_contiguous_seq, 0);
        // Frame 1 arrives → prefix spans 0..=2 → entry exported with the
        // advanced progress.
        r.receive_frame(id, 1, 16, 4, &p, 200, false).unwrap();
        let entry = r.ack_entry(id).unwrap();
        assert_eq!(entry.highest_contiguous_seq, 2);
        assert_eq!(entry.received_frames, 3);
        assert_eq!(
            r.missing_seqs(id, MAX_NACK_MISSING_ENTRIES),
            Vec::<u32>::new()
        );
        assert!(!r.is_complete(id)); // tail frame 3 still missing
    }

    #[test]
    fn tail_hole_not_reported() {
        let mut r = TransactionReassembler::default();
        let id = txid(9);
        let p = vec![9u8; 4];
        // Total needs 12 bytes (3 frames); only 0 and 1 arrived — the
        // missing tail frame is unknowable, not reported.
        r.receive_frame(id, 0, 12, 4, &p, 100, false).unwrap();
        r.receive_frame(id, 1, 12, 4, &p, 100, false).unwrap();
        assert_eq!(
            r.missing_seqs(id, MAX_NACK_MISSING_ENTRIES),
            Vec::<u32>::new()
        );
        assert!(!r.is_complete(id));
    }

    #[test]
    fn concurrent_transactions_isolated() {
        let mut r = TransactionReassembler::default();
        let a = txid(1);
        let b = txid(2);
        let pa = vec![1u8; 4];
        let pb = vec![2u8; 4];
        // Both slots open with their own seq-0 frame; transactions stay
        // isolated (byte counts, prefixes, completion per id).
        r.receive_frame(a, 0, 8, 4, &pa, 100, false).unwrap();
        r.receive_frame(b, 0, 8, 4, &pb, 100, false).unwrap();
        assert_eq!(r.open_transactions().count(), 2);
        r.receive_frame(a, 1, 8, 4, &pa, 150, false).unwrap();
        assert_eq!(r.take_completed(a).unwrap(), [pa.clone(), pa].concat());
        assert_eq!(r.open_transactions().count(), 1);
        // b completes independently — a's completion never touched it.
        r.receive_frame(b, 1, 8, 4, &pb, 150, false).unwrap();
        assert_eq!(r.take_completed(b).unwrap(), [pb.clone(), pb].concat());
        assert!(r.open_transactions().next().is_none());
    }

    #[test]
    fn interleaved_same_total_len_conflict_free() {
        let mut r = TransactionReassembler::default();
        let a = txid(1);
        let b = txid(2);
        let p = vec![5u8; 4];
        r.receive_frame(a, 0, 8, 4, &p, 100, false).unwrap();
        r.receive_frame(b, 0, 8, 4, &p, 100, false).unwrap();
        r.receive_frame(a, 1, 8, 4, &p, 150, false).unwrap();
        r.receive_frame(b, 1, 8, 4, &p, 150, false).unwrap();
        assert!(r.is_complete(a) && r.is_complete(b));
    }

    #[test]
    fn total_len_conflict_rejected() {
        let mut r = TransactionReassembler::default();
        let id = txid(1);
        let p = vec![1u8; 4];
        r.receive_frame(id, 0, 8, 4, &p, 100, false).unwrap();
        let err = r.receive_frame(id, 1, 12, 4, &p, 100, false).unwrap_err();
        assert_eq!(err, ReassemblyError::TotalLenConflict { first: 8, got: 12 });
    }

    #[test]
    fn frame_len_mismatch_rejected() {
        let mut r = TransactionReassembler::default();
        let err = r
            .receive_frame(txid(1), 0, 8, 4, &[1, 2, 3], 100, false)
            .unwrap_err();
        assert_eq!(
            err,
            ReassemblyError::FrameLenMismatch {
                declared: 4,
                actual: 3
            }
        );
    }

    #[test]
    fn exceeding_total_len_rejected() {
        let mut r = TransactionReassembler::default();
        let id = txid(1);
        let p4 = vec![1u8; 4];
        r.receive_frame(id, 0, 8, 4, &p4, 100, false).unwrap();
        r.receive_frame(id, 1, 8, 4, &p4, 100, false).unwrap();
        let err = r.receive_frame(id, 2, 8, 4, &p4, 100, false).unwrap_err();
        assert_eq!(err, ReassemblyError::TotalLenExceeded { total: 8 });
        // A single oversized frame on a newly opened slot is rejected too.
        let err = r
            .receive_frame(txid(2), 0, 4, 5, &[0; 5], 100, false)
            .unwrap_err();
        assert_eq!(err, ReassemblyError::TotalLenExceeded { total: 4 });
    }

    #[test]
    fn empty_total_len_rejected() {
        let mut r = TransactionReassembler::default();
        let err = r
            .receive_frame(txid(1), 0, 0, 0, &[], 100, false)
            .unwrap_err();
        assert_eq!(err, ReassemblyError::EmptyTotalLen);
    }

    #[test]
    fn take_incomplete_returns_none_and_keeps_slot() {
        let mut r = TransactionReassembler::default();
        let id = txid(1);
        let p = vec![1u8; 4];
        r.receive_frame(id, 0, 8, 4, &p, 100, false).unwrap();
        assert_eq!(r.take_completed(id), None);
        assert!(r.ack_entry(id).is_some());
    }

    #[test]
    fn no_time_based_sweep_slots_survive_long_idle() {
        // Retention sweep retired (SPEC §5 revised: copy lifetime =
        // transaction lifetime): slots are never dropped for idleness —
        // they live until completion or an explicit `discard`.
        let mut r = TransactionReassembler::default();
        let idle_id = txid(1);
        let active_id = txid(2);
        let done_id = txid(3);
        let p = vec![1u8; 4];
        r.receive_frame(idle_id, 0, 8, 4, &p, 0, false).unwrap();
        r.receive_frame(active_id, 0, 8, 4, &p, 0, false).unwrap();
        r.receive_frame(done_id, 0, 4, 4, &p, 0, false).unwrap();
        // Active one keeps receiving; the other goes quiet far beyond any
        // historical retention window — everything must still be there.
        r.receive_frame(active_id, 1, 8, 4, &p, 100_000, false)
            .unwrap();
        let open: std::collections::HashSet<[u8; 16]> = r.open_transactions().collect();
        assert!(open.contains(&idle_id), "quiet slot is never swept");
        assert!(open.contains(&active_id));
        assert!(r.ack_entry(idle_id).is_some());
        assert!(r.ack_entry(active_id).is_some());
        // Completed slot survives too (awaiting take_completed).
        assert!(r.ack_entry(done_id).is_some());
    }

    #[test]
    fn ack_entry_absent_for_unknown() {
        let r = TransactionReassembler::default();
        assert!(r.ack_entry(txid(42)).is_none());
        assert_eq!(r.missing_seqs(txid(42), 10), Vec::<u32>::new());
    }

    #[test]
    fn delivered_bytes_are_bit_exact_large() {
        let payload: Vec<u8> = (0..=255u8).cycle().take(100_000).collect();
        let frames = chunked(&payload, 8192);
        let mut r = TransactionReassembler::default();
        let id = txid(1);
        // Frame 0 opens the slot (SPEC §3/§5); every other frame arrives
        // in reverse order — worst case for contiguity bookkeeping.
        let mut order: Vec<usize> = (1..frames.len()).rev().collect();
        order.insert(0, 0);
        feed_order_and_deliver(&mut r, id, payload.len() as u32, &frames, &order, &payload);
    }
}
