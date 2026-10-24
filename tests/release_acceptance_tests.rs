//! Release acceptance test matrix (SPEC v1.0, `docs/SPEC.md`).
//!
//! Three sections, all wire-level (frames encoded/decoded through real
//! transports, never serde round-trips alone):
//!
//! - **A — functional coverage**: every command variant, every response
//!   variant, the full event vocabulary, and the `DATA_BINARY` size matrix,
//!   each asserted field-by-field across a live transport session.
//! - **B — extreme scenarios**: dirty-stream triage, mid-frame disconnects,
//!   size boundaries (exact pass / +1 reject, both encode and forged-header
//!   decode), retransmission-storm bounds with control-frame exemption and
//!   recovery, CRC tamper at all three points, version refusal, hostile
//!   capability declarations.
//! - **C — error-semantics ledger**: fault-tolerant behaviors (connection
//!   survives and keeps working) vs surfaced behaviors (error visible to
//!   the caller or connection), each asserted end-to-end.
//!
//! Shared infrastructure: a memory duplex pair, a transfer pair, an
//! "injector" rig (test-owned write half feeding hand-crafted frames into a
//! live transport peer, with response read-back), and a frame-dropping
//! writer for lossy-link scenarios.

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use serde_json::json;
use tokio::io::{AsyncBufRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};

use xacpp::carrier::XacppTransport;
use xacpp::carrier::stdio::StdioTransport;
use xacpp::reliability::transaction::SenderConfig;
use xacpp::session::XacppSession;
use xacpp::session::handler::RequestHandler;
use xacpp::session::handler::{
    EstablishDecision, EstablishHandler, NegotiateHandler, XacppSessionHandler,
};
use xacpp::session::peer::XacppPeer;
use xacpp::transfer::BinaryHandler;
use xacpp::transfer::TransferConfig;
use xacpp::wire::activity_ref::ActivityRef;
use xacpp::wire::capability::{Capabilities, EffectiveCapabilities};
use xacpp::wire::commands::XacppCommand;
use xacpp::wire::commands::lifecycle::{
    InvokeActivityPayload, ListActivityPayload, NewActivityPayload, SetActivityModePayload,
};
use xacpp::wire::error::XacppError;
use xacpp::wire::events::content::{
    AudioPart, ContentPart, FilePart, FileRef, ImagePart, TextPart, VideoPart,
};
use xacpp::wire::events::interaction::NotifyPayload;
use xacpp::wire::events::payload::{
    ActivityInfo, ActivityMode, ActivityStartEvent, AlertLevel, ContentDeltaEvent,
    ContentPartEvent, SecurityAlertEvent, ToolResultEvent, ToolResultStatus, ToolUseEvent,
    TraceableEvent,
};
use xacpp::wire::events::upload::{TokenUsage, UploadEvent};
use xacpp::wire::events::{XacppActivityEvent, XacppEvent};
use xacpp::wire::frame::{
    CRC_LEN, FRAME_OVERHEAD, Frame, FrameDecoder, FrameError, FrameType, HEADER_LEN, MAGIC,
    MAX_CONTROL_FRAME_LEN, MAX_DATA_BINARY_FRAME_LEN, MAX_DATA_JSON_FRAME_LEN,
    MAX_DECODE_TOTAL_CAP, crc32,
};
use xacpp::wire::message::{XacppEnvelope, XacppRequest, XacppResponse};

// =====================================================================================
// Shared helpers
// =====================================================================================

const DUPLEX_BUF: usize = 64 * 1024;

type BoxedWriter = Pin<Box<dyn AsyncWrite + Send>>;
type BoxedReader = Pin<Box<dyn AsyncBufRead + Send>>;

/// Deterministic payload pattern (not all-zero: exercises real bytes).
fn pattern(len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| ((i * 31 + (i >> 7)) & 0xff) as u8)
        .collect()
}

