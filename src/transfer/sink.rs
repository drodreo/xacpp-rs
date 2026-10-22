use crate::reliability::transaction::BeginError;
use crate::wire::error::XacppError;

use super::core::{BinarySendResult, CompletionCallback, SessionState, TransferSession, now_ms};

/// Opens a streaming transaction (docs/streams.md): declares
/// `total_len`, starts the send copy, returns the sink handle. `txid`:
/// `None` = library-generated; caller-supplied ids must be unique among
/// in-flight transactions and must not have been aborted earlier.
pub(crate) async fn open_binary_sink(
    session: &TransferSession,
    meta: crate::reliability::stream::BinaryMeta,
    txid: Option<[u8; 16]>,
) -> Result<crate::reliability::stream::BinarySink, XacppError> {
    let txid = txid.unwrap_or_else(|| uuid::Uuid::new_v4().into_bytes());
    {
        let mut st = session.core.state.lock().await;
        let now = now_ms();
        st.sender
            .open_transaction(txid, meta.total_len, now)
            .map_err(|e| match e {
                BeginError::DuplicateTransaction => {
                    XacppError::InvalidState("binary sink: transaction id already in flight".into())
                }
                BeginError::EmptyPayload => {
                    XacppError::InvalidState("binary sink: total_len must be non-zero".into())
                }
                other => XacppError::Internal(other.to_string()),
            })?;
    }
    Ok(crate::reliability::stream::BinarySink {
        session: session.clone(),
        txid,
    })
}

impl TransferSession {
    /// [`super::stream::BinarySink::write`]: append a chunk, parking while
    /// the backpressure cap is reached or the transaction is paused — and
    /// while the session is unwired (docs/streams.md: the consumer face
    /// has no "disconnect" concept; a parked write drains automatically at
    /// the next wire-up, byte progress carried by the per-frame copies and
    /// RESUME). Frames enter the sender state machine before the egress
    /// attempt, so even a write that races a dying connection is retried
    /// by the retransmission clock after re-wiring.
    ///
    /// Recovery surfaces: ACK releases (`wake_writers`), `resume()`,
    /// wire-up, and terminal cancellations (abort / RESUME rejected) — the
    /// latter fail the write once the loop re-checks the state machine.
    /// `Err` means the transaction is gone (cancelled/rejected) or the
    /// session itself was dropped.
    pub(crate) async fn sink_write(&self, txid: [u8; 16], chunk: &[u8]) -> Result<(), XacppError> {
        loop {
            // 0. Subscribe FIRST: any wakeup bump after this point (ACK
            //    release, resume, wire-up, cancellation) is never lost
            //    between the checks below and the wait.
            let mut wake = self.core.backpressure_watch.subscribe();
            // 1. Wiring check: unwired → park until the next wire-up (or
            //    session death). No NotConnected short-circuit. (Clone to a
            //    local first — a `match` scrutinee temporary would hold the
            //    std lock guard across the `.await`s below.)
            let wiring_opt = self.core.wiring.read().await.clone();
            let wiring = match wiring_opt {
                Some(wiring) => wiring,
                None => {
                    wake.changed().await.map_err(|_| XacppError::Closed)?;
                    continue;
                }
            };
            // 2. Append under the state lock (frames enter the state machine
            //    — their copies are held and their clocks armed).
            let frames = {
                let mut st = self.core.state.lock().await;
                let now = now_ms();
                // Park while paused (user intent) or gated awaiting the
                // receiver's resume verdict after a reconnect (recovery
                // discipline: nothing is sent for the transaction until
                // the verdict arrives). Both wake via backpressure_watch.
                if st.sender.is_transaction_paused(txid) || st.sender.is_awaiting_resume(txid) {
                    drop(st);
                    wake.changed().await.map_err(|_| XacppError::Closed)?;
                    continue;
                }
                match st.sender.append_chunk(txid, chunk, now) {
                    Ok(frames) => frames,
                    Err(BeginError::Backpressured { .. }) => {
                        drop(st);
                        wake.changed().await.map_err(|_| XacppError::Closed)?;
                        continue;
                    }
                    Err(e) => return Err(XacppError::InvalidState(e.to_string())),
                }
            };
            // 3. Egress is best-effort at this instant: on a write failure
            //    (connection dying right now) the frames are already in the
            //    state machine — the retransmission clock re-sends them once
            //    the session is wired again. The chunk is accepted.
            //    (The gate check above shares this lock critical section, so
            //    a chunk appended here is by definition not gated — nothing
            //    appended can overtake the RESUME handshake.)
            for frame in frames {
                if let Err(e) = wiring.write_data_frame(frame).await {
                    log::warn!("transfer: sink write raced a dead connection: {e}");
                    break;
                }
            }
            return Ok(());
        }
    }

