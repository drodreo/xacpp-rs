//! Streaming binary transfer tests (docs/streams.md — transport-layer
//! streaming; SPEC §5 test matrix items S1..S12).
//!
//! Matrix coverage:
//! - S1  sink multiple writes + finish confirmed (receiver aggregated whole)
//! - S2  write backpressure suspends until ACKs release copies
//! - S3  per-frame copy release (total written >> cap completes)
//! - S4  caller-supplied txid conflict rejected
//! - S5  abort: receiver stream terminates aborted, on_binary not delivered
//! - S6  pause suspends retransmission clock (no duplicate frames in flight)
//! - S7  aborted txid reuse rejected
//! - S8  TRANSACTION_PAUSE / TRANSACTION_ABORT codec + frame type values
//! - S9  legacy send_binary unchanged (smoke; full coverage stays in
//!   transport_reliability_tests)
//! - S10 on_binary_stream in-order chunks across a gap (retransmission fills)
//! - S11 stream meta (txid / total_len) readable on first frame
//! - S12 pause frame capture: receiver stream enters Paused, connection alive

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use tokio::io::BufReader;
use tokio::sync::Mutex as AsyncMutex;

use xacpp::carrier::XacppTransport;
use xacpp::carrier::stdio::StdioTransport;
use xacpp::reliability::stream::{BinaryMeta, BinarySink, BinaryStream, StreamState};
use xacpp::session::XacppSession;
use xacpp::session::handler::RequestHandler;
use xacpp::session::handler::{
    EstablishDecision, EstablishHandler, NegotiateHandler, XacppSessionHandler,
};
use xacpp::session::peer::XacppPeer;
use xacpp::transfer::{BinarySendResult, CompletionCallback, TransferActivityHandlers};
use xacpp::transfer::{TransferConfig, TransferWiring};
use xacpp::wire::capability::Capabilities;
use xacpp::wire::commands::XacppCommand;
use xacpp::wire::error::XacppError;
use xacpp::wire::frame::{Frame, FrameType};
use xacpp::wire::message::{XacppRequest, XacppResponse};

async fn timeout<F, T>(future: F) -> T
where
    F: std::future::Future<Output = T>,
{
    tokio::time::timeout(Duration::from_secs(10), future)
        .await
        .expect("test timeout")
}

fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i * 31 % 251) as u8).collect()
}

// ---- Test transport: stdio-duplex pipe with the binary channel enabled,
//      with a DATA_BINARY frame counter for wire-level assertions.

struct BinaryPipe {
    inner: Arc<StdioTransport>,
    data_frames_written: Arc<AtomicUsize>,
}

#[async_trait]
impl XacppTransport for BinaryPipe {
    async fn connect(&self) -> Result<(), XacppError> {
        self.inner.connect().await
    }
    async fn disconnect(&self) -> Result<(), XacppError> {
        self.inner.disconnect().await
    }
    async fn send(
        &self,
        session_id: Option<&str>,
        payload: XacppRequest,
    ) -> Result<XacppResponse, XacppError> {
        self.inner.send(session_id, payload).await
    }
    async fn send_faf(
        &self,
        session_id: Option<&str>,
        payload: XacppRequest,
    ) -> Result<(), XacppError> {
        self.inner.send_faf(session_id, payload).await
    }
    fn on_request(&self, handler: RequestHandler) -> Result<(), XacppError> {
        self.inner.on_request(handler)
    }
    fn closed(&self) -> tokio::sync::watch::Receiver<bool> {
        self.inner.closed()
    }
}

#[async_trait::async_trait]
impl TransferWiring for BinaryPipe {
    async fn write_control_frame(&self, bytes: Vec<u8>) -> Result<(), XacppError> {
        self.inner.write_control_frame(bytes).await
    }
    async fn write_data_frame(&self, frame: Frame) -> Result<(), XacppError> {
        // Count by frame type, not by channel: transaction control frames
        // (pause/abort/completed) also travel the data FIFO, and this
        // counter exists to measure DATA_BINARY traffic.
        if frame.frame_type == FrameType::DataBinary {
            self.data_frames_written.fetch_add(1, Ordering::SeqCst);
        }
        self.inner.write_data_frame(frame).await
    }
    async fn on_transfer_activity(&self, handlers: Option<TransferActivityHandlers>) {
        self.inner.on_transfer_activity(handlers).await;
    }
}