/// Poll `f` until true or `timeout`.
async fn wait_until(timeout: Duration, mut f: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    loop {
        if f() {
            return true;
        }
        if start.elapsed() >= timeout {
            return f();
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Shrunk timing for transfer scenarios (fast clock, 1 KiB frames).
fn test_cfg() -> TransferConfig {
    TransferConfig {
        sender: SenderConfig {
            frame_size: 1024,
            rto_initial_ms: 200,
            rto_max_ms: 800,
            backpressure_cap_bytes: 64 * 1024 * 1024,
        },
        heartbeat_interval_ms: 30_000,
        silent_timeout_ms: 60_000,
        ack_batch_frames: 64,
        ack_batch_interval_ms: 100,
        nack_feedback_interval_ms: 100,
        resume_verdict_timeout_ms: 300,
        tick_interval_ms: 20,
    }
}

/// Quiet config for observation windows: no heartbeats, no clock feedback,
/// no silent timeout — only explicit test traffic drives the wire.
fn quiet_cfg() -> TransferConfig {
    TransferConfig {
        heartbeat_interval_ms: u64::MAX,
        silent_timeout_ms: 60_000,
        ack_batch_interval_ms: u64::MAX,
        nack_feedback_interval_ms: u64::MAX,
        resume_verdict_timeout_ms: 300,
        ..test_cfg()
    }
}

/// An unconnected `StdioTransport` pair over a memory duplex.
fn duplex_pair() -> (StdioTransport, StdioTransport) {
    let (a, b) = tokio::io::duplex(DUPLEX_BUF);
    let (a_read, a_write) = tokio::io::split(a);
    let (b_read, b_write) = tokio::io::split(b);
    (
        StdioTransport::new(
            Box::pin(a_write) as BoxedWriter,
            Box::pin(BufReader::new(a_read)) as BoxedReader,
        ),
        StdioTransport::new(
            Box::pin(b_write) as BoxedWriter,
            Box::pin(BufReader::new(b_read)) as BoxedReader,
        ),
    )
}

/// Wire both sides with sessions, connect, return everything.
async fn reliable_pair(
    a_cfg: TransferConfig,
    b_cfg: TransferConfig,
) -> (
    Arc<StdioTransport>,
    Arc<XacppSession>,
    Arc<StdioTransport>,
    Arc<XacppSession>,
) {
    let (ta, tb) = duplex_pair();
    let session_a = Arc::new(XacppSession::new(Some(a_cfg)));
    let session_b = Arc::new(XacppSession::new(Some(b_cfg)));
    let ta: Arc<StdioTransport> = Arc::new(ta);
    let tb: Arc<StdioTransport> = Arc::new(tb);
    session_a
        .wire(Arc::clone(&ta) as Arc<dyn XacppTransport>)
        .await
        .unwrap();
    session_b
        .wire(Arc::clone(&tb) as Arc<dyn XacppTransport>)
        .await
        .unwrap();
    ta.connect().await.unwrap();
    tb.connect().await.unwrap();
    (ta, session_a, tb, session_b)
}

/// Register a binary delivery capture on `session`.
async fn capture_binary(session: &XacppSession) -> tokio::sync::mpsc::UnboundedReceiver<Vec<u8>> {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let tx = Arc::new(tx);
    let handler: BinaryHandler = Arc::new(move |_txid, payload| {
        let _ = tx.send(payload);
    });
    session.on_binary(handler).await;
    rx
}

// ---- Hand-crafted wire frames (forged headers, custom minor/major, tampering) ----

/// Assemble a full wire frame by hand: header (`major`/`minor` controllable),
/// payload, correct CRC over header+payload. `frame_len` on the wire equals
/// `payload.len()`.
fn build_wire(
    frame_type: FrameType,
    major: u8,
    minor: u8,
    txid: [u8; 16],
    total_len: u32,
    seq: u32,
    payload: &[u8],
) -> Vec<u8> {
    let mut out = Vec::with_capacity(FRAME_OVERHEAD + payload.len());
    out.extend_from_slice(&MAGIC);
    out.push(major);
    out.push(minor);
    out.extend_from_slice(&frame_type.to_u16().to_be_bytes());
    out.extend_from_slice(&txid);
    out.extend_from_slice(&total_len.to_be_bytes());
    out.extend_from_slice(&seq.to_be_bytes());
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(payload);
    let crc = crc32(&out);
    out.extend_from_slice(&crc.to_be_bytes());
    out
}

/// A lone 36-byte header declaring `frame_len` (no body, no CRC): header
/// validation fires before any body is needed, so rejection is observable
/// without a real body.
fn forged_header(frame_type: FrameType, major: u8, frame_len: u32) -> [u8; HEADER_LEN] {
    let mut hdr = [0u8; HEADER_LEN];
    hdr[..4].copy_from_slice(&MAGIC);
    hdr[4] = major;
    hdr[5] = 0;
    hdr[6..8].copy_from_slice(&frame_type.to_u16().to_be_bytes());
    hdr[32..36].copy_from_slice(&frame_len.to_be_bytes());
    hdr
}

fn envelope_bytes(envelope: &XacppEnvelope) -> Vec<u8> {
    serde_json::to_vec(envelope).expect("envelope serializes")
}

fn data_json_frame(envelope: &XacppEnvelope) -> Vec<u8> {
    build_wire(
        FrameType::DataJson,
        1,
        0,
        [0u8; 16],
        0,
        0,
        &envelope_bytes(envelope),
    )
}

fn ack_request(id: &str) -> XacppEnvelope {
    XacppEnvelope::Request {
        id: id.to_owned(),
        session_id: None,
        payload: XacppRequest::Command(XacppCommand::generic("probe", json!({}))),
    }
}

// ---- Injector rig: test-owned write half + live transport peer ----

/// One-directional injection rig: the test owns the write half feeding the
/// peer transport (`a_write → b_read`), and reads responses coming back on
/// the opposite direction (`b_write → a_read`). The peer transport runs a
/// real reader/egress stack; everything the test injects crosses the full
/// frame-decode path.
struct Injector {
    write: Option<Pin<Box<dyn AsyncWrite + Send>>>,
    read: Pin<Box<dyn AsyncBufRead + Send>>,
    decoder: FrameDecoder,
    peer: Arc<StdioTransport>,
}

impl Injector {
    /// Create the rig with an acknowledge-echo handler on the peer.
    /// `pre_connect_bytes` are written into the pipe *before* the peer's
    /// reader starts (dirty-stream scenarios).
    async fn new(pre_connect_bytes: &[u8]) -> Self {
        let (a, b) = tokio::io::duplex(DUPLEX_BUF);
        let (a_read, mut a_write) = tokio::io::split(a);
        let (b_read, b_write) = tokio::io::split(b);
        if !pre_connect_bytes.is_empty() {
            a_write
                .write_all(pre_connect_bytes)
                .await
                .expect("pre-write");
            a_write.flush().await.expect("pre-flush");
        }
        let peer = StdioTransport::new(
            Box::pin(b_write) as BoxedWriter,
            Box::pin(BufReader::new(b_read)) as BoxedReader,
        );
        peer.on_request(echo_handler())
            .expect("handler before connect");
        Self {
            write: Some(Box::pin(a_write) as BoxedWriter),
            read: Box::pin(BufReader::new(a_read)) as BoxedReader,
            decoder: FrameDecoder::new(),
            peer: Arc::new(peer),
        }
    }

    /// Start the peer's reader stack.
    async fn connect_peer(&self) {
        self.peer.connect().await.expect("peer connect");
    }

    /// Wire a session to the peer transport (before `connect_peer`).
    async fn wire_peer(&self, session: &XacppSession) {
        session
            .wire(Arc::clone(&self.peer) as Arc<dyn XacppTransport>)
            .await
            .expect("wire session to peer");
    }

    async fn inject(&mut self, bytes: &[u8]) {
        let w = self.write.as_mut().expect("write half alive");
        w.write_all(bytes).await.expect("inject write");
        w.flush().await.expect("inject flush");
    }

    fn peer_closed(&self) -> bool {
        *self.peer.closed().borrow()
    }

    /// Read the next `DATA_JSON` envelope the peer sends back, or `None` on
    /// timeout / EOF / fatal decode.
    async fn next_envelope(&mut self, timeout: Duration) -> Option<XacppEnvelope> {
        let deadline = Instant::now() + timeout;
        let mut buf = [0u8; 8192];
        loop {
            if Instant::now() >= deadline {
                return None;
            }
            let n = tokio::time::timeout(deadline - Instant::now(), self.read.read(&mut buf))
                .await
                .ok()?
                .ok()?;
            if n == 0 {
                return None; // EOF
            }
            let frames = self.decoder.feed(&buf[..n]).ok()?;
            for frame in frames {
                if frame.frame_type == FrameType::DataJson {
                    return serde_json::from_slice(&frame.payload).ok();
                }
            }
        }
    }
}

/// Replies `acknowledge` to every request, counting the deliveries.
fn echo_handler() -> RequestHandler {
    let count = Arc::new(std::sync::atomic::AtomicU64::new(0));
    Arc::new(move |_session_id, _request| {
        let count = Arc::clone(&count);
        Box::pin(async move {
            count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(XacppResponse::acknowledge())
        })
            as Pin<Box<dyn std::future::Future<Output = Result<XacppResponse, XacppError>> + Send>>
    })
}

// ---- Frame-dropping writer (lossy-link emulator, batch-3 test lineage) ----

const FRAME_TYPE_DATA_BINARY: u16 = 0x1002;
const FRAME_TYPE_TRANSACTION_COMPLETED: u16 = 0x000b;

/// Writer wrapper that scans the outbound byte stream at frame boundaries
/// and silently drops the **first** `DATA_BINARY` frame with `target_seq`
/// (of any transaction) — a lossy-link emulator for the backpressure
/// scenario.
struct FrameDropper<W> {
    inner: W,
    scan: Vec<u8>,
    out: Vec<u8>,
    target_seq: u32,
    drop_armed: bool,
    drop_completed: bool,
}

impl<W: AsyncWrite + Unpin> FrameDropper<W> {
    fn new(inner: W, target_seq: u32) -> Self {
        Self {
            inner,
            scan: Vec::new(),
            out: Vec::new(),
            target_seq,
            drop_armed: true,
            drop_completed: false,
        }
    }

    fn classify(&mut self) {
        loop {
            let Some(magic_pos) = self.scan.windows(4).position(|w| w == MAGIC) else {
                self.out.append(&mut self.scan);
                return;
            };
            if magic_pos > 0 {
                self.out.extend(self.scan.drain(..magic_pos));
            }
            if self.scan.len() < HEADER_LEN {
                return;
            }
            let frame_len =
                u32::from_be_bytes([self.scan[32], self.scan[33], self.scan[34], self.scan[35]])
                    as usize;
            let total = HEADER_LEN + frame_len + CRC_LEN;
            if self.scan.len() < total {
                return;
            }
            let frame: Vec<u8> = self.scan.drain(..total).collect();
            let ftype = u16::from_be_bytes([frame[6], frame[7]]);
            let seq = u32::from_be_bytes([frame[28], frame[29], frame[30], frame[31]]);
            if self.drop_armed && ftype == FRAME_TYPE_DATA_BINARY && seq == self.target_seq {
                self.drop_armed = false;
                // The loss window swallows the completion announcement too:
                // the completed frame rides the data FIFO right behind the
                // transaction's frames, and a link that drops frames drops
                // it like any other — the peer then completes on the byte
                // count (the announcement is not mandatory).
                self.drop_completed = true;
            } else if self.drop_completed && ftype == FRAME_TYPE_TRANSACTION_COMPLETED {
                self.drop_completed = false;
            } else {
                self.out.extend_from_slice(&frame);
            }
        }
    }

    fn forward(&mut self, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        while !self.out.is_empty() {
            match Pin::new(&mut self.inner).poll_write(cx, &self.out) {
                Poll::Ready(Ok(n)) => {
                    self.out.drain(..n);
                }
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Pending => return Poll::Pending,
            }
        }
        Poll::Ready(Ok(()))
    }
}

impl<W: AsyncWrite + Unpin + Send> AsyncWrite for FrameDropper<W> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let this = &mut *self;
        this.scan.extend_from_slice(buf);
        this.classify();
        match this.forward(cx) {
            Poll::Ready(Ok(())) => Poll::Ready(Ok(buf.len())),
            Poll::Pending => Poll::Ready(Ok(buf.len())),
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let this = &mut *self;
        if this.forward(cx)?.is_pending() {
            return Poll::Pending;
        }
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let this = &mut *self;
        if this.forward(cx)?.is_pending() {
            return Poll::Pending;
        }
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

// ---- Peer-side capture handlers (event vocabulary / command echo) ----

/// Session handler capturing every event, replying `acknowledge`.
struct EventCaptureHandler {
    events: tokio::sync::Mutex<Vec<XacppActivityEvent>>,
}

impl EventCaptureHandler {
    fn new() -> Self {
        Self {
            events: tokio::sync::Mutex::new(Vec::new()),
        }
    }

    async fn snapshot(&self) -> Vec<XacppActivityEvent> {
        self.events.lock().await.clone()
    }
}

#[async_trait::async_trait]
impl XacppSessionHandler for EventCaptureHandler {
    async fn on_command(&self, _command: XacppCommand) -> Result<XacppResponse, XacppError> {
        Ok(XacppResponse::acknowledge())
    }

    async fn on_event(&self, event: XacppActivityEvent) -> Result<XacppResponse, XacppError> {
        self.events.lock().await.push(event);
        Ok(XacppResponse::acknowledge())
    }
}

/// Session handler replying `acknowledge` (initiator side placeholder).
struct NoopSessionHandler;

#[async_trait::async_trait]
impl XacppSessionHandler for NoopSessionHandler {
    async fn on_command(&self, _command: XacppCommand) -> Result<XacppResponse, XacppError> {
        Ok(XacppResponse::acknowledge())
    }

    async fn on_event(&self, _event: XacppActivityEvent) -> Result<XacppResponse, XacppError> {
        Ok(XacppResponse::acknowledge())
    }
}

/// Negotiate handler capturing the effective capabilities.
struct NegotiateCapture(Arc<tokio::sync::Mutex<Option<EffectiveCapabilities>>>);

impl NegotiateCapture {
    fn new() -> (Self, Arc<tokio::sync::Mutex<Option<EffectiveCapabilities>>>) {
        let slot = Arc::new(tokio::sync::Mutex::new(None));
        (Self(Arc::clone(&slot)), slot)
    }
}

#[async_trait::async_trait]
impl NegotiateHandler for NegotiateCapture {
    async fn on_negotiate(&self, effective: EffectiveCapabilities) -> Result<(), XacppError> {
        *self.0.lock().await = Some(effective);
        Ok(())
    }
}

/// Establish handler issuing the shared capture session handler.
struct CaptureEstablishHandler {
    session: Arc<EventCaptureHandler>,
}

#[async_trait::async_trait]
impl EstablishHandler for CaptureEstablishHandler {
    async fn on_establish(
        &self,
        _transport: Arc<dyn XacppTransport>,
        _credentials: Option<String>,
    ) -> Result<EstablishDecision, XacppError> {
        Ok(EstablishDecision::Established {
            session_id: "acc-sid".into(),
            handler: Arc::clone(&self.session) as Arc<dyn XacppSessionHandler>,
            credentials: "acc-creds".into(),
        })
    }

    async fn on_establish_confirm(
        &self,
        _transport: Arc<dyn XacppTransport>,
    ) -> Result<(String, Arc<dyn XacppSessionHandler>, String), XacppError> {
        Ok((
            "acc-sid".into(),
            Arc::clone(&self.session) as Arc<dyn XacppSessionHandler>,
            "acc-creds".into(),
        ))
    }
}

fn event_names() -> Vec<&'static str> {
    vec![
        "content_delta",
        "content_part",
        "think",
        "info",
        "warn",
        "error",
        "notify",
        "activity_start",
        "activity_updates",
        "activity_done",
        "activity_aborted",
        "tool_use",
        "tool_result",
        "security_alert",
        "upload",
        "pair_complete",
        "complete",
    ]
}

fn initiator_caps() -> Capabilities {
    Capabilities {
        commands: vec![
            json!({"name": "new_activity"}),
            json!({"name": "invoke_activity"}),
        ],
        produce_events: event_names()
            .into_iter()
            .map(|n| json!({"name": n}))
            .collect(),
        accept_events: Vec::new(),
    }
}

fn responder_caps() -> Capabilities {
    Capabilities {
        commands: Vec::new(),
        produce_events: Vec::new(),
        accept_events: event_names()
            .into_iter()
            .map(|n| json!({"name": n}))
            .collect(),
    }
}

/// Peer pair negotiated + established: returns the initiator peer, the
/// initiator session (request_event / send_event path), and the responder's
/// event capture.
async fn event_peer_pair() -> (XacppPeer, Arc<XacppSession>, Arc<EventCaptureHandler>) {
    let (ta, tb) = duplex_pair();
    let capture = Arc::new(EventCaptureHandler::new());
    let (neg_capture, _slot_b) = NegotiateCapture::new();
    let peer_a = XacppPeer::new(
        initiator_caps(),
        Arc::new(ta),
        Some(Arc::new(XacppSession::new(None))),
        Arc::new(NegotiateCapture::new().0),
        Arc::new(CaptureEstablishHandler {
            session: Arc::clone(&capture),
        }),
    );
    let peer_b = XacppPeer::new(
        responder_caps(),
        Arc::new(tb),
        None,
        Arc::new(neg_capture),
        Arc::new(CaptureEstablishHandler {
            session: Arc::clone(&capture),
        }),
    );
    peer_a.connect().await.unwrap();
    peer_b.connect().await.unwrap();
    peer_a.negotiate().await.unwrap();
    let session = peer_a
        .establish(None, Arc::new(NoopSessionHandler), |_| Ok(()))
        .await
        .expect("establish");
    (peer_a, session, capture)
}

// =====================================================================================
// A — Functional coverage (full vocabularies, wire-level)
// =====================================================================================

/// A.1 — every `XacppCommand` variant crosses the wire (frame encode/decode
/// on a live transport) and arrives field-identical, including field
/// boundary values (u64::MAX, unicode, empty strings, full multimodal
/// message arrays).
#[tokio::test]
async fn a_command_variants_wire_roundtrip() {
    let (ta, tb) = duplex_pair();

    let received = Arc::new(tokio::sync::Mutex::new(Vec::<XacppCommand>::new()));
    let received_for_handler = Arc::clone(&received);
    tb.on_request(Arc::new(move |_session_id, payload| {
        let received = Arc::clone(&received_for_handler);
        Box::pin(async move {
            if let XacppRequest::Command(cmd) = payload {
                received.lock().await.push(cmd);
            }
            Ok(XacppResponse::acknowledge())
        })
            as Pin<Box<dyn std::future::Future<Output = Result<XacppResponse, XacppError>> + Send>>
    }))
    .unwrap();

    let ta: Arc<dyn XacppTransport> = Arc::new(ta);
    ta.connect().await.unwrap();
    tb.connect().await.unwrap();

    // Establish with credentials absent and present.
    ta.send(
        None,
        XacppRequest::Command(XacppCommand::Establish { credentials: None }),
    )
    .await
    .unwrap();
    ta.send(
        None,
        XacppRequest::Command(XacppCommand::Establish {
            credentials: Some("creds-✓-123".into()),
        }),
    )
    .await
    .unwrap();

    // EstablishConfirm (no fields).
    ta.send(None, XacppRequest::Command(XacppCommand::EstablishConfirm))
        .await
        .unwrap();

    // Negotiate with a fully populated capability set.
    ta.send(
        None,
        XacppRequest::Command(XacppCommand::Negotiate {
            capabilities: initiator_caps(),
        }),
    )
    .await
    .unwrap();

    // Generic without activity, boundary-value arguments.
    let plain = XacppCommand::generic(
        "new_activity",
        serde_json::to_value(NewActivityPayload {
            title: Some("标题 ✓ emoji 🚀".into()),
        })
        .unwrap(),
    );
    ta.send(None, XacppRequest::Command(plain.clone()))
        .await
        .unwrap();

    // Generic with activity, lifecycle arguments carrying every ContentPart
    // shape and numeric boundary values.
    let file_ref = FileRef {
        remote_url: "https://example.test/media.bin".into(),
        local_uri: "/tmp/media.bin".into(),
        remote_expires_at: Some("2030-01-01T00:00:00Z".into()),
        mime_type: "application/octet-stream".into(),
        require_organized: true,
        size_bytes: u64::MAX,
        sha256: String::new(),
    };
    let messages = vec![
        ContentPart::Text(TextPart {
            text: "hello".into(),
            part_id: Some("p-1".into()),
            part_type: "default".into(),
        }),
        ContentPart::Image(ImagePart {
            source: file_ref.clone(),
            detail: Some("high".into()),
            width: Some(u32::MAX),
            height: Some(0),
            part_id: None,
            part_type: "default".into(),
        }),
        ContentPart::Audio(AudioPart {
            source: file_ref.clone(),
            sample_rate: Some(48_000),
            channels: Some(u16::MAX),
            duration_ms: Some(u64::MAX),
            part_id: Some("a-1".into()),
            part_type: "default".into(),
        }),
        ContentPart::Video(VideoPart {
            source: file_ref.clone(),
            duration_ms: Some(0),
            fps: Some(u32::MAX),
            width: None,
            height: None,
            part_id: None,
            part_type: "default".into(),
        }),
        ContentPart::File(FilePart {
            source: file_ref.clone(),
            part_id: Some("f-1".into()),
            part_type: "default".into(),
        }),
    ];
    let with_activity = XacppCommand::generic_with_activity(
        "invoke_activity",
        serde_json::to_value(InvokeActivityPayload {
            activity: "act-target".into(),
            messages,
        })
        .unwrap(),
        ActivityRef::new("act-src"),
    );
    ta.send(None, XacppRequest::Command(with_activity.clone()))
        .await
        .unwrap();

    // Generic with list arguments carrying optional-field extremes.
    let list_cmd = XacppCommand::generic(
        "list_activity",
        serde_json::to_value(ListActivityPayload {
            query: Some(String::new()),
            page_num: Some(u32::MAX),
            page_size: Some(1),
        })
        .unwrap(),
    );
    ta.send(None, XacppRequest::Command(list_cmd.clone()))
        .await
        .unwrap();

    // Generic with a mode enum + interactive arguments shape.
    let mode_cmd = XacppCommand::generic(
        "set_activity_mode",
        serde_json::to_value(SetActivityModePayload {
            activity: "act-1".into(),
            mode: ActivityMode::Auto,
        })
        .unwrap(),
    );
    ta.send(None, XacppRequest::Command(mode_cmd.clone()))
        .await
        .unwrap();

    let got = received.lock().await;
    assert_eq!(got.len(), 8, "every command arrived exactly once");
    let value_of = |c: &XacppCommand| serde_json::to_value(c).unwrap();
    assert_eq!(
        value_of(&got[0]),
        json!({"establish": {}}),
        "Establish without credentials serializes with no credentials field"
    );
    assert_eq!(
        value_of(&got[1]),
        json!({"establish": {"credentials": "creds-✓-123"}})
    );
    assert_eq!(value_of(&got[2]), json!("establish_confirm"));
    assert_eq!(
        value_of(&got[3]),
        json!({"negotiate": {"capabilities": serde_json::to_value(initiator_caps()).unwrap()}})
    );
    assert_eq!(value_of(&got[4]), value_of(&plain));
    assert_eq!(value_of(&got[5]), value_of(&with_activity));
    // Activity reference survives the wire.
    match &got[5] {
        XacppCommand::Generic { activity, .. } => {
            assert_eq!(activity.as_ref(), Some(&ActivityRef::new("act-src")));
        }
        other => panic!("expected Generic, got {other:?}"),
    }
    assert_eq!(value_of(&got[6]), value_of(&list_cmd));
    assert_eq!(value_of(&got[7]), value_of(&mode_cmd));
    // Response path also exercised (each send awaited its acknowledge).
}

/// A.2 — Negotiate round trip: responder receives the initiator's full
/// capability set; initiator receives the responder's; the effective
/// intersection (emit_events) matches produce ∩ accept.
#[tokio::test]
async fn a_negotiate_capabilities_wire_fields() {
    let (ta, tb) = duplex_pair();
    let (neg_b, slot_b) = NegotiateCapture::new();
    let peer_a = XacppPeer::new(
        initiator_caps(),
        Arc::new(ta),
        Some(Arc::new(XacppSession::new(None))),
        Arc::new(NegotiateCapture::new().0),
        Arc::new(CaptureEstablishHandler {
            session: Arc::new(EventCaptureHandler::new()),
        }),
    );
    let peer_b = XacppPeer::new(
        responder_caps(),
        Arc::new(tb),
        None,
        Arc::new(neg_b),
        Arc::new(CaptureEstablishHandler {
            session: Arc::new(EventCaptureHandler::new()),
        }),
    );
    peer_a.connect().await.unwrap();
    peer_b.connect().await.unwrap();
    peer_a.negotiate().await.unwrap();

    // Initiator sees the responder's capabilities verbatim.
    let remote = peer_a.remote_capabilities().await;
    assert_eq!(
        serde_json::to_value(&remote).unwrap(),
        serde_json::to_value(responder_caps()).unwrap(),
        "responder capabilities cross the wire field-identical"
    );

    // Direction semantics (peer.rs): each side's emit_events =
    // own produce_events ∩ peer's accept_events. The initiator produces
    // everything the responder accepts (17 names); the responder produces
    // nothing, so its side of the intersection is empty.
    let effective_b = slot_b.lock().await.clone().expect("negotiate observed");
    assert!(
        effective_b.emit_events.is_empty(),
        "responder produces nothing: its emit list is empty"
    );
    assert_eq!(
        peer_a.emit_events().await,
        event_names(),
        "initiator emit list = initiator.produce ∩ responder.accept"
    );
}

/// A.3 — every `XacppResponse` variant crosses the wire and arrives
/// field-identical (transport-level handler constructs each variant).
#[tokio::test]
async fn a_response_variants_wire_roundtrip() {
    let (ta, tb) = duplex_pair();

    tb.on_request(Arc::new(|_session_id, payload| {
        Box::pin(async move {
            let XacppRequest::Command(XacppCommand::Generic { name, .. }) = payload else {
                return Ok(XacppResponse::acknowledge());
            };
            match name.as_str() {
                "r_negotiated" => Ok(XacppResponse::Negotiated {
                    capabilities: responder_caps(),
                }),
                "r_established" => Ok(XacppResponse::Established {
                    session_id: "sid-1".into(),
                    credentials: "creds-1".into(),
                }),
                "r_prepare" => Ok(XacppResponse::EstablishPrepare {
                    challenge: "challenge-✓".into(),
                }),
                "r_reject" => Ok(XacppResponse::EstablishReject {
                    reason: "not allowed".into(),
                }),
                "r_error" => Ok(XacppResponse::Error {
                    code: "custom_code".into(),
                    message: "boom".into(),
                }),
                "r_generic" => Ok(XacppResponse::Generic {
                    name: "activity_ready".into(),
                    data: json!({"activity": "act-1"}),
                }),
                other => panic!("unexpected probe name {other}"),
            }
        })
            as Pin<Box<dyn std::future::Future<Output = Result<XacppResponse, XacppError>> + Send>>
    }))
    .unwrap();

    let ta: Arc<dyn XacppTransport> = Arc::new(ta);
    ta.connect().await.unwrap();
    tb.connect().await.unwrap();

    let probe = |ta: Arc<dyn XacppTransport>, name: &'static str| async move {
        ta.send(
            None,
            XacppRequest::Command(XacppCommand::generic(name, json!({}))),
        )
        .await
        .expect("round trip")
    };

    match probe(Arc::clone(&ta), "r_negotiated").await {
        XacppResponse::Negotiated { capabilities } => assert_eq!(
            serde_json::to_value(&capabilities).unwrap(),
            serde_json::to_value(responder_caps()).unwrap()
        ),
        other => panic!("expected Negotiated, got {other:?}"),
    }
    match probe(Arc::clone(&ta), "r_established").await {
        XacppResponse::Established {
            session_id,
            credentials,
        } => {
            assert_eq!(session_id, "sid-1");
            assert_eq!(credentials, "creds-1");
        }
        other => panic!("expected Established, got {other:?}"),
    }
    match probe(Arc::clone(&ta), "r_prepare").await {
        XacppResponse::EstablishPrepare { challenge } => assert_eq!(challenge, "challenge-✓"),
        other => panic!("expected EstablishPrepare, got {other:?}"),
    }
    match probe(Arc::clone(&ta), "r_reject").await {
        XacppResponse::EstablishReject { reason } => assert_eq!(reason, "not allowed"),
        other => panic!("expected EstablishReject, got {other:?}"),
    }
    match probe(Arc::clone(&ta), "r_error").await {
        XacppResponse::Error { code, message } => {
            assert_eq!(code, "custom_code");
            assert_eq!(message, "boom");
        }
        other => panic!("expected Error, got {other:?}"),
    }
    match probe(Arc::clone(&ta), "r_generic").await {
        XacppResponse::Generic { name, data } => {
            assert_eq!(name, "activity_ready");
            assert_eq!(data, json!({"activity": "act-1"}));
        }
        other => panic!("expected Generic, got {other:?}"),
    }
}

/// Representative `data` payload for each event vocabulary name (typed
/// payload structs serialized exactly as consumers would).
fn event_sample(name: &str, idx: usize) -> XacppActivityEvent {
    let file_ref = FileRef {
        remote_url: format!("https://example.test/{name}.bin"),
        local_uri: format!("/tmp/{name}.bin"),
        remote_expires_at: Some("2030-01-01T00:00:00Z".into()),
        mime_type: "application/octet-stream".into(),
        require_organized: false,
        size_bytes: 42,
        sha256: "deadbeef".into(),
    };
    let data = match name {
        "content_delta" => serde_json::to_value(ContentDeltaEvent {
            round: "r-1".into(),
            pair: "p-1".into(),
            payload: ContentPart::Text(TextPart {
                text: "delta".into(),
                part_id: Some("pid-1".into()),
                part_type: "default".into(),
            }),
        })
        .unwrap(),
        "content_part" => serde_json::to_value(ContentPartEvent {
            round: "r-1".into(),
            pair: "p-2".into(),
            payload: ContentPart::Image(ImagePart {
                source: file_ref,
                detail: Some("auto".into()),
                width: Some(640),
                height: Some(480),
                part_id: Some("pid-2".into()),
                part_type: "default".into(),
            }),
        })
        .unwrap(),
        "think" => json!({"content": "thinking…", "round": "r-1", "pair": "p-3"}),
        "info" | "warn" | "error" => serde_json::to_value(TraceableEvent {
            title: format!("{name} title"),
            content: format!("{name} details"),
        })
        .unwrap(),
        "notify" => serde_json::to_value(NotifyPayload {
            message: "notify me".into(),
        })
        .unwrap(),
        "activity_start" => serde_json::to_value(ActivityStartEvent {
            goal: "do the thing".into(),
            activity: "act-sub".into(),
            metadata: Some([("k".to_owned(), "v".to_owned())].into_iter().collect()),
        })
        .unwrap(),
        "activity_updates" => json!({"activities": [serde_json::to_value(ActivityInfo {
            activity: "act-1".into(),
            agent: "agent-1".into(),
            title: Some("t".into()),
            description: None,
            updated_at: Some("2026-10-13T00:00:00Z".into()),
            working_dir: Some("$AGENT_HOME".into()),
            mode: ActivityMode::Auto,
        })
        .unwrap()]}),
        "activity_done" => json!({"activity": "act-1", "round": "r-1"}),
        "activity_aborted" => json!({"activity": "act-1", "reason": "user cancel"}),
        "tool_use" => serde_json::to_value(ToolUseEvent {
            request_id: format!("req-{idx}"),
            tool_name: "shell".into(),
            index: idx as u32,
            arguments: "{\"cmd\":\"ls\"}".into(),
            intent: "list files".into(),
        })
        .unwrap(),
        "tool_result" => serde_json::to_value(ToolResultEvent {
            request_id: format!("req-{idx}"),
            tool_name: "shell".into(),
            index: idx as u32,
            parts: vec![
                ContentPart::Text(TextPart {
                    text: "out".into(),
                    part_id: None,
                    part_type: "default".into(),
                }),
                ContentPart::File(FilePart {
                    source: FileRef {
                        remote_url: String::new(),
                        local_uri: "/tmp/out.txt".into(),
                        remote_expires_at: None,
                        mime_type: "text/plain".into(),
                        require_organized: false,
                        size_bytes: 3,
                        sha256: String::new(),
                    },
                    part_id: None,
                    part_type: "default".into(),
                }),
            ],
            auto: true,
            status: Some(ToolResultStatus::Completed),
            is_background: Some(false),
            is_error: false,
        })
        .unwrap(),
        "security_alert" => serde_json::to_value(SecurityAlertEvent {
            event_id: format!("evt-{idx}"),
            tool_name: "shell".into(),
            alert_level: AlertLevel::Critical,
            description: "suspicious".into(),
            threat_type: Some("injection".into()),
            matched_pattern: Some(r"(?i)rm\s+-rf".into()),
            context_snippet: Some("rm -rf /".into()),
        })
        .unwrap(),
        "upload" => match idx % 3 {
            0 => serde_json::to_value(UploadEvent::Progress {
                name: "f.bin".into(),
                uploaded: 1,
                total: 2,
            })
            .unwrap(),
            1 => serde_json::to_value(UploadEvent::Completed {
                name: "f.bin".into(),
                media_source: FileRef {
                    remote_url: "https://example.test/f.bin".into(),
                    local_uri: "/tmp/f.bin".into(),
                    remote_expires_at: None,
                    mime_type: "application/octet-stream".into(),
                    require_organized: false,
                    size_bytes: 2,
                    sha256: String::new(),
                },
            })
            .unwrap(),
            _ => serde_json::to_value(UploadEvent::Error {
                name: "f.bin".into(),
                error: "disk full".into(),
            })
            .unwrap(),
        },
        "pair_complete" => json!({
            "round": "r-1",
            "pair": "p-9",
            "contextWindow": {"used": 100, "cap": 200},
            "tokenUsage": serde_json::to_value(TokenUsage {
                id: format!("tu-{idx}"),
                message: "m".into(),
                prompt_tokens: 10,
                completion_tokens: 20,
                total_tokens: 30,
                cache_hit_tokens: None,
            })
            .unwrap()
        }),
        "complete" => json!({"round": "r-1", "status": "ok"}),
        other => unreachable!("unknown event name {other}"),
    };
    XacppActivityEvent::new("act-src", XacppEvent::new(name, data))
}

/// A.4 — the full event vocabulary crosses the wire through the Peer
/// `request_event` path; the responder's session handler receives each
/// event field-identical.
#[tokio::test]
async fn a_event_vocabulary_request_event_wire() {
    let (_peer_a, session, capture) = event_peer_pair().await;

    let names = event_names();
    let mut sent = Vec::new();
    let mut idx = 0;
    for name in &names {
        let repeats = if *name == "upload" { 3 } else { 1 };
        for _ in 0..repeats {
            let event = event_sample(name, idx);
            let response = session
                .request_event(event.clone())
                .await
                .expect("event round trip");
            match response {
                XacppResponse::Generic { name: n, .. } => assert_eq!(n, "acknowledge"),
                other => panic!("expected acknowledge, got {other:?}"),
            }
            sent.push(event);
            idx += 1;
        }
    }

    let got = capture.snapshot().await;
    assert_eq!(got.len(), sent.len(), "every event delivered exactly once");
    for (want, have) in sent.iter().zip(got.iter()) {
        assert_eq!(
            serde_json::to_value(have).unwrap(),
            serde_json::to_value(want).unwrap(),
            "event {:?} field-identical across the wire",
            want.event.name
        );
    }
}

/// A.5 — the fire-and-forget `send_event` path delivers with no response
/// wait; capture completes asynchronously.
#[tokio::test]
async fn a_event_send_event_faf_wire() {
    let (_peer_a, session, capture) = event_peer_pair().await;

    let event = event_sample("notify", 0);
    session.send_event(event.clone()).await.expect("queued");

    let deadline = Instant::now() + Duration::from_secs(5);
    while capture.snapshot().await.is_empty() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let got = capture.snapshot().await;
    assert_eq!(got.len(), 1, "faf event delivered exactly once");
    assert_eq!(
        serde_json::to_value(&got[0]).unwrap(),
        serde_json::to_value(&event).unwrap()
    );
}

/// A.6 — `DATA_BINARY` size matrix (100 KiB / 1 MiB / 5 MiB): each
/// transaction delivers byte-for-byte through working-frame segmentation.
#[tokio::test]
async fn a_binary_matrix_size_tiers_wire() {
    let (_a, sa, _b, sb) = reliable_pair(test_cfg(), test_cfg()).await;
    let mut delivered = capture_binary(&sb).await;

    for size in [100 * 1024, 1024 * 1024, 5 * 1024 * 1024] {
        let payload = pattern(size);
        sa.send_binary(payload.clone(), None)
            .await
            .expect("send_binary");
        let got = tokio::time::timeout(Duration::from_secs(30), delivered.recv())
            .await
            .expect("delivery within timeout")
            .expect("channel open");
        assert_eq!(got.len(), size, "tier {size}: length exact");
        assert!(got == payload, "tier {size}: bytes identical");
    }
}

/// A.7 — the `DATA_BINARY` ceiling case: exactly 16 MiB in one transaction
/// (16 working frames at 1 MiB) delivers byte-for-byte.
#[tokio::test]
async fn a_binary_16mib_exact_ceiling_wire() {
    let (_a, sa, _b, sb) = reliable_pair(test_cfg(), test_cfg()).await;
    let mut delivered = capture_binary(&sb).await;

    let size = MAX_DATA_BINARY_FRAME_LEN;
    let payload = pattern(size);
    sa.send_binary(payload.clone(), None)
        .await
        .expect("send_binary");
    let got = tokio::time::timeout(Duration::from_secs(60), delivered.recv())
        .await
        .expect("delivery within timeout")
        .expect("channel open");
    assert_eq!(got.len(), size, "16 MiB ceiling: length exact");
    assert!(got == payload, "16 MiB ceiling: bytes identical");
}

/// A.8 — tail-frame boundary: payload of `k·frame_size + 1` produces a
/// 1-byte tail frame and still delivers byte-for-byte.
#[tokio::test]
async fn a_binary_tail_frame_boundary_wire() {
    let (_a, sa, _b, sb) = reliable_pair(test_cfg(), test_cfg()).await;
    let mut delivered = capture_binary(&sb).await;

    let size = 2 * 1024 * 1024 + 1; // frames: 1 MiB + 1 MiB + 1 byte tail
    let payload = pattern(size);
    sa.send_binary(payload.clone(), None)
        .await
        .expect("send_binary");
    let got = tokio::time::timeout(Duration::from_secs(30), delivered.recv())
        .await
        .expect("delivery within timeout")
        .expect("channel open");
    assert_eq!(got.len(), size, "tail-frame case: length exact");
    assert!(got == payload, "tail-frame case: bytes identical");
    // The tail byte is the last pattern byte, proving frame boundaries carry
    // no off-by-one.
    assert_eq!(got[size - 1], payload[size - 1]);
}

/// A.9 — a `DATA_BINARY` frame one byte over the 16 MiB ceiling is rejected
/// at encode, before any wire byte exists.
#[tokio::test]
async fn a_binary_oversize_frame_encode_rejected() {
    let frame = Frame::new(
        FrameType::DataBinary,
        [0u8; 16],
        0,
        0,
        vec![0u8; MAX_DATA_BINARY_FRAME_LEN + 1],
    );
    match frame.encode() {
        Err(FrameError::FrameTooLarge {
            frame_type: FrameType::DataBinary,
            max,
            actual,
        }) => {
            assert_eq!(max, MAX_DATA_BINARY_FRAME_LEN);
            assert_eq!(actual, MAX_DATA_BINARY_FRAME_LEN + 1);
        }
        other => panic!("expected FrameTooLarge, got {other:?}"),
    }
}

// =====================================================================================
// B — Extreme-scenario matrix
// =====================================================================================

/// B.1 — dirty stream, garbage prefix: junk without a valid magic+header is
/// skipped at resync; the peer keeps working normally afterwards.
#[tokio::test]
async fn b_dirty_stream_garbage_prefix_recovers_wire() {
    let mut junk = Vec::new();
    junk.extend_from_slice(b"\x00\x01\x02 garbage, no magic here\n");
    junk.extend_from_slice(&[0xff; 64]);
    junk.extend_from_slice(b"trailing text line\n");
    let mut rig = Injector::new(&junk).await;
    rig.connect_peer().await;

    assert!(!rig.peer_closed(), "garbage prefix must not kill the peer");
    rig.inject(&data_json_frame(&ack_request("after-garbage")))
        .await;
    let response = rig
        .next_envelope(Duration::from_secs(5))
        .await
        .expect("peer responds after garbage");
    match response {
        XacppEnvelope::Response { id, payload, .. } => {
            assert_eq!(id, "after-garbage");
            assert!(matches!(payload, XacppResponse::Generic { .. }));
        }
        other => panic!("expected Response, got {other:?}"),
    }
}

/// B.2 — pseudo-magic inside a frame payload never disturbs framing: a
/// `DATA_BINARY` transaction whose payload embeds `XACP`-prefixed junk
/// delivers byte-for-byte and the connection keeps working.
#[tokio::test]
async fn b_dirty_stream_pseudo_magic_payload_inert_wire() {
    let (_a, sa, _b, sb) = reliable_pair(test_cfg(), test_cfg()).await;
    let mut delivered = capture_binary(&sb).await;

    // Payload containing pseudo-frame fragments (magic + plausible header
    // bytes + junk): if the receiver ever scanned inside a frame body these
    // would resync and corrupt the stream.
    let mut payload = pattern(3 * 1024 * 1024 + 7);
    for offset in [16usize, 65536, 1_000_000, payload.len() - 64] {
        payload[offset..offset + 4].copy_from_slice(&MAGIC);
        payload[offset + 4] = 1; // would-be major
        payload[offset + 5] = 0; // would-be minor
        payload[offset + 6..offset + 8].copy_from_slice(&0x7fffu16.to_be_bytes());
    }
    sa.send_binary(payload.clone(), None)
        .await
        .expect("send_binary");
    let got = tokio::time::timeout(Duration::from_secs(30), delivered.recv())
        .await
        .expect("delivery within timeout")
        .expect("channel open");
    assert!(
        got == payload,
        "bytes identical despite embedded pseudo-magic"
    );
}

/// B.3 — stdio log interleaving: free-form text lines (no frame magic) on
/// the same pipe are tolerated and framing recovers.
#[tokio::test]
async fn b_dirty_stream_stdio_text_interleaved_wire() {
    let log_junk = b"2026-10-13T12:00:00Z INFO  some log line without any framing\n\
                     2026-10-13T12:00:01Z WARN  another one, still no magic\n\
                     plain text \xe4\xb8\xad\xe6\x96\x87 unicode noise\n";
    let mut rig = Injector::new(log_junk).await;
    rig.connect_peer().await;

    assert!(
        !rig.peer_closed(),
        "text interleaving must not kill the peer"
    );
    rig.inject(&data_json_frame(&ack_request("after-logs")))
        .await;
    let response = rig
        .next_envelope(Duration::from_secs(5))
        .await
        .expect("peer responds after log junk");
    match response {
        XacppEnvelope::Response { id, .. } => assert_eq!(id, "after-logs"),
        other => panic!("expected Response, got {other:?}"),
    }
}

/// B.4 — mid-frame disconnect: a truncated frame leaves the decoder
/// suspended (connection alive, not dead), and the subsequent EOF tears it
/// down — the half frame never materializes as a phantom delivery.
#[tokio::test]
async fn b_half_frame_truncation_connection_fate() {
    let (a, b) = tokio::io::duplex(DUPLEX_BUF);
    let (_a_read, mut a_write) = tokio::io::split(a);
    let (b_read, b_write) = tokio::io::split(b);
    let transport = StdioTransport::new(
        Box::pin(b_write) as BoxedWriter,
        Box::pin(BufReader::new(b_read)) as BoxedReader,
    );
    transport.on_request(echo_handler()).expect("handler");
    let closed = transport.closed();

    // Truncated frame: 60% of a valid envelope frame.
    let full = data_json_frame(&ack_request("never-delivered"));
    let cut = full.len() * 6 / 10;
    a_write
        .write_all(&full[..cut])
        .await
        .expect("partial write");
    a_write.flush().await.expect("partial flush");

    transport.connect().await.expect("connect");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !*closed.borrow(),
        "decoder suspended mid-frame: connection alive, not dead"
    );

    // Half-close the pipe: EOF completes the reader, connection is torn down.
    a_write.shutdown().await.expect("half-close");
    let dead = wait_until(Duration::from_secs(5), || *closed.borrow()).await;
    assert!(
        dead,
        "EOF after truncated frame must tear the connection down"
    );
}

/// B.5 — size boundaries: each frame class passes at exactly its §4 limit
/// and rejects one byte over — on the encode side *and* on the
/// forged-header decode side (protocol-level validation before any body).
#[tokio::test]
async fn b_size_limits_exact_pass_and_plus_one_reject() {
    // Encode side: exact limits pass and round-trip through the decoder.
    for (frame_type, limit) in [
        (FrameType::Ack, MAX_CONTROL_FRAME_LEN),
        (FrameType::DataJson, MAX_DATA_JSON_FRAME_LEN),
        (FrameType::DataBinary, MAX_DATA_BINARY_FRAME_LEN),
    ] {
        let frame = Frame::new(frame_type, [1u8; 16], 0, 0, vec![7u8; limit]);
        let wire = frame
            .encode()
            .unwrap_or_else(|e| panic!("{frame_type:?}: exact limit {limit} must encode, got {e}"));
        let decoded = FrameDecoder::new()
            .feed(&wire)
            .unwrap_or_else(|e| panic!("{frame_type:?}: exact limit must decode, got {e}"));
        assert_eq!(decoded.len(), 1);
        assert_eq!(decoded[0].frame_type, frame_type);
        assert_eq!(decoded[0].payload.len(), limit);
    }

    // Decode side: forged headers declaring limit+1 are protocol errors
    // before any body arrives.
    for (frame_type, limit) in [
        (FrameType::Ack, MAX_CONTROL_FRAME_LEN),
        (FrameType::DataJson, MAX_DATA_JSON_FRAME_LEN),
        (FrameType::DataBinary, MAX_DATA_BINARY_FRAME_LEN),
    ] {
        let mut decoder = FrameDecoder::new();
        match decoder.feed(&forged_header(frame_type, 1, limit as u32 + 1)) {
            Err(FrameError::FrameTooLarge {
                frame_type: ft,
                max,
                ..
            }) => {
                assert_eq!(ft, frame_type);
                assert_eq!(max, limit);
            }
            other => panic!("{frame_type:?}: expected FrameTooLarge for limit+1, got {other:?}"),
        }
    }
}

/// CRC tamper wire helper: inject a frame with one byte flipped at `point`
/// (re-encoded with its now-wrong CRC intact), assert the peer connection
/// dies — the CRC is the primary integrity defense in plaintext mode
/// (SPEC §7) and a mismatch is fatal.
async fn crc_tamper_wire(point: &str) {
    let mut rig = Injector::new(b"").await;
    rig.connect_peer().await;

    let mut wire = data_json_frame(&ack_request("tampered"));
    let crc_start = wire.len() - 4;
    match point {
        "payload" => wire[HEADER_LEN] ^= 0xff,
        "header" => wire[8] ^= 0x01, // inside the transaction id field
        "trailer" => wire[crc_start] ^= 0xff,
        other => unreachable!("unknown tamper point {other}"),
    }
    rig.inject(&wire).await;

    let dead = wait_until(Duration::from_secs(5), || rig.peer_closed()).await;
    assert!(
        dead,
        "CRC tamper at {point} must be fatal for the connection"
    );
}

/// B.6 — CRC tamper, payload point.
#[tokio::test]
async fn b_crc_tamper_payload_kills_connection() {
    crc_tamper_wire("payload").await;
}

/// B.7 — CRC tamper, header point.
#[tokio::test]
async fn b_crc_tamper_header_kills_connection() {
    crc_tamper_wire("header").await;
}

/// B.8 — CRC tamper, trailer point.
#[tokio::test]
async fn b_crc_tamper_trailer_kills_connection() {
    crc_tamper_wire("trailer").await;
}

/// B.9 — version refusal on the wire: a frame speaking another frame-format
/// major is a fatal protocol error; the connection is refused (torn down).
#[tokio::test]
async fn b_version_major_mismatch_refuses_wire() {
    let mut rig = Injector::new(b"").await;
    rig.connect_peer().await;

    let wire = build_wire(FrameType::DataJson, 2, 0, [0u8; 16], 0, 0, b"{}");
    rig.inject(&wire).await;

    let dead = wait_until(Duration::from_secs(5), || rig.peer_closed()).await;
    assert!(dead, "major version mismatch must refuse the connection");
}

/// B.10 — hostile capability declaration: an unknown frame type whose
/// declared size blows past the decode buffer cap is rejected at header
/// validation, before any body is buffered.
#[tokio::test]
async fn b_hostile_unknown_cap_declared_rejected_wire() {
    let mut rig = Injector::new(b"").await;
    rig.connect_peer().await;

    // Header-only injection: validation fires at 36 bytes, so no body is
    // needed — memory exposure is bounded by construction.
    let header = forged_header(
        FrameType::Unknown(0x7fff),
        1,
        (MAX_DECODE_TOTAL_CAP + 1) as u32,
    );
    rig.inject(&header).await;

    let dead = wait_until(Duration::from_secs(5), || rig.peer_closed()).await;
    assert!(dead, "hostile cap declaration must kill the connection");

    // Decoder-level: the same header surfaces FrameTooLarge to the caller.
    let mut decoder = FrameDecoder::new();
    match decoder.feed(&header) {
        Err(FrameError::FrameTooLarge { frame_type, .. }) => {
            assert_eq!(frame_type, FrameType::Unknown(0x7fff));
        }
        other => panic!("expected FrameTooLarge, got {other:?}"),
    }
}

/// B.11 — retransmission-storm bounds: with the unacknowledged cap in
/// force and one working frame lost forever on the first transmission, the
/// transaction stalls paused — but control frames stay exempt (the
/// receiver's NACK reaches the sender, the sender's heartbeat reaches the
/// receiver) and the NACK-triggered retransmission completes the transfer.
/// After the final ACK releases the copy the cap is clear and new
/// transactions flow again (water-level recovery).
///
/// (New-transaction fast-fail at the cap itself is covered by
/// `backpressure_fails_fast_at_cap` in `transport_reliability_tests.rs`.)
#[tokio::test]
async fn b_backpressure_pause_control_exempt_and_recovery() {
    let mut cfg_a = test_cfg();
    cfg_a.sender.backpressure_cap_bytes = 4096; // payload exactly fills the cap → pause
    cfg_a.sender.rto_initial_ms = u64::MAX; // no timeout retransmission: NACK path only
    cfg_a.sender.rto_max_ms = u64::MAX;
    cfg_a.heartbeat_interval_ms = 100; // A keeps proving liveness while paused
    cfg_a.silent_timeout_ms = 10_000;
    let mut cfg_b = test_cfg();
    cfg_b.silent_timeout_ms = 1_000; // dies unless A's exempt heartbeats arrive
    cfg_b.nack_feedback_interval_ms = 100; // B reports the gap promptly
    cfg_b.ack_batch_interval_ms = 100;

    // Lossy A→B link: the first seq-1 DATA_BINARY frame never arrives.
    let (a, b) = tokio::io::duplex(DUPLEX_BUF);
    let (a_read, a_write) = tokio::io::split(a);
    let (b_read, b_write) = tokio::io::split(b);
    let lossy_a = StdioTransport::new(
        Box::pin(FrameDropper::new(a_write, 1)) as BoxedWriter,
        Box::pin(BufReader::new(a_read)) as BoxedReader,
    );
    let tb = StdioTransport::new(
        Box::pin(b_write) as BoxedWriter,
        Box::pin(BufReader::new(b_read)) as BoxedReader,
    );
    let session_a = Arc::new(XacppSession::new(Some(cfg_a)));
    let session_b = Arc::new(XacppSession::new(Some(cfg_b)));
    let a: Arc<StdioTransport> = Arc::new(lossy_a);
    let b: Arc<StdioTransport> = Arc::new(tb);
    session_a
        .wire(Arc::clone(&a) as Arc<dyn XacppTransport>)
        .await
        .unwrap();
    session_b
        .wire(Arc::clone(&b) as Arc<dyn XacppTransport>)
        .await
        .unwrap();
    a.connect().await.unwrap();
    b.connect().await.unwrap();
    let b_closed = b.closed();

    let mut delivered = capture_binary(&session_b).await;
    session_a
        .send_binary(pattern(4096), None)
        .await
        .expect("first transaction");

    // Recovery chain: NACK (control, exempt) → exempt retransmission of the
    // lost frame → completion → final ACK → copy released.
    let got = tokio::time::timeout(Duration::from_secs(10), delivered.recv())
        .await
        .expect("recovery within timeout")
        .expect("channel open");
    assert_eq!(got.len(), 4096);
    assert!(got == pattern(4096), "recovered bytes identical");

    // B stayed alive throughout: A's heartbeats flowed while A was paused
    // (control frames are exempt from the backpressure pause).
    assert!(
        !*b_closed.borrow(),
        "receiver must stay alive on exempt control traffic"
    );

    // Water-level recovery: the cap is clear, new transactions flow.
    let first = session_a
        .send_binary(pattern(16), None)
        .await
        .expect("cap released by final ACK: new transaction accepted");
    let _ = first;
    let got = tokio::time::timeout(Duration::from_secs(10), delivered.recv())
        .await
        .expect("second delivery within timeout")
        .expect("channel open");
    assert_eq!(got.len(), 16);
}

// =====================================================================================
// C — Error-semantics ledger
//
// Tolerated column: the connection survives and keeps working.
// Surfaced column: the error is visible (to the caller, or as connection
// death) — never swallowed.
// =====================================================================================

/// C.1 (tolerated) — an unknown frame type with a valid CRC is ignored per
/// SPEC §9 (minor-version tolerance); the connection keeps working
/// bidirectionally afterwards.
#[tokio::test]
async fn c_unknown_frame_type_crc_valid_ignored_wire() {
    let mut rig = Injector::new(b"").await;
    rig.connect_peer().await;

    let unknown = build_wire(
        FrameType::Unknown(0x7fff),
        1,
        0,
        [9u8; 16],
        0,
        0,
        b"future-payload",
    );
    rig.inject(&unknown).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!rig.peer_closed(), "unknown frame type must be ignored");

    // Connection still works in both directions.
    rig.inject(&data_json_frame(&ack_request("after-unknown")))
        .await;
    let response = rig
        .next_envelope(Duration::from_secs(5))
        .await
        .expect("peer still responds");
    match response {
        XacppEnvelope::Response { id, .. } => assert_eq!(id, "after-unknown"),
        other => panic!("expected Response, got {other:?}"),
    }
}

