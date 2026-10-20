//! TRANSACTION_COMPLETED frame + short-write branch tests (SPEC §5).
//!
//! Coverage:
//! - T1 kernel byte-count consistency: the written count always equals the
//!   sum of the emitted frame payloads; a finish with a byte shortfall is
//!   rejected and leaves the transaction state intact.
//! - T2 normal completion: the sender announces the completion on the wire
//!   (wire-level: frame type + txid) and the receiver's stream completes.
//! - T3 short write: the sender announces the cancellation (TRANSACTION_ABORT)
//!   and the receiver's stream ends explicitly as `Cancelled` — it never
//!   hangs.
//! - T4 completed with missing bytes: an abnormally injected
//!   TRANSACTION_COMPLETED ends the stream as `Incomplete`, carrying the
//!   byte gap.
//! - T5 late completed: a TRANSACTION_COMPLETED arriving after the stream
//!   completed has no side effect and the session stays healthy.
//!
//! The announcement rides the **data** FIFO channel (the control channel is
//! drained with priority and would let it overtake the queued data frames —
//! the receiver would then see "bytes not yet complete + completed" and
//! wrongly end the stream as `Incomplete`).

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::io::BufReader;
use tokio::sync::Mutex as AsyncMutex;

use xacpp::carrier::XacppTransport;
use xacpp::carrier::stdio::StdioTransport;
use xacpp::reliability::stream::{BinaryMeta, StreamState};
use xacpp::reliability::transaction::{BeginError, SenderConfig, TransactionSender};
use xacpp::session::XacppSession;
use xacpp::session::handler::RequestHandler;
use xacpp::session::handler::{
    EstablishDecision, EstablishHandler, NegotiateHandler, XacppSessionHandler,
};
use xacpp::session::peer::XacppPeer;
use xacpp::transfer::{TransferActivityHandlers, TransferConfig, TransferWiring};
use xacpp::wire::capability::Capabilities;
use xacpp::wire::commands::XacppCommand;
use xacpp::wire::control::TransactionCompletedPayload;
use xacpp::wire::error::XacppError;
use xacpp::wire::events::XacppActivityEvent;
use xacpp::wire::frame::{Frame, FrameType, HEADER_LEN};
use xacpp::wire::message::{XacppRequest, XacppResponse};

/// Wire frame type of TRANSACTION_COMPLETED (SPEC §5, control class).
const FRAME_TYPE_COMPLETED: u16 = 0x000B;
/// Wire frame type of TRANSACTION_ABORT (SPEC §5, control class).
const FRAME_TYPE_ABORT: u16 = 0x000A;

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

// ---- Wire recorder -------------------------------------------------------

/// Every frame the sender put on the wire, as (wire frame type, payload).
///
/// The control channel carries whole wire frames, the data channel carries
/// `Frame` values — both are reduced to the same shape so the tests can
/// assert on the frame type and decode the payload.
struct Wire {
    /// Shared cell of the recorded frames. A watch channel keeps the
    /// recorder's write path synchronous (`send_modify`) and lets every
    /// reader take an independent snapshot (`borrow`): the recorder is
    /// read several times per test, so the value must never be consumed.
    frames: tokio::sync::watch::Sender<Vec<(u16, Vec<u8>)>>,
}

impl Default for Wire {
    fn default() -> Self {
        Self {
            frames: tokio::sync::watch::channel(Vec::new()).0,
        }
    }
}

impl Wire {
    fn record(&self, frame_type: u16, payload: Vec<u8>) {
        self.frames
            .send_modify(|frames| frames.push((frame_type, payload)));
    }

    fn frames(&self) -> Vec<(u16, Vec<u8>)> {
        (*self.frames.borrow()).clone()
    }

    /// The payloads of every frame of the given wire type.
    fn payloads_of(&self, frame_type: u16) -> Vec<Vec<u8>> {
        self.frames()
            .into_iter()
            .filter(|(t, _)| *t == frame_type)
            .map(|(_, p)| p)
            .collect()
    }
}

/// Splits a wire frame into (frame type, payload).
fn decode_wire(bytes: &[u8]) -> Option<(u16, Vec<u8>)> {
    if bytes.len() < HEADER_LEN {
        return None;
    }
    let frame_type = u16::from_be_bytes([bytes[6], bytes[7]]);
    let frame_len = u32::from_be_bytes([bytes[32], bytes[33], bytes[34], bytes[35]]) as usize;
    let payload = bytes.get(HEADER_LEN..HEADER_LEN + frame_len)?.to_vec();
    Some((frame_type, payload))
}