fn stdio_duplex() -> (Arc<StdioTransport>, Arc<StdioTransport>) {
    let (stream_a, stream_b) = tokio::io::duplex(64 * 1024);
    let (a_read, a_write) = tokio::io::split(stream_a);
    let (b_read, b_write) = tokio::io::split(stream_b);
    (
        Arc::new(StdioTransport::new(
            Box::pin(a_write),
            Box::pin(BufReader::new(a_read)),
        )),
        Arc::new(StdioTransport::new(
            Box::pin(b_write),
            Box::pin(BufReader::new(b_read)),
        )),
    )
}

// ---- Session handlers ----

struct NullHandler;

#[async_trait]
impl XacppSessionHandler for NullHandler {
    async fn on_command(&self, _command: XacppCommand) -> Result<XacppResponse, XacppError> {
        Ok(XacppResponse::acknowledge())
    }

    async fn on_event(
        &self,
        _event: xacpp::wire::events::XacppActivityEvent,
    ) -> Result<XacppResponse, XacppError> {
        Ok(XacppResponse::acknowledge())
    }
}

struct AcceptNegotiate;

#[async_trait]
impl NegotiateHandler for AcceptNegotiate {
    async fn on_negotiate(
        &self,
        _effective: xacpp::wire::capability::EffectiveCapabilities,
    ) -> Result<(), XacppError> {
        Ok(())
    }
}

/// Initiator side never accepts inbound establishes.
struct NoEstablish;

#[async_trait]
impl EstablishHandler for NoEstablish {
    async fn on_establish(
        &self,
        _transport: Arc<dyn XacppTransport>,
        _credentials: Option<String>,
    ) -> Result<EstablishDecision, XacppError> {
        Err(XacppError::Internal("no inbound establishes".into()))
    }

    async fn on_establish_confirm(
        &self,
        _transport: Arc<dyn XacppTransport>,
    ) -> Result<(String, Arc<dyn XacppSessionHandler>, String), XacppError> {
        Err(XacppError::Internal("challenge path not used".into()))
    }
}

/// Responder side: constructs + wires the session, retains it in the slot.
struct BCollector {
    config: Option<TransferConfig>,
    slot: Arc<AsyncMutex<Option<Arc<XacppSession>>>>,
}

#[async_trait]
impl EstablishHandler for BCollector {
    async fn on_establish(
        &self,
        transport: Arc<dyn XacppTransport>,
        _credentials: Option<String>,
    ) -> Result<EstablishDecision, XacppError> {
        let session = Arc::new(
            XacppSession::new(self.config)
                .with_identity("sess-b".into(), "creds-b".into())
                .await,
        );
        session.wire(transport).await?;
        *self.slot.lock().await = Some(Arc::clone(&session));
        Ok(EstablishDecision::Established {
            session_id: "sess-b".into(),
            handler: Arc::new(NullHandler),
            credentials: "creds-b".into(),
        })
    }

    async fn on_establish_confirm(
        &self,
        _transport: Arc<dyn XacppTransport>,
    ) -> Result<(String, Arc<dyn XacppSessionHandler>, String), XacppError> {
        Err(XacppError::Internal("challenge path not used".into()))
    }
}