/// C.2 (tolerated) — a higher minor version on a perfectly valid frame is
/// tolerated: the frame is processed normally (SPEC §9).
#[tokio::test]
async fn c_higher_minor_version_tolerated_wire() {
    let mut rig = Injector::new(b"").await;
    rig.connect_peer().await;

    let envelope = ack_request("high-minor");
    let wire = build_wire(
        FrameType::DataJson,
        1,
        9,
        [0u8; 16],
        0,
        0,
        &envelope_bytes(&envelope),
    );
    rig.inject(&wire).await;
    let response = rig
        .next_envelope(Duration::from_secs(5))
        .await
        .expect("higher-minor frame processed");
    match response {
        XacppEnvelope::Response { id, .. } => assert_eq!(id, "high-minor"),
        other => panic!("expected Response, got {other:?}"),
    }
}

/// C.3 (tolerated) — duplicate `DATA_BINARY` frames are idempotent on the
/// receive side while the transaction is incomplete: retransmission copies
/// injected raw on the wire never produce an early or second delivery.
/// (The completed-transaction replay guard lives at the fetch extension
/// layer: `ext10_completed_replay_not_redelivered`.)
#[tokio::test]
async fn c_duplicate_binary_frames_idempotent_incomplete_wire() {
    let session = Arc::new(XacppSession::new(Some(quiet_cfg())));
    let mut rig = Injector::new(b"").await;
    rig.wire_peer(&session).await;
    rig.connect_peer().await;

    let mut delivered = capture_binary(&session).await;

    // Hand-craft a 2-frame transaction directly onto the wire and inject
    // duplicates of seq 0 before the completing frame.
    let txid = [0x42u8; 16];
    let body = pattern(2048);
    let seq0 = Frame::new(
        FrameType::DataBinary,
        txid,
        body.len() as u32,
        0,
        body[..1024].to_vec(),
    )
    .encode()
    .expect("seq0 encodes");
    let seq1 = Frame::new(
        FrameType::DataBinary,
        txid,
        body.len() as u32,
        1,
        body[1024..].to_vec(),
    )
    .encode()
    .expect("seq1 encodes");

    rig.inject(&seq0).await;
    rig.inject(&seq0).await;
    rig.inject(&seq0).await; // duplicate retransmission copies
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        delivered.try_recv().is_err(),
        "incomplete transaction must not deliver early"
    );
    rig.inject(&seq1).await;

    let got = tokio::time::timeout(Duration::from_secs(5), delivered.recv())
        .await
        .expect("delivery within timeout")
        .expect("channel open");
    assert!(got == body, "completed bytes identical");
}