    /// [`super::stream::BinarySink::finish`]: close the frame sequence,
    /// then wait for the transaction's terminal state (final ACK →
    /// confirmed; abort / RESUME rejection → unconfirmed). The wait spans
    /// disconnects — it resolves once the session is re-wired and the
    /// final ACK (or a cancellation) arrives; only session death ends it
    /// with an error.
    pub(crate) async fn sink_finish(&self, txid: [u8; 16]) -> Result<BinarySendResult, XacppError> {
        // 0. Fast path: the final ACK may already have released the copy
        //    before `finish` was called (e.g. the caller finished writing,
        //    the peer confirmed instantly, and `finish` is invoked
        //    afterwards). Not in pending = already released = confirmed —
        //    `finish` only ever acts on a successfully opened transaction,
        //    so absence from the pending table can only mean the final ACK
        //    released it.
        if !self
            .core
            .state
            .lock()
            .await
            .sender
            .contains_transaction(txid)
        {
            return Ok(BinarySendResult {
                transaction_id: txid,
                confirmed: true,
            });
        }
        // 1. Close the frame sequence (frames enter the state machine).
        let tail = {
            let mut st = self.core.state.lock().await;
            let now = now_ms();
            st.sender.finish_transaction(txid, now)
        };
        let tail = match tail {
            Ok(tail) => tail,
            Err(e) => {
                // The declared total was not written (short write / provider
                // mismatch): the peer must not be left waiting for bytes
                // that will never arrive. Announce the cancellation on the
                // wire (TRANSACTION_ABORT — same channel as the pause /
                // abort frames) and fail the finish. No TRANSACTION_COMPLETED
                // is emitted: the transaction was not completed.
                if let Err(abort_err) = self.sink_abort(txid).await {
                    log::warn!(
                        "transfer: short write could not be announced to the peer: {abort_err}"
                    );
                }
                return Err(XacppError::InvalidState(e.to_string()));
            }
        };
        // 2. Best-effort immediate flush of the tail; a failure (or an
        //    unwired session — park one beat and retry once) is harmless:
        //    the tail frames are in the state machine and the
        //    retransmission clock carries them over after re-wiring.
        //    Subscribe before the wiring check so a concurrent wire-up's
        //    wake cannot slip between check and wait.
        //    Recovery discipline (SPEC §5): a transaction gated awaiting
        //    the receiver's resume verdict SENDS NOTHING — the tail rides
        //    with the verdict-driven retransmission instead. Flushing it
        //    here (with the completion announcement behind it) would let
        //    both overtake the RESUME handshake on the new connection: the
        //    receiver would see a byte shortfall, end the stream as
        //    incomplete and drop the slot — and the verdict-driven
        //    retransmission would then land slotless.
        let gated = self.core.state.lock().await.sender.is_awaiting_resume(txid);
        let mut wake = self.core.backpressure_watch.subscribe();
        let mut waited_unwired = false;
        let mut tail_flushed = false;
        loop {
            if gated {
                // Gated: nothing goes out — the tail and the completion
                // announcement stay in the state machine until the verdict
                // arrives (`Resumed` retransmits them; `Unknown`/`Rejected`
                // closes by holder-side adjudication below).
                break;
            }
            let wiring = self.core.wiring.read().await.clone();
            match wiring {
                Some(wiring) => {
                    let mut flushed = true;
                    for frame in tail {
                        if let Err(e) = wiring.write_data_frame(frame).await {
                            log::warn!("transfer: sink finish raced a dead connection: {e}");
                            flushed = false;
                            break;
                        }
                    }
                    tail_flushed = flushed;
                    break;
                }
                None => {
                    if waited_unwired {
                        // Still unwired after a wait: leave the tail to the
                        // retransmission clock at the next wire-up.
                        break;
                    }
                    waited_unwired = true;
                    wake.changed().await.map_err(|_| XacppError::Closed)?;
                }
            }
        }
        // 2b. Declare the completion on the wire. The frame keeps its place
        //     behind the queued data frames: it travels the **data** FIFO
        //     channel, not the priority control channel — a completion that
        //     overtakes the tail would make the peer see a byte shortfall
        //     that does not exist. The wire frame type stays the control
        //     type `0x000B`; only the local scheduling path differs.
        //     Emitted after the tail flush and without waiting for the ACK.
        //     Only when the tail really went out. Best-effort: the
        //     completion itself is decided by the final ACK, and a peer
        //     that never sees this frame completes on the byte count.
        if tail_flushed {
            match crate::reliability::stream::completed_frame(txid) {
                Ok(frame) => {
                    if let Err(e) = self.write_ordered_control(frame).await {
                        log::warn!("transfer: could not announce the transaction completion: {e}");
                    }
                }
                Err(e) => {
                    log::warn!("transfer: could not encode the transaction completion: {e}");
                }
            }
        }
        // 3. Wait for the terminal state (same exact-once completion
        //    machinery as `send_binary`). No connection-close branch:
        //    disconnections do not resolve a finish (docs/streams.md).
        //    Session death drops the completion map and this oneshot's
        //    counterpart — `rx` then fails, which surfaces as `Closed`.
        let (tx, rx) = tokio::sync::oneshot::channel::<BinarySendResult>();
        // FnOnce: the firing path takes the callback out of the table, so
        // the oneshot sender moves straight into the closure — no lock.
        let callback: CompletionCallback = Box::new(move |result| {
            let _ = tx.send(result);
        });
        {
            let mut st = self.core.state.lock().await;
            st.completions.insert(txid, callback);
        }
        // Re-check after registering (the ACK may have landed between
        // step 0 and the registration; the completion map entry would
        // otherwise never fire). Not in pending = already released.
        if !self
            .core
            .state
            .lock()
            .await
            .sender
            .contains_transaction(txid)
        {
            // Consume our own registration slot (it will never fire).
            let _ = self.core.state.lock().await.completions.remove(&txid);
            return Ok(BinarySendResult {
                transaction_id: txid,
                confirmed: true,
            });
        }
        rx.await.map_err(|_| XacppError::Closed)
    }