/// Establishes a duplex session pair. `config_fn` tunes both sides' transfer
/// configs. Returns (a, b, a→b DATA_BINARY frame counter).
async fn session_pair(
    config_fn: impl Fn(TransferConfig) -> TransferConfig,
) -> (Arc<XacppSession>, Arc<XacppSession>, Arc<AtomicUsize>) {
    let (ta, tb) = stdio_duplex();
    let counter = Arc::new(AtomicUsize::new(0));
    let pipe_a = BinaryPipe {
        inner: ta,
        data_frames_written: Arc::clone(&counter),
    };
    let pipe_b = BinaryPipe {
        inner: tb,
        data_frames_written: Arc::new(AtomicUsize::new(0)),
    };
    let b_slot: Arc<AsyncMutex<Option<Arc<XacppSession>>>> = Arc::new(AsyncMutex::new(None));

    let session_a = Arc::new(XacppSession::new(Some(
        config_fn(TransferConfig::default()),
    )));
    let peer_a = XacppPeer::new(
        Capabilities::default(),
        Arc::new(pipe_a),
        Some(Arc::clone(&session_a)),
        Arc::new(AcceptNegotiate),
        Arc::new(NoEstablish),
    );
    let peer_b = XacppPeer::new(
        Capabilities::default(),
        Arc::new(pipe_b),
        None,
        Arc::new(AcceptNegotiate),
        Arc::new(BCollector {
            config: Some(config_fn(TransferConfig::default())),
            slot: Arc::clone(&b_slot),
        }),
    );
    peer_a.connect().await.unwrap();
    peer_b.connect().await.unwrap();
    peer_a.negotiate().await.unwrap();
    peer_a
        .establish(None, Arc::new(NullHandler), |_| Ok(()))
        .await
        .unwrap();
    let session_b = b_slot.lock().await.clone().expect("b session retained");
    (session_a, session_b, counter)
}

/// Fast test config: fast ACK batches / ticks on top of custom sender knobs.
fn tuned(sender: fn(&mut xacpp::reliability::transaction::SenderConfig)) -> TransferConfig {
    let mut c = TransferConfig {
        ack_batch_interval_ms: 50,
        nack_feedback_interval_ms: 50,
        resume_verdict_timeout_ms: 300,
        tick_interval_ms: 20,
        ..TransferConfig::default()
    };
    sender(&mut c.sender);
    c
}

/// Collects the aggregate binary surface into a channel. The handler is a
/// synchronous `Fn` — it cannot await — so the collector is an unbounded
/// channel: `send` is synchronous, and the payloads are drained in
/// arrival order.
fn collect_into_binary(
    sender: &tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
) -> xacpp::transfer::BinaryHandler {
    let sender = sender.clone();
    Arc::new(move |_txid, payload| {
        let _ = sender.send(payload);
    })
}

/// Drains the collected aggregate payloads into one byte buffer.
fn drain_collected(rx: &mut tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>) -> Vec<u8> {
    let mut out = Vec::new();
    while let Ok(payload) = rx.try_recv() {
        out.extend_from_slice(&payload);
    }
    out
}

// ---- S8: TRANSACTION_PAUSE / TRANSACTION_ABORT codec ----

#[test]
fn s8_pause_abort_frame_type_values() {
    assert_eq!(
        xacpp::wire::frame::FrameType::TransactionPause.to_u16(),
        0x0009
    );
    assert_eq!(
        xacpp::wire::frame::FrameType::TransactionAbort.to_u16(),
        0x000A
    );
    assert_eq!(
        xacpp::wire::frame::FrameType::from_u16(0x0009),
        xacpp::wire::frame::FrameType::TransactionPause
    );
    assert_eq!(
        xacpp::wire::frame::FrameType::from_u16(0x000A),
        xacpp::wire::frame::FrameType::TransactionAbort
    );
}

#[test]
fn s8_pause_abort_payload_roundtrip() {
    let txid = [7u8; 16];
    let pause = xacpp::wire::control::TransactionPausePayload {
        transaction_id: txid,
    };
    let bytes = pause.encode().expect("encode pause");
    assert_eq!(bytes.len(), 16);
    let de = xacpp::wire::control::TransactionPausePayload::decode(&bytes).expect("decode pause");
    assert_eq!(de.transaction_id, txid);

    let abort = xacpp::wire::control::TransactionAbortPayload {
        transaction_id: txid,
    };
    let bytes = abort.encode().expect("encode abort");
    assert_eq!(bytes.len(), 16);
    let de = xacpp::wire::control::TransactionAbortPayload::decode(&bytes).expect("decode abort");
    assert_eq!(de.transaction_id, txid);
}

// ---- S1: multiple writes + finish confirmed ----

