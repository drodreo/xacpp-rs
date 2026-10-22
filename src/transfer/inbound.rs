use std::sync::Arc;

use crate::reliability::reassemble::ReceiveOutcome;
use crate::wire::control::{
    AckEntry, AckPayload, ErrorPayload, HeartbeatPayload, NackPayload, ResumeRequestPayload,
    ResumeResponsePayload, ResumeStatus, ResumeStatusEntry, TransactionAbortPayload,
    TransactionCompletedPayload, TransactionPausePayload, error_codes,
};
use crate::wire::frame::{Frame, FrameType};

use super::core::{
    BinarySendResult, CompletionCallback, SessionState, TransferSession, TransferWiring,
    control_frame, now_ms,
};

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
    let handler_present = session.core.binary_handler.read().await.is_some();
    let stream_present = session.core.stream_handler.read().await.is_some();
    let mut st = session.core.state.lock().await;
    st.last_peer_activity_ms = now;
    let mut delivered: Vec<([u8; 16], Vec<u8>)> = Vec::new();
    let mut completions: Vec<(CompletionCallback, BinarySendResult)> = Vec::new();
    let mut ack_progressed = false;
    let mut opened_stream: Option<Arc<crate::reliability::stream::StreamInner>> = None;

    match frame.frame_type {
        // The envelope channel never reaches the transfer layer (the
        // reader forwards `DATA_JSON` payloads to the envelope dispatch
        // before this dispatch point); defensive no-op.
        FrameType::DataJson => {}
        FrameType::DataBinary => {
            opened_stream = on_data_frame(
                &mut st,
                frame,
                wiring,
                &mut delivered,
                handler_present,
                stream_present,
                now,
            )
            .await
        }
        FrameType::Ack => {
            on_ack_frame(&mut st, &frame, &mut completions, now);
            // ACKs release per-frame copies (streaming): parked writers
            // re-check the cap. notify_waiters is non-blocking — safe
            // inside the state lock.
            ack_progressed = true;
        }
        FrameType::Nack => on_nack_frame(&mut st, &frame, wiring, now).await,
        // Two directions, two frame types (§5): dispatch on frame_type,
        // never on payload shape.
        FrameType::Resume => on_resume_request_frame(&mut st, &frame, wiring, now).await,
        FrameType::ResumeResponse => {
            on_resume_response_frame(&mut st, &frame, wiring, &mut completions, now).await;
            // Verdicts release held copies (and may fail transactions
            // out): parked writers re-check the state machine.
            ack_progressed = true;
        }
        FrameType::TransactionPause => on_transaction_pause_frame(&mut st, &frame),
        FrameType::TransactionAbort => on_transaction_abort_frame(&mut st, &frame),
        FrameType::TransactionCompleted => on_transaction_completed_frame(&mut st, &frame),
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
    if ack_progressed {
        session.wake_writers();
    }
    // Deliver outside the state lock: callbacks may re-enter the session.
    if let Some(inner) = opened_stream
        && let Some(handler) = session.core.stream_handler.read().await.clone()
    {
        handler(crate::reliability::stream::BinaryStream { inner });
    }
    for (txid, payload) in delivered {
        session.deliver(txid, payload).await;
    }
    for (cb, result) in completions {
        cb(result);
    }
}

/// TRANSACTION_PAUSE (holder → receiver, SPEC §5): the named transaction's
/// stream becomes `Paused` (consumer-visible; connection liveness
/// unaffected). Streams for unknown transactions are logged and dropped.
fn on_transaction_pause_frame(st: &mut SessionState, frame: &Frame) {
    let payload = match TransactionPausePayload::decode(&frame.payload) {
        Ok(payload) => payload,
        Err(e) => {
            log::warn!("transfer: malformed TRANSACTION_PAUSE: {e}");
            return;
        }
    };
    if let Some(entry) = st.streams.get(&payload.transaction_id) {
        entry.inner.set_paused();
    } else {
        log::debug!("transfer: TRANSACTION_PAUSE for a non-streamed transaction (ignored)");
    }
}

