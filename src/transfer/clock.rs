use std::sync::Arc;

use crate::carrier::XacppTransport;
use crate::wire::control::{
    AckEntry, HeartbeatPayload, NackEntry, NackPayload, ResumeRequestPayload,
};
use crate::wire::frame::FrameType;

use super::core::{TransferSession, TransferWiring, control_frame, now_ms};
use super::inbound::send_ack_entries;

/// On (re)connect: the holder declares its pending send transactions to
/// the receiver (recovery discipline — the verdict-driven continuation
/// model: the holder announces, the receiver adjudicates per transaction,
/// and only the receiver's verdict re-arms transmission). Declaring arms
/// the reconnect gate on every pending transaction (`awaiting_resume`):
/// until the verdict arrives, they send and retransmit nothing — blind
/// retransmission of retained copies across a reconnect is retired.
///
/// A holder with nothing pending declares nothing (no empty frame).
pub(crate) async fn send_resume_declaration(
    session: &TransferSession,
    wiring: &dyn TransferWiring,
) {
    let entries: Vec<AckEntry> = {
        let mut st = session.core.state.lock().await;
        st.sender.declare_pending(now_ms())
    };
    if entries.is_empty() {
        return;
    }
    send_resume_frame(wiring, entries).await;
}

/// Encode and write one RESUME declaration frame (control-priority
/// channel). Shared by the wire-up declaration and the verdict-timeout
/// retransmission.
async fn send_resume_frame(wiring: &dyn TransferWiring, entries: Vec<AckEntry>) {
    let bytes = ResumeRequestPayload { entries }.encode();
    if let Ok(bytes) = bytes {
        match control_frame(FrameType::Resume, bytes) {
            Ok(wire) => {
                if let Err(e) = wiring.write_control_frame(wire).await {
                    log::warn!("transfer: failed to send RESUME declaration: {e}");
                }
            }
            Err(e) => log::warn!("transfer: failed to encode RESUME declaration: {e}"),
        }
    }
}

/// The session's self-driven clock task: started at wire, aborted at
/// unwire, exits when the connection closes (watch) or when a tick
/// declares the peer silent (then tears the connection down through the
/// transport's own `disconnect`).
pub(super) async fn run_clock(session: TransferSession, transport: Arc<dyn XacppTransport>) {
    let tick_ms = session.core.config.tick_interval_ms.max(1);
    let mut interval = tokio::time::interval(std::time::Duration::from_millis(tick_ms));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut closed = transport.closed();
    loop {
        tokio::select! {
            _ = interval.tick() => {}
            // Connection dead (peer disconnect / write failure / local
            // disconnect): drop the wiring side and stall the receive-side
            // streams (docs/streams.md — the objects survive with the
            // session), then exit — the session's transaction state
            // survives for the next wire.
            _ = closed.changed() => {
                session.detach_connection().await;
                session.stall_streams().await;
                break;
            }
        }
        let now = now_ms();
        let silent = clock_tick(&session, transport.as_ref(), now).await;
        if !silent {
            session.wake_writers();
        }
        if silent {
            // Silent peer: self-driven dead-connection path — detach
            // (hooks off, wiring dropped), then tear the connection down
            // through the transport's `disconnect`.
            log::info!(
                "transfer: silent timeout after {} ms without peer frames",
                session.core.config.silent_timeout_ms
            );
            session.detach_connection().await;
            session.stall_streams().await;
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
            session.deliver(txid, payload).await;
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
    if session.core.binary_handler.read().await.is_none() {
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

/// One clock tick: batched ACK export, NACK feedback, retransmissions,
/// heartbeat, silent-peer check. Returns the silent-peer verdict (the
/// retention sweeps are retired — SPEC §5 revised: copy lifetime =
/// transaction lifetime; there is no expiry-driven completion release).
async fn clock_tick(session: &TransferSession, wiring: &dyn TransferWiring, now: u64) -> bool {
    let cfg = &session.core.config;
    let mut st = session.core.state.lock().await;

    // 1. Batched ACK export (§5: incremental ACKs every `ack_batch_frames`
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
        send_ack_entries(wiring, entries).await;
    }

    // 2. Receiver NACK feedback (§5: "Receivers ... return ACK (receipt) /
    //    NACK (gap list) feedback"). One frame per gapped transaction — a
    //    single entry's gap list is bounded by MAX_NACK_MISSING_ENTRIES so
    //    each frame fits the control cap.
    if now.saturating_sub(st.last_nack_ms) >= cfg.nack_feedback_interval_ms {
        st.last_nack_ms = now;
        for id in st.reassembler.open_transactions().collect::<Vec<_>>() {
            let missing = st
                .reassembler
                .missing_seqs(id, crate::reliability::reassemble::MAX_NACK_MISSING_ENTRIES);
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
                        if let Err(e) = wiring.write_control_frame(wire).await {
                            log::warn!("transfer: failed to send NACK: {e}");
                        }
                    }
                    Err(e) => log::warn!("transfer: failed to encode NACK: {e}"),
                },
                Err(e) => log::warn!("transfer: failed to encode NACK: {e}"),
            }
        }
    }

    // 3. Retransmissions (in-flight frames past their RTO). Exempt from the
    //    backpressure pause: the cap itself bounds retransmission storms.
    for wire_frame in st.sender.due_retransmissions(now) {
        if let Err(e) = wiring.write_data_frame(wire_frame).await {
            log::warn!("transfer: failed to send retransmission: {e}");
            return true;
        }
    }

    // 3b. RESUME declaration retransmission (§5 recovery discipline): a
    //     declaration that stays unanswered past the verdict timeout is
    //     retransmitted — the handshake is the sole recovery entry, so its
    //     own reliability is mandatory. Only still-gated transactions are
    //     re-declared (adjudicated ones are out of the pending set or out
    //     of the gate); each retransmission re-anchors its verdict clock.
    let redeclare = st
        .sender
        .timed_out_declarations(now, cfg.resume_verdict_timeout_ms);
    if !redeclare.is_empty() {
        log::debug!(
            "transfer: retransmitting RESUME declaration for {} unanswered transaction(s)",
            redeclare.len()
        );
        send_resume_frame(wiring, redeclare).await;
    }

    // 4. Heartbeat (§5 liveness; `u64::MAX` interval disables).
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
                    if let Err(e) = wiring.write_control_frame(wire).await {
                        log::warn!("transfer: failed to send heartbeat: {e}");
                    }
                }
                Err(e) => log::warn!("transfer: failed to encode heartbeat: {e}"),
            },
            Err(e) => log::warn!("transfer: failed to encode heartbeat: {e}"),
        }
    }

    // 5. Silent peer → dead connection.
    now.saturating_sub(st.last_peer_activity_ms) >= cfg.silent_timeout_ms
}