/// C.4 (tolerated) — a `DATA_JSON` frame whose payload is not a valid
/// envelope is skipped with a warning (peer-side bug containment); the
/// connection keeps working.
#[tokio::test]
async fn c_invalid_envelope_payload_skipped_wire() {
    let mut rig = Injector::new(b"").await;
    rig.connect_peer().await;

    rig.inject(&build_wire(
        FrameType::DataJson,
        1,
        0,
        [0u8; 16],
        0,
        0,
        b"this is not json at all",
    ))
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!rig.peer_closed(), "invalid envelope must be skipped");

    rig.inject(&data_json_frame(&ack_request("after-invalid")))
        .await;
    let response = rig
        .next_envelope(Duration::from_secs(5))
        .await
        .expect("peer still responds after invalid envelope");
    match response {
        XacppEnvelope::Response { id, .. } => assert_eq!(id, "after-invalid"),
        other => panic!("expected Response, got {other:?}"),
    }
}

// ---- Surfaced column ----

/// C.5 (surfaced) — every fatal `FrameError` variant is visible to the
/// caller with its exact identity: `VersionMismatch`, `CrcMismatch`, and
/// `FrameTooLarge` each surface as themselves from the decoder (never
/// coalesced into a generic error). Connection-level consequences (teardown
/// on each) are covered by B.6–B.10.
#[tokio::test]
async fn c_decoder_fatal_variants_surfaced() {
    // VersionMismatch: wrong major in the header.
    let mut decoder = FrameDecoder::new();
    let bad_major = build_wire(FrameType::DataJson, 3, 0, [0u8; 16], 0, 0, b"{}");
    match decoder.feed(&bad_major) {
        Err(FrameError::VersionMismatch { got, expected }) => {
            assert_eq!(got, 3);
            assert_eq!(expected, 1);
        }
        other => panic!("expected VersionMismatch, got {other:?}"),
    }

    // CrcMismatch: valid frame with a flipped trailer byte.
    let mut decoder = FrameDecoder::new();
    let mut wire = data_json_frame(&ack_request("crc"));
    let crc_start = wire.len() - 4;
    wire[crc_start] ^= 0xff;
    match decoder.feed(&wire) {
        Err(FrameError::CrcMismatch { expected, actual }) => {
            assert_ne!(expected, actual, "flipped trailer byte must alter the CRC");
        }
        other => panic!("expected CrcMismatch, got {other:?}"),
    }

    // FrameTooLarge: forged header over the per-type limit.
    let mut decoder = FrameDecoder::new();
    match decoder.feed(&forged_header(
        FrameType::DataJson,
        1,
        MAX_DATA_JSON_FRAME_LEN as u32 + 1,
    )) {
        Err(FrameError::FrameTooLarge { .. }) => {}
        other => panic!("expected FrameTooLarge, got {other:?}"),
    }
}