/// TRANSACTION_ABORT (holder → receiver, SPEC §5): discard the partial
/// reassembly and terminate the stream as `Cancelled`. The aggregate surface
/// never sees the transaction (it was never complete).
fn on_transaction_abort_frame(st: &mut SessionState, frame: &Frame) {
    let payload = match TransactionAbortPayload::decode(&frame.payload) {
        Ok(payload) => payload,
        Err(e) => {
            log::warn!("transfer: malformed TRANSACTION_ABORT: {e}");
            return;
        }
    };
    let txid = payload.transaction_id;
    if let Some(entry) = st.streams.remove(&txid) {
        // Terminal flag first, then the entry's drop closes the chunk
        // channel by ownership.
        entry
            .inner
            .finish(crate::reliability::stream::StreamState::Cancelled);
    }
    st.stream_owned.remove(&txid);
    st.reassembler.discard(txid);
    st.pending_ack_frames.remove(&txid);
}

/// TRANSACTION_COMPLETED (holder → receiver): the sender declares the
/// transaction closed — every declared byte has been sent.
///
/// Three cases (SPEC §5):
///
/// 1. Slot present and the bytes add up → the transaction is complete: the
///    byte-count path owns the verdict (it already closed the stream);
///    a stream still open here is closed as `Complete`. Idempotent — the
///    first verdict wins, later frames are ignored.
/// 2. Slot present and the bytes are missing → the sender declared
///    completion but the byte count falls short: the stream ends
///    explicitly as `Incomplete` (never hangs), carrying the gap. This is
///    the "the sender says it is done, but the bytes are not there" case.
/// 3. No slot → the transaction already completed or already reached a
///    terminal state: ignored.
fn on_transaction_completed_frame(st: &mut SessionState, frame: &Frame) {
    let payload = match TransactionCompletedPayload::decode(&frame.payload) {
        Ok(payload) => payload,
        Err(e) => {
            log::warn!("transfer: malformed TRANSACTION_COMPLETED: {e}");
            return;
        }
    };
    let txid = payload.transaction_id;
    if !st.reassembler.contains_transaction(txid) {
        // Case 3: already completed / already terminal — idempotent ignore.
        log::debug!("transfer: TRANSACTION_COMPLETED for a settled transaction (ignored)");
        return;
    }
    if st.reassembler.is_complete(txid) {
        // Case 1: the bytes add up. The byte-count path already closed the
        // stream; close a still-open one as `Complete` (idempotent).
        if let Some(entry) = st.streams.remove(&txid) {
            entry
                .inner
                .finish(crate::reliability::stream::StreamState::Complete);
            st.stream_owned.remove(&txid);
        }
        return;
    }
    // Case 2: the declared completion arrived with bytes missing. End the
    // stream explicitly with the gap instead of waiting for bytes that
    // will never come.
    let (received, expected) = st
        .reassembler
        .byte_progress(txid)
        .unwrap_or((0, frame.total_len));
    log::warn!(
        "transfer: TRANSACTION_COMPLETED with missing bytes (tx {:?}): received {received} of {expected}",
        uuid::Uuid::from_bytes(txid),
    );
    if let Some(entry) = st.streams.remove(&txid) {
        entry.inner.finish_incomplete(received, expected);
    }
    st.stream_owned.remove(&txid);
    st.reassembler.discard(txid);
    st.pending_ack_frames.remove(&txid);
}