// ---- Test transport ------------------------------------------------------

/// Stdio-duplex pipe that records every frame the session puts on the wire.
struct RecordingPipe {
    inner: Arc<StdioTransport>,
    wire: Arc<Wire>,
}

#[async_trait]
impl XacppTransport for RecordingPipe {
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
impl TransferWiring for RecordingPipe {
    async fn write_control_frame(&self, bytes: Vec<u8>) -> Result<(), XacppError> {
        if let Some((frame_type, payload)) = decode_wire(&bytes) {
            self.wire.record(frame_type, payload);
        }
        self.inner.write_control_frame(bytes).await
    }
    async fn write_data_frame(&self, frame: Frame) -> Result<(), XacppError> {
        self.wire
            .record(frame.frame_type.to_u16(), frame.payload.clone());
        self.inner.write_data_frame(frame).await
    }
    async fn on_transfer_activity(&self, handlers: Option<TransferActivityHandlers>) {
        self.inner.on_transfer_activity(handlers).await;
    }
}

// ---- Session handlers ----------------------------------------------------

struct NullHandler;

#[async_trait]
impl XacppSessionHandler for NullHandler {
    async fn on_command(&self, _command: XacppCommand) -> Result<XacppResponse, XacppError> {
        Ok(XacppResponse::acknowledge())
    }