/// C.6 (surfaced) — after a fatal CRC error the decoder's buffered state is
/// drained: the failed frame's body bytes cannot smear into the next
/// frame's framing, and parsing resumes cleanly. (The caller must still
/// discard the decoder together with the stream per the frame-module
/// contract; this pins the no-smear behavior the drain guarantees.)
#[tokio::test]
async fn c_decoder_drained_after_fatal_no_smear() {
    let mut decoder = FrameDecoder::new();

    // Fatal: CRC mismatch over a complete frame (body was buffered).
    let mut bad = data_json_frame(&ack_request("bad-crc"));
    let crc_start = bad.len() - 4;
    bad[crc_start] ^= 0xff;
    assert!(matches!(
        decoder.feed(&bad),
        Err(FrameError::CrcMismatch { .. })
    ));

    // The very next bytes on the stream are a valid frame: with the drained
    // state it parses cleanly — no residue, no phantom framing, no replay
    // of the failed frame.
    let good = data_json_frame(&ack_request("clean"));
    let frames = decoder.feed(&good).expect("clean parse after fatal");
    assert_eq!(frames.len(), 1);
    assert_eq!(
        serde_json::to_value(serde_json::from_slice::<XacppEnvelope>(&frames[0].payload).unwrap())
            .unwrap(),
        serde_json::to_value(ack_request("clean")).unwrap()
    );
}