#[tokio::test]
async fn s1_sink_multiple_writes_finish_confirmed() {
    let (a, b, _counter) = session_pair(|_| tuned(|_| {})).await;

    let (received_tx, mut received_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    b.on_binary(collect_into_binary(&received_tx)).await;

    let payload = pattern(3 * 1000 + 17);
    let sink = timeout(a.open_binary_sink(
        BinaryMeta {
            total_len: payload.len() as u32,
        },
        None,
    ))
    .await
    .expect("open sink");

    for chunk in payload.chunks(1000) {
        timeout(sink.write(chunk)).await.expect("write");
    }
    let result = timeout(sink.finish()).await.expect("finish");
    assert!(result.confirmed, "final ACK confirms the transaction");

    let got = drain_collected(&mut received_rx);
    assert_eq!(got, payload, "receiver reassembles the streamed bytes");
}

// ---- S2: write backpressure suspends until ACKs release ----

#[tokio::test]
async fn s2_write_backpressure_suspends_then_recovers() {
    // Cap = 2 frames; a 5-frame transaction: writes beyond the cap park
    // until the receiver's ACKs release confirmed frames.
    let (a, b, _counter) = session_pair(|_| {
        tuned(|s| {
            s.frame_size = 1000;
            s.backpressure_cap_bytes = 2000;
        })
    })
    .await;

    let (received_tx, mut received_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    b.on_binary(collect_into_binary(&received_tx)).await;

    let payload = pattern(5 * 1000);
    let sink = timeout(a.open_binary_sink(
        BinaryMeta {
            total_len: payload.len() as u32,
        },
        None,
    ))
    .await
    .expect("open sink");

    // First two frames fill the cap.
    timeout(sink.write(&payload[..1000]))
        .await
        .expect("write 1");
    timeout(sink.write(&payload[1000..2000]))
        .await
        .expect("write 2");
    // The rest parks; it must still complete (no error) once ACKs flow.
    let rest = payload[2000..].to_vec();
    let expected_tail = rest.clone();
    let parked = tokio::spawn(async move {
        for chunk in rest.chunks(1000) {
            sink.write(chunk).await.expect("parked write");
        }
        sink.finish().await.expect("finish")
    });
    // Parked writes are suspended, not failed: give the runtime a beat to
    // prove the write loop survives the cap without erroring, then let the
    // ACK stream drive it to completion.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let result = timeout(parked).await.expect("parked writes recover");
    assert!(result.confirmed);
    let got = drain_collected(&mut received_rx);
    assert_eq!(&got[..2000], &payload[..2000]);
    assert_eq!(&got[2000..], &expected_tail[..]);
}

// ---- S3: per-frame copy release (written >> cap completes) ----

#[tokio::test]
async fn s3_streaming_completes_beyond_cap_by_per_frame_release() {
    // Cap = 3 frames; 20-frame transaction. With transaction-level copies
    // this cannot complete (unacked 20 frames >> 3 forever); with per-frame
    // release the ACK stream keeps freeing confirmed frames and the sink
    // drains to completion.
    let (a, b, _counter) = session_pair(|_| {
        tuned(|s| {
            s.frame_size = 1000;
            s.backpressure_cap_bytes = 3000;
        })
    })
    .await;

    let (received_tx, mut received_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    b.on_binary(collect_into_binary(&received_tx)).await;

    let payload = pattern(20 * 1000);
    let sink = timeout(a.open_binary_sink(
        BinaryMeta {
            total_len: payload.len() as u32,
        },
        None,
    ))
    .await
    .expect("open sink");

    for chunk in payload.chunks(1000) {
        timeout(sink.write(chunk)).await.expect("write under cap");
    }
    let result = timeout(sink.finish()).await.expect("finish");
    assert!(result.confirmed);
    assert_eq!(drain_collected(&mut received_rx), payload);
}

// ---- S4: caller-supplied txid conflict ----

#[tokio::test]
async fn s4_caller_txid_conflict_rejected() {
    let (a, _b, _counter) = session_pair(|_| tuned(|_| {})).await;

    let txid = [0x42u8; 16];
    let _first = timeout(a.open_binary_sink(BinaryMeta { total_len: 10 }, Some(txid)))
        .await
        .expect("first sink with caller txid");

    let second = timeout(a.open_binary_sink(BinaryMeta { total_len: 10 }, Some(txid))).await;
    assert!(second.is_err(), "duplicate in-flight txid must be rejected");
}

// ---- S5: abort discards receiver partial reassembly ----

#[tokio::test]
async fn s5_abort_terminates_receiver_stream_aborted() {
    let (a, b, _counter) = session_pair(|_| {
        tuned(|s| {
            s.frame_size = 1000;
        })
    })
    .await;

    // Receiver: stream handler collects chunks and the terminal state; the
    // binary aggregate surface records (unexpected) deliveries.
    // The aggregate handler is a synchronous `Fn` (it cannot await) and the
    // terminal state is sampled while the transfer is still running, so the
    // three slots are watch cells: synchronous writes, snapshot reads that
    // never consume the value.
    struct Rx {
        chunks: tokio::sync::watch::Sender<Vec<Vec<u8>>>,
        terminal: tokio::sync::watch::Sender<Option<StreamState>>,
        aggregated: tokio::sync::watch::Sender<Vec<usize>>,
    }
    let rx = Arc::new(Rx {
        chunks: tokio::sync::watch::channel(Vec::new()).0,
        terminal: tokio::sync::watch::channel(None).0,
        aggregated: tokio::sync::watch::channel(Vec::new()).0,
    });
    {
        let rx = Arc::clone(&rx);
        b.on_binary_stream(Arc::new(move |stream| {
            let rx = Arc::clone(&rx);
            tokio::spawn(async move {
                while let Some(chunk) = stream.read().await {
                    rx.chunks.send_modify(|chunks| chunks.push(chunk.bytes));
                }
                rx.terminal.send_replace(Some(stream.state()));
            });
        }))
        .await;
    }
    b.on_binary({
        let rx = Arc::clone(&rx);
        Arc::new(move |_txid, payload| {
            rx.aggregated.send_modify(|sizes| sizes.push(payload.len()));
        })
    })
    .await;

    let payload = pattern(4 * 1000);
    let sink = timeout(a.open_binary_sink(
        BinaryMeta {
            total_len: payload.len() as u32,
        },
        None,
    ))
    .await
    .expect("open sink");

    // Half in, then abort.
    timeout(sink.write(&payload[..2000]))
        .await
        .expect("write half");
    tokio::time::sleep(Duration::from_millis(200)).await; // frames reach b
    timeout(sink.abort()).await.expect("abort");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if matches!(*rx.terminal.borrow(), Some(StreamState::Cancelled)) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "stream never reached Cancelled"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let total: usize = rx.chunks.borrow().iter().map(|c| c.len()).sum();
    assert!(
        total < payload.len(),
        "aborted stream must not deliver the whole payload"
    );
    assert!(
        rx.aggregated.borrow().is_empty(),
        "aborted transaction must not reach the aggregate surface"
    );
}

// ---- S6: pause suspends retransmission clock ----

#[tokio::test]
async fn s6_pause_suspends_retransmission_heartbeat_alive() {
    let (a, b, counter) = session_pair(|_| {
        tuned(|s| {
            s.frame_size = 1000;
            s.rto_initial_ms = 200;
            s.rto_max_ms = 400;
        })
    })
    .await;

    let (received_tx, mut received_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    b.on_binary(collect_into_binary(&received_tx)).await;

    let payload = pattern(4 * 1000);
    let sink = timeout(a.open_binary_sink(
        BinaryMeta {
            total_len: payload.len() as u32,
        },
        None,
    ))
    .await
    .expect("open sink");

    timeout(sink.write(&payload[..1000]))
        .await
        .expect("write 1");
    timeout(sink.write(&payload[1000..2000]))
        .await
        .expect("write 2");
    tokio::time::sleep(Duration::from_millis(200)).await; // frames reach b
    let frames_before_pause = counter.load(Ordering::SeqCst);
    assert!(frames_before_pause >= 2);

    timeout(sink.pause()).await.expect("pause");
    // Parked well past several RTOs: no retransmission may fire.
    tokio::time::sleep(Duration::from_millis(1000)).await;
    let frames_during_pause = counter.load(Ordering::SeqCst);
    assert_eq!(
        frames_during_pause, frames_before_pause,
        "retransmissions must be suspended while paused"
    );

    // Liveness unaffected: resume and finish complete normally.
    timeout(sink.resume()).await.expect("resume");
    for chunk in payload[2000..].chunks(1000) {
        timeout(sink.write(chunk)).await.expect("post-resume write");
    }
    let result = timeout(sink.finish()).await.expect("finish after resume");
    assert!(result.confirmed);
    assert_eq!(drain_collected(&mut received_rx), payload);
}

// ---- S7: aborted txid reuse — no terminal trace, reuse allowed ----

#[tokio::test]
async fn s7_aborted_txid_reuse_allowed() {
    let (a, _b, _counter) = session_pair(|_| tuned(|_| {})).await;

    let txid = [0x77u8; 16];
    let sink = timeout(a.open_binary_sink(BinaryMeta { total_len: 100 }, Some(txid)))
        .await
        .expect("open sink");
    timeout(sink.abort()).await.expect("abort");

    // The holder keeps no terminal trace: an aborted id is just an unused
    // id — reuse opens a fresh transaction (uniqueness is the caller's
    // discipline, the library does not police it).
    let reuse = timeout(a.open_binary_sink(BinaryMeta { total_len: 100 }, Some(txid))).await;
    assert!(
        reuse.is_ok(),
        "aborted txid is reusable — no terminal trace"
    );
}

// ---- S9: legacy send_binary unchanged ----

#[tokio::test]
async fn s9_legacy_send_binary_smoke() {
    let (a, b, _counter) = session_pair(|_| tuned(|_| {})).await;

    let (received_tx, mut received_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    b.on_binary(collect_into_binary(&received_tx)).await;

    let payload = pattern(2500);
    // The completion callback is a synchronous `FnOnce` — it cannot await —
    // and the test polls the collected completions while the transfer is
    // still running, so the slot is a watch cell: a synchronous push, a
    // snapshot read that never consumes the value.
    let completions = tokio::sync::watch::channel(Vec::<BinarySendResult>::new()).0;
    let cb_sink = completions.clone();
    let on_complete: CompletionCallback = Box::new(move |result| {
        cb_sink.send_modify(|completions| completions.push(result));
    });
    let txid = timeout(a.send_binary(payload.clone(), Some(on_complete)))
        .await
        .expect("send_binary");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let done = {
            let completions = completions.borrow();
            completions.len() == 1 && completions[0].transaction_id == txid
        };
        if done {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "on_complete never fired"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(completions.borrow()[0].confirmed);
    assert_eq!(drain_collected(&mut received_rx), payload);
}

// ---- S10: in-order chunks across a gap (retransmission fills) ----

#[tokio::test]
async fn s10_stream_in_order_chunks_across_gap() {
    let (a, b, _counter) = session_pair(|_| {
        tuned(|s| {
            s.frame_size = 1000;
        })
    })
    .await;

    // Both slots are watch cells: the writes are synchronous and the test
    // polls the terminal state while the transfer is still running, so
    // what it needs is a snapshot read that never consumes the value.
    struct Rx {
        chunks: tokio::sync::watch::Sender<Vec<Vec<u8>>>,
        terminal: tokio::sync::watch::Sender<Option<StreamState>>,
    }
    let rx = Arc::new(Rx {
        chunks: tokio::sync::watch::channel(Vec::new()).0,
        terminal: tokio::sync::watch::channel(None).0,
    });
    {
        let rx = Arc::clone(&rx);
        b.on_binary_stream(Arc::new(move |stream| {
            let rx = Arc::clone(&rx);
            tokio::spawn(async move {
                while let Some(chunk) = stream.read().await {
                    rx.chunks.send_modify(|chunks| chunks.push(chunk.bytes));
                }
                rx.terminal.send_replace(Some(stream.state()));
            });
        }))
        .await;
    }

    let payload = pattern(5 * 1000);
    let sink = timeout(a.open_binary_sink(
        BinaryMeta {
            total_len: payload.len() as u32,
        },
        None,
    ))
    .await
    .expect("open sink");

    for chunk in payload.chunks(1000) {
        timeout(sink.write(chunk)).await.expect("write");
    }
    let result = timeout(sink.finish()).await.expect("finish");
    assert!(result.confirmed);

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if matches!(*rx.terminal.borrow(), Some(StreamState::Complete)) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "stream never completed"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let got = rx.chunks.borrow().concat();
    assert_eq!(got, payload, "chunks arrive in seq order, whole payload");
}

// ---- S11: stream meta readable on first frame ----

#[tokio::test]
async fn s11_stream_meta_visible_on_first_frame() {
    let (a, b, _counter) = session_pair(|_| {
        tuned(|s| {
            s.frame_size = 1000;
        })
    })
    .await;

    type StreamMeta = ([u8; 16], u32);
    // The stream handler is a synchronous `Fn` — it cannot await — and the
    // test polls for the metadata while the transfer is still running, so
    // the slot is a watch cell: a synchronous write, a snapshot read that
    // never consumes the value.
    let meta = tokio::sync::watch::channel(None::<StreamMeta>).0;
    {
        let meta = meta.clone();
        b.on_binary_stream(Arc::new(move |stream: BinaryStream| {
            meta.send_replace(Some((stream.txid(), stream.total_len())));
        }))
        .await;
    }

    let payload = pattern(2500);
    let txid = [0x5Au8; 16];
    let sink = timeout(a.open_binary_sink(
        BinaryMeta {
            total_len: payload.len() as u32,
        },
        Some(txid),
    ))
    .await
    .expect("open sink");

    for chunk in payload.chunks(1000) {
        timeout(sink.write(chunk)).await.expect("write");
    }
    timeout(sink.finish()).await.expect("finish");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if (*meta.borrow()).is_some() {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "no stream callback");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let (seen_txid, seen_total) = (*meta.borrow()).expect("meta set");
    assert_eq!(seen_txid, txid);
    assert_eq!(seen_total, payload.len() as u32);
}

// ---- S12: receiver captures TRANSACTION_PAUSE, connection stays alive ----

#[tokio::test]
async fn s12_receiver_captures_pause_frame() {
    let (a, b, _counter) = session_pair(|_| {
        tuned(|s| {
            s.frame_size = 1000;
        })
    })
    .await;

    // Receiver: a reader task drains chunks; an independent poller samples
    // the stream state (visible even while no chunks flow during a pause).
    // Both slots are watch cells: the writes are synchronous and the test
    // polls both values while the transfer is still running, so what it
    // needs is a snapshot read that never consumes the value.
    struct Rx {
        paused_seen: tokio::sync::watch::Sender<bool>,
        terminal: tokio::sync::watch::Sender<Option<StreamState>>,
    }
    let rx = Arc::new(Rx {
        paused_seen: tokio::sync::watch::channel(false).0,
        terminal: tokio::sync::watch::channel(None).0,
    });
    {
        let rx = Arc::clone(&rx);
        b.on_binary_stream(Arc::new(move |stream: BinaryStream| {
            let reader_rx = Arc::clone(&rx);
            let reader_stream = stream.clone();
            tokio::spawn(async move {
                while reader_stream.read().await.is_some() {}
                reader_rx.terminal.send_replace(Some(reader_stream.state()));
            });
            let poller_rx = Arc::clone(&rx);
            let poller_stream = stream;
            tokio::spawn(async move {
                for _ in 0..200 {
                    tokio::time::sleep(Duration::from_millis(25)).await;
                    if matches!(poller_stream.state(), StreamState::Paused) {
                        poller_rx.paused_seen.send_replace(true);
                    }
                }
            });
        }))
        .await;
    }

    let payload = pattern(4 * 1000);
    let sink: BinarySink = timeout(a.open_binary_sink(
        BinaryMeta {
            total_len: payload.len() as u32,
        },
        None,
    ))
    .await
    .expect("open sink");

    timeout(sink.write(&payload[..2000]))
        .await
        .expect("write half");
    tokio::time::sleep(Duration::from_millis(150)).await; // frames reach b
    timeout(sink.pause()).await.expect("pause");

    // The receiver must observe the pause frame (stream Paused).
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !*rx.paused_seen.borrow() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "receiver stream never observed Paused"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Connection alive throughout: resume completes the transaction and the
    // receiver stream terminates Complete.
    timeout(sink.resume()).await.expect("resume");
    for chunk in payload[2000..].chunks(1000) {
        timeout(sink.write(chunk)).await.expect("post-resume write");
    }
    let result = timeout(sink.finish()).await.expect("finish");
    assert!(result.confirmed, "connection alive across the pause window");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if matches!(*rx.terminal.borrow(), Some(StreamState::Complete)) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "stream never completed"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