    async fn on_event(&self, _event: XacppActivityEvent) -> Result<XacppResponse, XacppError> {
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

// ---- Rig -----------------------------------------------------------------

struct Rig {
    session_a: Arc<XacppSession>,
    session_b: Arc<XacppSession>,
    /// The sender's transport — the injection point for abnormal frames.
    transport_a: Arc<RecordingPipe>,
    wire: Arc<Wire>,
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

/// Transfer config with a small working-frame size, so a few hundred bytes
/// leave as whole frames (the default 1 MiB frame would keep them in the
/// sender's partial buffer until the finish).
fn small_frames() -> TransferConfig {
    let mut config = TransferConfig::default();
    config.sender.frame_size = 100;
    config
}

/// Establishes a duplex session pair with a wire recorder on the sender side.
async fn session_pair() -> Rig {
    let (ta, tb) = stdio_duplex();
    let wire = Arc::new(Wire::default());
    let pipe_a = Arc::new(RecordingPipe {
        inner: ta,
        wire: Arc::clone(&wire),
    });
    let pipe_b = Arc::new(RecordingPipe {
        inner: tb,
        wire: Arc::new(Wire::default()),
    });
    let b_slot: Arc<AsyncMutex<Option<Arc<XacppSession>>>> = Arc::new(AsyncMutex::new(None));

    let session_a = Arc::new(XacppSession::new(Some(small_frames())));
    let peer_a = XacppPeer::new(
        Capabilities::default(),
        Arc::clone(&pipe_a) as Arc<dyn XacppTransport>,
        Some(Arc::clone(&session_a)),
        Arc::new(AcceptNegotiate),
        Arc::new(NoEstablish),
    );
    let peer_b = XacppPeer::new(
        Capabilities::default(),
        Arc::clone(&pipe_b) as Arc<dyn XacppTransport>,
        None,
        Arc::new(AcceptNegotiate),
        Arc::new(BCollector {
            config: Some(small_frames()),
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
    Rig {
        session_a,
        session_b,
        transport_a: pipe_a,
        wire,
    }
}

/// Builds a TRANSACTION_COMPLETED frame for the given transaction id.
fn completed_frame(txid: [u8; 16]) -> Frame {
    let payload = TransactionCompletedPayload {
        transaction_id: txid,
    }
    .encode()
    .expect("completed payload encodes");
    Frame::new(FrameType::TransactionCompleted, [0u8; 16], 0, 0, payload)
}

/// Registers a stream handler on the receiver side and returns the channel
/// the opened streams arrive on.
async fn capture_streams(
    session_b: &XacppSession,
) -> tokio::sync::mpsc::UnboundedReceiver<xacpp::reliability::stream::BinaryStream> {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    session_b
        .on_binary_stream(Arc::new(move |stream| {
            let _ = tx.send(stream);
        }))
        .await;
    rx
}

/// Drains a stream to its end and returns the number of bytes received.
async fn drain(stream: &xacpp::reliability::stream::BinaryStream) -> usize {
    let mut total = 0usize;
    while let Some(chunk) = timeout(stream.read()).await {
        total += chunk.bytes.len();
    }
    total
}

// ---- Tests ---------------------------------------------------------------

/// T1 — kernel byte-count consistency.
///
/// The written count always equals the sum of the emitted frame payloads
/// (frame-aligned writes), and a finish with a byte shortfall is rejected
/// with the counts reported — without polluting the transaction state.
#[test]
fn t1_written_count_equals_frame_payload_sum_and_short_finish_is_rejected() {
    let mut sender = TransactionSender::new(SenderConfig {
        frame_size: 100,
        ..SenderConfig::default()
    });
    let txid = [7u8; 16];
    sender.open_transaction(txid, 1000, 0).unwrap();

    // Frame-aligned writes: every chunk leaves as whole frames, so the
    // written count is exactly the sum of the frame payloads.
    let mut payload_sum = 0usize;
    for _ in 0..3 {
        let frames = sender.append_chunk(txid, &pattern(100), 10).unwrap();
        let sum: usize = frames.iter().map(|f| f.payload.len()).sum();
        assert_eq!(sum, 100, "one chunk leaves as one whole frame");
        payload_sum += sum;
    }
    assert_eq!(
        payload_sum, 300,
        "written count equals the frame payload sum"
    );

    // Finish short: rejected, with the counts reported.
    match sender.finish_transaction(txid, 30) {
        Err(BeginError::IncompletePayload { written, declared }) => {
            assert_eq!(
                written, 300,
                "reported written count is the frame payload sum"
            );
            assert_eq!(declared, 1000, "declared total reported");
        }
        other => panic!("expected IncompletePayload, got {other:?}"),
    }

    // The rejection did not pollute the state: the transaction is still
    // pending and a repeat finish reports the same shortfall.
    assert!(
        sender.contains_transaction(txid),
        "state intact after rejection"
    );
    match sender.finish_transaction(txid, 40) {
        Err(BeginError::IncompletePayload { written, declared }) => {
            assert_eq!((written, declared), (300, 1000), "rejection is repeatable");
        }
        other => panic!("expected IncompletePayload again, got {other:?}"),
    }

    // A sub-frame remainder counts towards the written total as well.
    let mut sender2 = TransactionSender::new(SenderConfig {
        frame_size: 100,
        ..SenderConfig::default()
    });
    let txid2 = [8u8; 16];
    sender2.open_transaction(txid2, 1000, 0).unwrap();
    let frames = sender2.append_chunk(txid2, &pattern(350), 10).unwrap();
    let sum: usize = frames.iter().map(|f| f.payload.len()).sum();
    assert_eq!(sum, 300, "the remainder is held back, not framed");
    match sender2.finish_transaction(txid2, 30) {
        Err(BeginError::IncompletePayload { written, declared }) => {
            assert_eq!(written, 350, "written count includes the held remainder");
            assert_eq!(declared, 1000);
        }
        other => panic!("expected IncompletePayload, got {other:?}"),
    }
}

/// T2 — normal completion: the receiver gets the completed frame and the
/// stream completes.
#[tokio::test]
async fn t2_finish_announces_completion_and_the_stream_completes() {
    let rig = session_pair().await;
    let mut streams = capture_streams(&rig.session_b).await;

    let sink = rig
        .session_a
        .open_binary_sink(BinaryMeta { total_len: 1000 }, None)
        .await
        .unwrap();
    let txid = sink.txid();
    sink.write(&pattern(1000)).await.unwrap();
    let result = sink.finish().await.unwrap();
    assert!(result.confirmed, "delivery confirmed");

    // The receiver's stream completes with every byte.
    let stream = timeout(streams.recv()).await.expect("stream opened");
    assert_eq!(drain(&stream).await, 1000, "all bytes delivered");
    assert_eq!(stream.state(), StreamState::Complete, "stream completed");
    assert!(stream.incomplete_detail().is_none(), "no gap reported");

    // Wire-level: a TRANSACTION_COMPLETED frame naming this transaction was
    // emitted, on the data channel (ordered behind the data frames).
    let payloads = rig.wire.payloads_of(FRAME_TYPE_COMPLETED);
    assert_eq!(payloads.len(), 1, "exactly one completion announced");
    let announced = TransactionCompletedPayload::decode(&payloads[0]).unwrap();
    assert_eq!(
        announced.transaction_id, txid,
        "completion names the transaction"
    );
}

/// T3 — short write: the sender announces the cancellation and the
/// receiver's stream ends explicitly instead of hanging.
#[tokio::test]
async fn t3_short_write_aborts_the_peer_stream_instead_of_hanging() {
    let rig = session_pair().await;
    let mut streams = capture_streams(&rig.session_b).await;

    let sink = rig
        .session_a
        .open_binary_sink(BinaryMeta { total_len: 1000 }, None)
        .await
        .unwrap();
    let txid = sink.txid();
    // The provider declares 1000 bytes but writes 400.
    sink.write(&pattern(400)).await.unwrap();

    // Wait for the peer to have the first frame (the stream exists): the
    // abort cannot be overtaken by data that has not been delivered yet.
    let stream = timeout(streams.recv()).await.expect("stream opened");

    // The finish fails: the declared total was not written.
    match sink.finish().await {
        Ok(result) => panic!("short write must fail the finish, got {result:?}"),
        Err(e) => {
            assert!(
                e.to_string().contains("incomplete payload"),
                "the error names the byte shortfall: {e}"
            );
        }
    }

    // The peer's stream ends explicitly as cancelled — it never hangs.
    drain(&stream).await;
    assert_eq!(stream.state(), StreamState::Cancelled, "stream cancelled");
    assert!(stream.incomplete_detail().is_none(), "no gap reported");

    // Wire-level: the cancellation was announced, and no completion was.
    let aborts = rig.wire.payloads_of(FRAME_TYPE_ABORT);
    assert_eq!(aborts.len(), 1, "exactly one abort announced");
    assert!(
        rig.wire.payloads_of(FRAME_TYPE_COMPLETED).is_empty(),
        "a short write announces no completion"
    );
    let _ = txid;
}

/// T4 — an abnormally injected completion with missing bytes ends the
/// stream as `Incomplete`, carrying the byte gap.
#[tokio::test]
async fn t4_completed_with_missing_bytes_ends_the_stream_as_incomplete() {
    let rig = session_pair().await;
    let mut streams = capture_streams(&rig.session_b).await;

    let sink = rig
        .session_a
        .open_binary_sink(BinaryMeta { total_len: 1000 }, None)
        .await
        .unwrap();
    let txid = sink.txid();
    // Only 400 of the declared 1000 bytes are written.
    sink.write(&pattern(400)).await.unwrap();
    let stream = timeout(streams.recv()).await.expect("stream opened");

    // Abnormal injection: a TRANSACTION_COMPLETED for a transaction whose
    // bytes have not all arrived.
    rig.transport_a
        .write_data_frame(completed_frame(txid))
        .await
        .unwrap();

    // The stream ends explicitly as incomplete, with the gap.
    drain(&stream).await;
    assert_eq!(stream.state(), StreamState::Incomplete, "stream incomplete");
    let detail = stream.incomplete_detail().expect("gap reported");
    assert_eq!(detail.received, 400, "received bytes reported");
    assert_eq!(detail.expected, 1000, "declared total reported");
}

/// T5 — a completion arriving after the stream completed has no side effect
/// and the session stays healthy.
#[tokio::test]
async fn t5_late_completed_frame_is_idempotent() {
    let rig = session_pair().await;
    let mut streams = capture_streams(&rig.session_b).await;

    let sink = rig
        .session_a
        .open_binary_sink(BinaryMeta { total_len: 1000 }, None)
        .await
        .unwrap();
    let txid = sink.txid();
    sink.write(&pattern(1000)).await.unwrap();
    sink.finish().await.unwrap();

    let stream = timeout(streams.recv()).await.expect("stream opened");
    assert_eq!(drain(&stream).await, 1000);
    assert_eq!(stream.state(), StreamState::Complete, "stream completed");

    // The late completion for the settled transaction: ignored.
    rig.transport_a
        .write_data_frame(completed_frame(txid))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        stream.state(),
        StreamState::Complete,
        "the late frame changed nothing"
    );
    assert!(stream.incomplete_detail().is_none(), "no gap reported");

    // The session is still healthy: a second transfer completes.
    let sink2 = rig
        .session_a
        .open_binary_sink(BinaryMeta { total_len: 10 }, None)
        .await
        .unwrap();
    sink2.write(&pattern(10)).await.unwrap();
    assert!(sink2.finish().await.unwrap().confirmed, "session healthy");
}

/// T6 — the whole-payload path (`send_binary`) also announces the
/// completion: the contract has no carve-out for it.
#[tokio::test]
async fn t6_send_binary_also_announces_completion() {
    let rig = session_pair().await;

    // Aggregate surface: the receiver collects the complete payload.
    // The handler is a synchronous `Fn` — it cannot await — so the slot is
    // a watch cell: the write overwrites synchronously, the read snapshots
    // the last value.
    let received = tokio::sync::watch::channel(None::<(usize, Vec<u8>)>).0;
    let received2 = received.clone();
    rig.session_b
        .on_binary(Arc::new(move |txid: [u8; 16], bytes: Vec<u8>| {
            received2.send_replace(Some((bytes.len(), bytes)));
            let _ = txid;
        }))
        .await;

    let payload = pattern(3000);
    let (done_tx, done_rx) = tokio::sync::oneshot::channel();
    let txid = rig
        .session_a
        .send_binary(
            payload.clone(),
            Some(Box::new(move |result| {
                assert!(result.confirmed, "delivery confirmed");
                let _ = done_tx.send(());
            })),
        )
        .await
        .unwrap();
    timeout(done_rx).await.unwrap();

    // The bytes arrived intact on the aggregate surface.
    let got = (*received.borrow()).clone().expect("payload delivered");
    assert_eq!(got.0, 3000);
    assert_eq!(got.1, payload, "bit-exact");

    // Wire-level: exactly one TRANSACTION_COMPLETED, naming this
    // transaction, ordered behind the data frames on the wire.
    let payloads = rig.wire.payloads_of(FRAME_TYPE_COMPLETED);
    assert_eq!(payloads.len(), 1, "exactly one completion announced");
    let announced = TransactionCompletedPayload::decode(&payloads[0]).unwrap();
    assert_eq!(announced.transaction_id, txid);
    let frames = rig.wire.frames();
    let last_data = frames
        .iter()
        .rposition(|(t, _)| *t == 0x1002)
        .expect("data frames on the wire");
    let completed_pos = frames
        .iter()
        .position(|(t, _)| *t == FRAME_TYPE_COMPLETED)
        .expect("completion on the wire");
    assert!(
        completed_pos > last_data,
        "the completion keeps its place behind the data frames"
    );
}

/// T7 — an abort right after a write (frames still queued) cannot overtake
/// the transaction's own data frames: the peer opens the stream first, then
/// sees the cancellation — no orphan stream is left behind.
#[tokio::test]
async fn t7_abort_never_overtakes_the_first_data_frame() {
    let rig = session_pair().await;
    let mut streams = capture_streams(&rig.session_b).await;

    let sink = rig
        .session_a
        .open_binary_sink(BinaryMeta { total_len: 1000 }, None)
        .await
        .unwrap();
    sink.write(&pattern(400)).await.unwrap();
    // Abort immediately — the data frames may still sit in the egress
    // queue; the FIFO discipline keeps the abort behind them.
    sink.abort().await.unwrap();

    // The peer saw the stream open (the data frames landed first) and then
    // end as cancelled — exactly one terminal state, no orphan.
    let stream = timeout(streams.recv()).await.expect("stream opened");
    drain(&stream).await;
    assert_eq!(
        stream.state(),
        StreamState::Cancelled,
        "stream cancelled, not orphaned"
    );

    // Wire-level: every data frame of the transaction precedes the abort.
    let frames = rig.wire.frames();
    let abort_pos = frames
        .iter()
        .position(|(t, _)| *t == FRAME_TYPE_ABORT)
        .expect("abort on the wire");
    assert!(
        frames
            .iter()
            .enumerate()
            .filter(|(_, (t, _))| *t == 0x1002)
            .all(|(i, _)| i < abort_pos),
        "no data frame lands after the abort"
    );
}