/// C.7 (surfaced) — connection-type `Err` semantics: after a local
/// disconnect every outbound path reports a connection error immediately
/// (never a silent success), matching the transport-layer error convention.
/// The in-flight cancellation path (`Closed` on a pending request when the
/// peer dies) is covered by `test_inflight_request_cancelled_on_disconnect`
/// and `test_disconnect_aborts_inflight_no_deadlock`.
#[tokio::test]
async fn c_connection_closed_errors_after_disconnect() {
    let (ta, tb) = duplex_pair();
    let tb: Arc<dyn XacppTransport> = Arc::new(tb);
    tb.on_request(echo_handler()).expect("handler");
    tb.connect().await.unwrap();
    let ta: Arc<dyn XacppTransport> = Arc::new(ta);
    ta.connect().await.unwrap();

    // Sanity: the link works before the disconnect.
    ta.send(
        None,
        XacppRequest::Command(XacppCommand::generic("probe", json!({}))),
    )
    .await
    .expect("pre-disconnect round trip");

    ta.disconnect().await.unwrap();

    let err = ta
        .send(
            None,
            XacppRequest::Command(XacppCommand::generic("after", json!({}))),
        )
        .await
        .expect_err("send after disconnect must fail");
    assert!(
        matches!(err, XacppError::NotConnected | XacppError::Closed),
        "connection-type error expected, got {err:?}"
    );

    let err = ta
        .send_faf(
            None,
            XacppRequest::Command(XacppCommand::generic("after-faf", json!({}))),
        )
        .await
        .expect_err("send_faf after disconnect must fail");
    assert!(
        matches!(err, XacppError::NotConnected | XacppError::Closed),
        "connection-type error expected, got {err:?}"
    );

    // The same connection-type semantics for the session's binary path:
    // wire a session, disconnect, then send_binary reports the dead link.
    let session = XacppSession::new(None);
    session
        .wire(Arc::clone(&ta))
        .await
        .expect("wire before disconnect");
    ta.disconnect().await.unwrap();
    let err = session
        .send_binary(vec![1, 2, 3], None)
        .await
        .expect_err("send_binary after disconnect must fail");
    assert!(
        matches!(
            err,
            XacppError::NotConnected | XacppError::Closed | XacppError::Internal(_)
        ),
        "connection-type error expected, got {err:?}"
    );
}

/// C.8 (surfaced) — `send_binary` on an unwired session reports
/// NotConnected (never silently drops the payload).
#[tokio::test]
async fn c_send_binary_unwired_reports_not_connected() {
    let session = XacppSession::new(None);
    let err = session
        .send_binary(vec![1, 2, 3], None)
        .await
        .expect_err("unwired send_binary must fail");
    assert!(matches!(err, XacppError::NotConnected), "got {err:?}");
}