    /// [`super::stream::BinarySink::pause`]: suspend new frames and
    /// retransmissions, then emit TRANSACTION_PAUSE (SPEC §5 MUST).
    pub(crate) async fn sink_pause(&self, txid: [u8; 16]) -> Result<(), XacppError> {
        let frame = {
            let mut st = self.core.state.lock().await;
            let now = now_ms();
            if !st.sender.set_paused(txid, true, now) {
                return Err(XacppError::InvalidState(
                    "binary sink pause: transaction not active".into(),
                ));
            }
            crate::reliability::stream::pause_frame(txid)?
        };
        // Transaction control frames keep their place behind the
        // transaction's own queued data frames (see `pause_frame`).
        self.write_ordered_control(frame).await
    }

    /// [`super::stream::BinarySink::resume`]: retransmission clock restarts
    /// (no explicit frame — resumption is implicit, SPEC §5); parked
    /// writes drain.
    pub(crate) async fn sink_resume(&self, txid: [u8; 16]) -> Result<(), XacppError> {
        let mut st = self.core.state.lock().await;
        let now = now_ms();
        if !st.sender.set_paused(txid, false, now) {
            return Err(XacppError::InvalidState(
                "binary sink resume: transaction not active".into(),
            ));
        }
        drop(st);
        // Wake parked writers (they re-check the paused flag and drain).
        self.wake_writers();
        Ok(())
    }

    /// [`super::stream::BinarySink::abort`]: release the copy, emit
    /// TRANSACTION_ABORT, fire the completion as unconfirmed.
    pub(crate) async fn sink_abort(&self, txid: [u8; 16]) -> Result<(), XacppError> {
        let (existed, completion) = {
            let mut st = self.core.state.lock().await;
            let existed = st.sender.abort_transaction(txid);
            (existed, st.completions.remove(&txid))
        };
        if !existed {
            return Err(XacppError::InvalidState(
                "binary sink abort: transaction not active".into(),
            ));
        }
        // Terminal state: the exact-once completion fires unconfirmed
        // (outside the state lock — callbacks may re-enter the session).
        if let Some(cb) = completion {
            cb(BinarySendResult {
                transaction_id: txid,
                confirmed: false,
            });
        }
        // Parked writers must wake to observe the cancellation (their next
        // append fails with UnknownTransaction → InvalidState).
        self.wake_writers();
        // FIFO, not the priority control channel: an abort that overtakes
        // the transaction's own first data frame would be ignored by a peer
        // that never saw the stream, and the late frame would open a stream
        // that never terminates.
        let frame = crate::reliability::stream::abort_frame(txid)?;
        self.write_ordered_control(frame).await
    }

    /// Bump the backpressure watch (parked writers re-check the cap). Safe
    /// inside the state lock — `send` never blocks.
    pub(super) fn wake_writers(&self) {
        self.core
            .backpressure_watch
            .send_modify(|v| *v = v.wrapping_add(1));
    }

    /// Write one control-type frame through the **data** FIFO channel, so
    /// it keeps its place behind the already-queued data frames.
    ///
    /// The control channel is drained with priority (SPEC §5 performance
    /// invariant 4) — a frame that must stay ordered relative to the data
    /// frames (any transaction control frame) goes through this path
    /// instead. The wire frame type is unchanged; only the scheduling
    /// differs.
    async fn write_ordered_control(
        &self,
        frame: crate::wire::frame::Frame,
    ) -> Result<(), XacppError> {
        let wiring = self
            .core
            .wiring
            .read()
            .await
            .clone()
            .ok_or(XacppError::NotConnected)?;
        wiring.write_data_frame(frame).await
    }

    /// Disconnect-path flow stall (docs/streams.md): mark every
    /// non-terminal stream `Stalled` and leave it otherwise untouched — no
    /// drain of `st.streams`, no chunk-channel close, no buffer loss. The
    /// holder's paused intent survives (the paused flag is orthogonal).
    /// The stream resumes from the same object at the next wire-up.
    pub(super) async fn stall_streams(&self) {
        let st = self.core.state.lock().await;
        for entry in st.streams.values() {
            entry.inner.set_stalled();
        }
    }
}

pub(super) fn backpressure_error(st: &SessionState) -> XacppError {
    XacppError::Application {
        code: "backpressure_stall".into(),
        message: format!(
            "unacknowledged bytes {} at cap {}: retry after ACKs release copies",
            st.sender.unacked_bytes(),
            st.sender.config().backpressure_cap_bytes,
        ),
    }
}