/// DATA_BINARY frame → reassembler. On completion: immediate final ACK
/// (§5: "a final ACK on completion") regardless of handler presence.
///
/// Receive-side streaming (docs/streams.md): when a stream handler is
/// registered, the first frame of a transaction opens a `BinaryStream`
/// (returned to the async caller, which hands it to the handler outside the
/// state lock) and every contiguous-prefix advance pushes its newly covered
/// frames into the stream. Streamed transactions belong to the stream
/// surface — on completion their slot is released without aggregate
/// delivery (no double delivery). Non-streamed transactions keep the exact
/// legacy semantics: the payload is taken for delivery only while a binary
/// handler is registered — without one the completed slot is retained until
/// the handler arrives (the clock path delivers it then, exactly once).
async fn on_data_frame(
    st: &mut SessionState,
    frame: Frame,
    wiring: &dyn TransferWiring,
    delivered: &mut Vec<([u8; 16], Vec<u8>)>,
    handler_present: bool,
    stream_present: bool,
    now: u64,
) -> Option<Arc<crate::reliability::stream::StreamInner>> {
    // First frame of a transaction with a stream handler registered → the
    // transaction becomes a stream (checked before the slot opens). Only
    // `seq == 0` can open: a slotless id with `seq > 0` is dropped by the
    // reassembler (SPEC §3/§5) and must not open a stream that would never
    // terminate.
    let opens_stream = stream_present
        && frame.seq == 0
        && !st.reassembler.contains_transaction(frame.transaction_id);
    let mut opened: Option<Arc<crate::reliability::stream::StreamInner>> = None;
    match st.reassembler.receive_frame(
        frame.transaction_id,
        frame.seq,
        frame.total_len,
        frame.payload.len(),
        &frame.payload,
        now,
        stream_present,
    ) {
        Ok(ReceiveOutcome::Accepted { complete, emitted }) => {
            *st.pending_ack_frames
                .entry(frame.transaction_id)
                .or_insert(0) += 1;
            if opens_stream {
                let (inner, chunk_tx) = crate::reliability::stream::StreamInner::new(
                    frame.transaction_id,
                    frame.total_len,
                );
                for (seq, bytes) in emitted {
                    let _ = chunk_tx.send(crate::reliability::stream::StreamChunk { seq, bytes });
                }
                st.streams.insert(
                    frame.transaction_id,
                    crate::transfer::core::StreamEntry {
                        inner: Arc::clone(&inner),
                        chunk_tx,
                    },
                );
                st.stream_owned.insert(frame.transaction_id);
                opened = Some(inner);
            } else if let Some(entry) = st.streams.get(&frame.transaction_id) {
                // Resumption after pause is implicit: the next data frame of
                // the transaction clears the paused flag (SPEC §5).
                entry.inner.clear_paused();
                for (seq, bytes) in emitted {
                    let _ = entry
                        .chunk_tx
                        .send(crate::reliability::stream::StreamChunk { seq, bytes });
                }
            }
            if complete {
                if let Some(entry) = st.reassembler.ack_entry(frame.transaction_id) {
                    send_ack_entries(wiring, [entry]).await;
                }
                st.pending_ack_frames.remove(&frame.transaction_id);
                if st.stream_owned.remove(&frame.transaction_id) {
                    // Stream-owned completion: close the stream, release the
                    // slot, no aggregate delivery. The entry's drop closes
                    // the chunk channel by ownership.
                    if let Some(entry) = st.streams.remove(&frame.transaction_id) {
                        entry
                            .inner
                            .finish(crate::reliability::stream::StreamState::Complete);
                    }
                    st.reassembler.take_completed(frame.transaction_id);
                } else if handler_present
                    && let Some(payload) = st.reassembler.take_completed(frame.transaction_id)
                {
                    delivered.push((frame.transaction_id, payload));
                }
            }
        }
        // Retransmission already stored: idempotent.
        Ok(ReceiveOutcome::Duplicate) => {}
        // Slotless id with `seq > 0` (SPEC §3/§5): a late retransmission
        // of a settled transaction or an unannounced stream. Dropped — no
        // slot, no ACK, no NACK, no reply: the holder's closure signal is
        // the RESUME verdict, never a data-frame reply.
        Ok(ReceiveOutcome::SlotlessDropped) => {
            log::debug!(
                "transfer: dropped slotless DATA_BINARY frame (tx {:?} seq {}) — not reassembled",
                uuid::Uuid::from_bytes(frame.transaction_id),
                frame.seq,
            );
        }
        Err(e) => {
            log::warn!(
                "transfer: rejected DATA_BINARY frame (tx {:?} seq {}): {}",
                uuid::Uuid::from_bytes(frame.transaction_id),
                frame.seq,
                e
            );
        }
    }
    opened
}

/// Encode and send an ACK control frame (control-priority channel).
pub(super) async fn send_ack_entries(
    wiring: &dyn TransferWiring,
    entries: impl IntoIterator<Item = AckEntry>,
) {
    let entries: Vec<AckEntry> = entries.into_iter().collect();
    if entries.is_empty() {
        return;
    }
    let bytes = AckPayload { entries }.encode();
    match bytes {
        Ok(bytes) => match control_frame(FrameType::Ack, bytes) {
            Ok(wire) => {
                if let Err(e) = wiring.write_control_frame(wire).await {
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

async fn on_nack_frame(
    st: &mut SessionState,
    frame: &Frame,
    wiring: &dyn TransferWiring,
    now: u64,
) {
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
            if let Err(e) = wiring.write_data_frame(wire_frame).await {
                log::warn!("transfer: failed to send NACK retransmission: {e}");
                return;
            }
        }
    }
}

/// RESUME (`0x0004`, holder → receiver, recovery discipline): the holder
/// declares its pending send transactions after a reconnect. We
/// adjudicate each declared txid against our own open inbound table (no
/// tombstones — with no terminal trace kept, a consumed-and-delivered
/// transaction and a never-seen one are indistinguishable) and answer
/// with ONE RESUME_RESPONSE carrying exactly one verdict per declared
/// entry:
///
/// - `Resumed` — the transaction has an **open incomplete slot** here:
///   the continuation instruction; the holder lifts its gate and
///   retransmits the unconfirmed frames (its own ACK bookkeeping is the
///   watermark).
/// - `Unknown` — everything else: an open-but-complete slot
///   (completed-but-undelivered — it awaits local consumption, needs
///   nothing more from the holder) or no slot at all. The holder closes
///   the transfer by its own adjudication (SPEC §5).
///
/// Malformed payloads are logged and dropped — never guessed at.
async fn on_resume_request_frame(
    st: &mut SessionState,
    frame: &Frame,
    wiring: &dyn TransferWiring,
    _now: u64,
) {
    let req = match ResumeRequestPayload::decode(&frame.payload) {
        Ok(req) => req,
        Err(e) => {
            log::error!("transfer: malformed RESUME request dropped: {e}");
            return;
        }
    };
    if req.entries.is_empty() {
        // Defensive: the holder declares only when it has pending sends,
        // so an empty declaration carries no question to answer.
        return;
    }
    let entries = req
        .entries
        .iter()
        .map(|entry| ResumeStatusEntry {
            transaction_id: entry.transaction_id,
            status: if st.reassembler.contains_transaction(entry.transaction_id)
                && !st.reassembler.is_complete(entry.transaction_id)
            {
                // Open incomplete slot: the continuation instruction.
                ResumeStatus::Resumed
            } else {
                // Completed-but-undelivered slot or no slot: unknown —
                // with no terminal trace kept, a consumed-and-delivered
                // transaction and a never-seen one are indistinguishable,
                // and a complete slot needs nothing more from the holder.
                ResumeStatus::Unknown
            },
        })
        .collect();
    // Cross-check the mirror image: an open inbound transaction that the
    // holder did NOT declare is one the holder no longer holds (aborted or
    // released while disconnected) — it can never complete now. Streams
    // close only on cancellation or completion, so cancel them here
    // (declared-or-abandoned is decided in this one critical section; the
    // declaration frame rides the priority control channel, so frames of
    // any transaction created after the declaration snapshot are processed
    // after this section and cannot be misjudged). Only INCOMPLETE slots
    // are cross-checked: a completed-but-undelivered slot needs nothing
    // more from the holder (it awaits local delivery), so the holder
    // rightly does not declare it.
    let abandoned: Vec<[u8; 16]> = st
        .reassembler
        .open_transactions()
        .filter(|id| {
            !st.reassembler.is_complete(*id) && !req.entries.iter().any(|e| e.transaction_id == *id)
        })
        .collect();
    for id in abandoned {
        st.reassembler.discard(id);
        if let Some(entry) = st.streams.remove(&id) {
            entry
                .inner
                .finish(crate::reliability::stream::StreamState::Cancelled);
        }
        st.stream_owned.remove(&id);
    }
    let response = ResumeResponsePayload { entries };
    match response.encode() {
        Ok(bytes) => match control_frame(FrameType::ResumeResponse, bytes) {
            Ok(wire) => {
                if let Err(e) = wiring.write_control_frame(wire).await {
                    log::warn!("transfer: failed to send RESUME response: {e}");
                }
            }
            Err(e) => log::warn!("transfer: failed to encode RESUME response: {e}"),
        },
        Err(e) => log::warn!("transfer: failed to encode RESUME response: {e}"),
    }
}

/// RESUME response (`0x0008`, receiver → holder, recovery discipline):
/// apply the receiver's verdicts to our pending sends. `Resumed` lifts
/// the reconnect gate and retransmits every unconfirmed frame;
/// `Unknown`/`Rejected` closes the transfer by **holder-side adjudication**
/// (SPEC §5): a transaction whose frames were all sent (tail frame plus
/// the completion announcement) ends **confirmed** — the copy is released
/// and any completion waiter fires with a confirmation terminal; a
/// transaction that still owes frames ends **failed** (failure terminal).
async fn on_resume_response_frame(
    st: &mut SessionState,
    frame: &Frame,
    wiring: &dyn TransferWiring,
    completions: &mut Vec<(CompletionCallback, BinarySendResult)>,
    now: u64,
) {
    let resp = match ResumeResponsePayload::decode(&frame.payload) {
        Ok(resp) => resp,
        Err(e) => {
            log::error!("transfer: malformed RESUME_RESPONSE dropped: {e}");
            return;
        }
    };
    let verdicts = st.sender.apply_resume_verdicts(&resp.entries, now);
    for wire_frame in &verdicts.retransmit {
        if let Err(e) = wiring.write_data_frame(wire_frame.clone()).await {
            log::warn!("transfer: failed to send resume retransmission: {e}");
            return;
        }
    }
    // Confirmed closure (adjudicated fully-sent): same exact-once channel
    // as the final ACK — the completion waiter fires confirmed.
    for txid in verdicts.confirmed {
        if let Some(cb) = st.completions.remove(&txid) {
            completions.push((
                cb,
                BinarySendResult {
                    transaction_id: txid,
                    confirmed: true,
                },
            ));
        }
    }
    // Failed closure (frames still owed): the failure terminal.
    for txid in verdicts.failed {
        if let Some(cb) = st.completions.remove(&txid) {
            completions.push((
                cb,
                BinarySendResult {
                    transaction_id: txid,
                    confirmed: false,
                },
            ));
        }
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
        // free-form, not parsed). Semantics (docs/streams.md): the
        // holder's copy reached a terminal release — the receive slot is
        // dropped AND a stream open for the transaction is cancelled.
        if let Some(id) = parse_hex_txid_prefix(&payload.message) {
            st.reassembler.discard(*id.as_bytes());
            if let Some(entry) = st.streams.remove(id.as_bytes()) {
                entry
                    .inner
                    .finish(crate::reliability::stream::StreamState::Cancelled);
            }
            st.stream_owned.remove(id.as_bytes());
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
