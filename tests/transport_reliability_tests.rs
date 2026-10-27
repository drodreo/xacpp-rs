//! Transport-integration e2e tests for the transfer session (SPEC §5).
//!
//! All tests run over memory duplexes (`StdioTransport`), mirroring
//! `peer_e2e_tests::duplex_pair`. Timing-sensitive scenarios use shrunk
//! configs (RTO / heartbeat / timeouts) with real-clock waiting.

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};

use xacpp::carrier::XacppTransport;
use xacpp::carrier::stdio::StdioTransport;
use xacpp::reliability::transaction::SenderConfig;
use xacpp::session::XacppSession;
use xacpp::transfer::TransferConfig;
use xacpp::wire::commands::XacppCommand;
use xacpp::wire::control::{
    AckEntry, AckPayload, ErrorPayload, ResumeRequestPayload, ResumeResponsePayload, ResumeStatus,
    ResumeStatusEntry, error_codes,
};
use xacpp::wire::error::XacppError;
use xacpp::wire::frame::{Frame, FrameType};
use xacpp::wire::message::{XacppEnvelope, XacppRequest, XacppResponse};

// ---- Test config ----

/// Shrunk timing: 1 KiB frames, 200 ms initial RTO, fast clock.
fn test_config() -> TransferConfig {
    TransferConfig {
        sender: SenderConfig {
            frame_size: 1024,
            rto_initial_ms: 200,
            rto_max_ms: 800,
            backpressure_cap_bytes: 64 * 1024 * 1024,
        },
        heartbeat_interval_ms: 30_000,
        silent_timeout_ms: 4_000,
        ack_batch_frames: 64,
        ack_batch_interval_ms: 100,
        nack_feedback_interval_ms: 100,
        resume_verdict_timeout_ms: 300,
        tick_interval_ms: 20,
    }
}

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

// ---- Memory duplex pair ----

const DUPLEX_BUF: usize = 64 * 1024;

type BoxedWriter = Pin<Box<dyn AsyncWrite + Send>>;
type BoxedReader = Pin<Box<dyn AsyncBufRead + Send>>;

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

/// Create, wire and connect both sides: returns the transports plus the
/// logical sessions owning the transfer state.
async fn connected_reliable_pair(
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
    // Connect first, then wire: wiring attaches the session to a live
    // connection (clock starts, RESUME handshake runs through the write
    // surface).
    ta.connect().await.unwrap();
    tb.connect().await.unwrap();
    session_a
        .wire(Arc::clone(&ta) as Arc<dyn XacppTransport>)
        .await
        .unwrap();
    session_b
        .wire(Arc::clone(&tb) as Arc<dyn XacppTransport>)
        .await
        .unwrap();
    (ta, session_a, tb, session_b)
}

// ---- Frame-dropping writer (lossy link emulator) ----

const MAGIC: &[u8; 4] = b"XACP";
const HEADER_LEN: usize = 36;
const CRC_LEN: usize = 4;
const FRAME_TYPE_DATA_BINARY: u16 = 0x1002;
const FRAME_TYPE_TRANSACTION_COMPLETED: u16 = 0x000b;

/// Writer wrapper that scans the outbound byte stream at frame boundaries
/// and silently drops the **first** `DATA_BINARY` frame with `target_seq`
/// (of any transaction) — a lossy-link emulator for the retransmission and
/// RESUME tests (`target_seq = None` = full passthrough).
struct FrameDropper<W> {
    inner: W,
    /// Bytes not yet classified into complete frames.
    scan: Vec<u8>,
    /// Classified bytes waiting to reach `inner`.
    out: Vec<u8>,
    target_seq: Option<u32>,
    drop_armed: bool,
    drop_completed: bool,
}

impl<W: AsyncWrite + Unpin> FrameDropper<W> {
    fn new(inner: W, target_seq: Option<u32>) -> Self {
        Self {
            inner,
            scan: Vec::new(),
            out: Vec::new(),
            target_seq,
            drop_armed: true,
            drop_completed: false,
        }
    }

    /// Move complete frames from `scan` to `out`, dropping the armed target.
    fn classify(&mut self) {
        let Some(_target_seq) = self.target_seq else {
            self.out.append(&mut self.scan);
            return;
        };
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
            if self.drop_armed && ftype == FRAME_TYPE_DATA_BINARY && Some(seq) == self.target_seq {
                self.drop_armed = false;
                // The loss window swallows the completion announcement too:
                // the completed frame rides the data FIFO right behind the
                // transaction's frames, and a link that drops frames drops
                // it like any other — the peer then completes on the byte
                // count (the announcement is not mandatory).
                self.drop_completed = true;
                log::debug!("FrameDropper: dropped DATA_BINARY seq {seq}");
            } else if self.drop_completed && ftype == FRAME_TYPE_TRANSACTION_COMPLETED {
                self.drop_completed = false;
                log::debug!("FrameDropper: dropped TRANSACTION_COMPLETED (same loss window)");
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
            // Bytes are internally buffered; the write is accepted in full
            // even while `inner` backpressures (flushed on a later poll).
            Poll::Pending => Poll::Ready(Ok(buf.len())),
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let this = &mut *self;
        if this.forward(cx)?.is_pending() {
            return Poll::Pending;
        }
        Pin::new(&mut this.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let this = &mut *self;
        if this.forward(cx)?.is_pending() {
            return Poll::Pending;
        }
        Pin::new(&mut this.inner).poll_shutdown(cx)
    }
}

// ---- Handlers ----

/// Replies `acknowledge` to every request.
fn echo_handler() -> xacpp::session::handler::RequestHandler {
    Arc::new(|_session_id, _request| {
        Box::pin(async { Ok(XacppResponse::acknowledge()) })
            as Pin<Box<dyn std::future::Future<Output = Result<XacppResponse, XacppError>> + Send>>
    })
}

// ---- Tests ----

/// >1 MiB payload: many working frames + tail frame, byte-for-byte delivery.
#[tokio::test]
async fn large_payload_roundtrip_multi_frame() {
    let mut cfg_a = test_config();
    cfg_a.sender.frame_size = 256 * 1024; // 2 MiB → 8 frames + tail
    let (_a, session_a, _b, session_b) = connected_reliable_pair(cfg_a, test_config()).await;

    let (delivered_tx, mut delivered_rx) = tokio::sync::mpsc::unbounded_channel();
    session_b
        .on_binary(Arc::new(move |txid, payload| {
            let _ = delivered_tx.send((txid, payload));
        }))
        .await;

    let payload = pattern(2 * 1024 * 1024 + 137);
    let txid = session_a.send_binary(payload.clone(), None).await.unwrap();

    let (got_txid, got) = tokio::time::timeout(Duration::from_secs(15), delivered_rx.recv())
        .await
        .expect("delivery within timeout")
        .expect("channel open");
    assert_eq!(got_txid, txid, "txid identity preserved end-to-end");
    assert_eq!(got.len(), payload.len(), "length exact");
    assert!(got == payload, "bytes equal");
}

/// First transmission of one working frame is swallowed by a lossy link;
/// NACK feedback and/or RTO retransmission recovers, delivery completes.
#[tokio::test]
async fn lost_frame_recovered_by_retransmission() {
    let mut cfg = test_config();
    cfg.nack_feedback_interval_ms = 60;
    cfg.sender.rto_initial_ms = 150;

    // Lossy A→B link: drop the first DATA_BINARY seq=1 frame.
    let (a, b) = tokio::io::duplex(DUPLEX_BUF);
    let (a_read, a_write) = tokio::io::split(a);
    let (b_read, b_write) = tokio::io::split(b);
    let lossy_a: StdioTransport = StdioTransport::new(
        Box::pin(FrameDropper::new(a_write, Some(1))) as BoxedWriter,
        Box::pin(BufReader::new(a_read)) as BoxedReader,
    );
    let b_transport: StdioTransport = StdioTransport::new(
        Box::pin(b_write) as BoxedWriter,
        Box::pin(BufReader::new(b_read)) as BoxedReader,
    );

    let session_a = Arc::new(XacppSession::new(Some(cfg)));
    let session_b = Arc::new(XacppSession::new(Some(cfg)));
    let a: Arc<StdioTransport> = Arc::new(lossy_a);
    let b: Arc<StdioTransport> = Arc::new(b_transport);
    // Connect first, then wire (attach to a live connection).
    a.connect().await.unwrap();
    b.connect().await.unwrap();
    session_a
        .wire(Arc::clone(&a) as Arc<dyn XacppTransport>)
        .await
        .unwrap();
    session_b
        .wire(Arc::clone(&b) as Arc<dyn XacppTransport>)
        .await
        .unwrap();

    let (delivered_tx, mut delivered_rx) = tokio::sync::mpsc::unbounded_channel();
    session_b
        .on_binary(Arc::new(move |txid, payload| {
            let _ = delivered_tx.send((txid, payload));
        }))
        .await;

    // 4 KiB at 1 KiB frames → seq 0..4; seq 1 first transmission is dropped.
    let payload = pattern(4096);
    session_a.send_binary(payload.clone(), None).await.unwrap();

    let got = tokio::time::timeout(Duration::from_secs(10), delivered_rx.recv())
        .await
        .expect("delivery within timeout (retransmission path)")
        .expect("channel open");
    assert_eq!(got.1.len(), payload.len(), "length exact");
    assert!(got.1 == payload, "bytes equal after recovery");
}

/// Transfer interrupted mid-way, both sides reconnect with the *same*
/// logical sessions wired to fresh transports: the transfer state lives on
/// the session, so the recovery handshake bridges the connection boundary
/// and the transfer completes — breakpoint resume is the natural
/// consequence of session-owned state.
///
/// "Mid-way" is manufactured deterministically: connection 1 drops the
/// seq-3 working frame and both retransmission paths are disabled (no NACK
/// feedback on the receiver, no RTO on the sender), so the receiver holds
/// frames 0..2 (contiguous prefix, transaction incomplete) until the
/// disconnect. On connection 2 the holder's wire emits the RESUME
/// declaration (its send state intact on the same session), the receiver
/// adjudicates the txid as still open and answers `Resumed` (the resume
/// instruction), and the holder lifts its gate and retransmits the missing
/// frame.
#[tokio::test]
async fn resume_across_connections() {
    let mut cfg_a = test_config();
    cfg_a.sender.rto_initial_ms = u64::MAX; // conn 1: never RTO-retransmit
    cfg_a.sender.rto_max_ms = u64::MAX;
    let mut cfg_b = test_config();
    cfg_b.nack_feedback_interval_ms = u64::MAX; // conn 1: never NACK

    // Sessions outlive the connections they are wired to.
    let session_a = Arc::new(XacppSession::new(Some(cfg_a)));
    let session_b = Arc::new(XacppSession::new(Some(cfg_b)));

    // Connection 1: lossy A→B link (drops the seq-3 DATA_BINARY frame).
    let (a, b) = tokio::io::duplex(DUPLEX_BUF);
    let (a_read, a_write) = tokio::io::split(a);
    let (b_read, b_write) = tokio::io::split(b);
    let lossy_a: StdioTransport = StdioTransport::new(
        Box::pin(FrameDropper::new(a_write, Some(3))) as BoxedWriter,
        Box::pin(BufReader::new(a_read)) as BoxedReader,
    );
    let tb1: StdioTransport = StdioTransport::new(
        Box::pin(b_write) as BoxedWriter,
        Box::pin(BufReader::new(b_read)) as BoxedReader,
    );
    let a1: Arc<StdioTransport> = Arc::new(lossy_a);
    let tb1: Arc<StdioTransport> = Arc::new(tb1);
    // Connect first, then wire (attach to a live connection).
    a1.connect().await.unwrap();
    tb1.connect().await.unwrap();
    session_a
        .wire(Arc::clone(&a1) as Arc<dyn XacppTransport>)
        .await
        .unwrap();
    session_b
        .wire(Arc::clone(&tb1) as Arc<dyn XacppTransport>)
        .await
        .unwrap();

    let (delivered_tx, mut delivered_rx) = tokio::sync::mpsc::unbounded_channel();
    session_b
        .on_binary(Arc::new(move |txid, payload| {
            let _ = delivered_tx.send((txid, payload));
        }))
        .await;

    let txid = session_a.send_binary(pattern(4096), None).await.unwrap();

    // Wait out a quiet window for the deterministic partial state: frames
    // 0..2 stored on B (a memory duplex delivers them within milliseconds —
    // the window is a wide safety margin), seq 3 dropped once and both
    // retransmission paths disabled, so nothing else moves on this
    // connection. (The old internal-state probe is gone: the observable
    // verdict is the resumed delivery after the reconnect below.)
    tokio::time::sleep(Duration::from_secs(1)).await;
    a1.disconnect().await.unwrap();
    drop(a1);
    drop(tb1);

    // Connection 2: fresh clean transports, the SAME sessions wired again —
    // the holder's wire emits the RESUME declaration, the receiver (its
    // incomplete slot intact on the same session) answers `Resumed`, and
    // the holder retransmits.
    let (ta2, tb2) = duplex_pair();
    let ta2: Arc<StdioTransport> = Arc::new(ta2);
    let tb2: Arc<StdioTransport> = Arc::new(tb2);
    tb2.connect().await.unwrap();
    session_b
        .wire(Arc::clone(&tb2) as Arc<dyn XacppTransport>)
        .await
        .unwrap();
    ta2.connect().await.unwrap();
    session_a
        .wire(Arc::clone(&ta2) as Arc<dyn XacppTransport>)
        .await
        .unwrap();

    let got = tokio::time::timeout(Duration::from_secs(10), delivered_rx.recv())
        .await
        .expect("delivery within timeout (resume path)")
        .expect("channel open");
    assert_eq!(got.1.len(), 4096, "resumed length exact");
    assert!(got.1 == pattern(4096), "resumed bytes equal");
    let _ = txid;
}

/// Backpressure: at the unacknowledged-bytes cap `send_binary` fails fast
/// with `backpressure_stall` and never blocks the caller.
#[tokio::test]
async fn backpressure_fails_fast_at_cap() {
    let mut cfg_a = test_config();
    cfg_a.sender.backpressure_cap_bytes = 2048; // exactly one full payload
    // Peer's ACK export disabled (batching window infinite) → no ACKs ever
    // arrive → the cap stays hit.
    let mut cfg_b = test_config();
    cfg_b.ack_batch_interval_ms = u64::MAX;
    cfg_b.ack_batch_frames = u32::MAX;
    let (ta, tb) = duplex_pair();
    let session_a = Arc::new(XacppSession::new(Some(cfg_a)));
    let session_b = Arc::new(XacppSession::new(Some(cfg_b)));
    let a: Arc<StdioTransport> = Arc::new(ta);
    let b: Arc<StdioTransport> = Arc::new(tb);
    // Connect first, then wire (attach to a live connection).
    a.connect().await.unwrap();
    b.connect().await.unwrap();
    session_a
        .wire(Arc::clone(&a) as Arc<dyn XacppTransport>)
        .await
        .unwrap();
    session_b
        .wire(Arc::clone(&b) as Arc<dyn XacppTransport>)
        .await
        .unwrap();

    session_a.send_binary(pattern(2048), None).await.unwrap();

    let second = session_a.send_binary(pattern(16), None).await.unwrap_err();
    match second {
        XacppError::Application { code, .. } => {
            assert_eq!(code, "backpressure_stall", "expected stall error code");
        }
        other => panic!("expected backpressure Application error, got {other:?}"),
    }
}

/// One side sends heartbeats, the other stays silent: the side receiving
/// nothing declares the connection dead (watch flips true).
#[tokio::test]
async fn silent_peer_times_out_connection() {
    let mut cfg_a = test_config();
    cfg_a.heartbeat_interval_ms = 50; // A keeps proving liveness
    cfg_a.silent_timeout_ms = 250; // A receives nothing from B → A dies
    let mut cfg_b = test_config();
    cfg_b.heartbeat_interval_ms = u64::MAX; // B never sends anything
    cfg_b.silent_timeout_ms = 10_000; // B receives A's heartbeats → alive

    let (a, _sa, b, _sb) = connected_reliable_pair(cfg_a, cfg_b).await;

    let a_closed = a.closed();
    let b_closed = b.closed();

    // A receives nothing from B → A goes silent-timeout and kills the link.
    let a_dead = wait_until(Duration::from_secs(5), || *a_closed.borrow()).await;
    assert!(
        a_dead,
        "silent sender (no inbound frames) must be declared dead"
    );

    // Killing A's side tears the duplex down; B observes EOF → closed too.
    let b_dead = wait_until(Duration::from_secs(5), || *b_closed.borrow()).await;
    assert!(b_dead, "peer side observes the teardown");
}

/// The envelope channel is untouched by the transfer wiring: wired
/// sessions keep request/response flowing; an unwired session rejects
/// `send_binary` with NotConnected (never silently drops the payload).
#[tokio::test]
async fn envelope_channel_regression_with_bound_session() {
    // Handler must be registered before connect (transport contract).
    let (ta, tb) = duplex_pair();
    let session_a = Arc::new(XacppSession::new(Some(test_config())));
    let session_b = Arc::new(XacppSession::new(Some(test_config())));
    let ta: Arc<StdioTransport> = Arc::new(ta);
    let tb: Arc<StdioTransport> = Arc::new(tb);
    tb.on_request(echo_handler()).unwrap();
    // Connect first, then wire (attach to a live connection).
    ta.connect().await.unwrap();
    tb.connect().await.unwrap();
    session_a
        .wire(Arc::clone(&ta) as Arc<dyn XacppTransport>)
        .await
        .unwrap();
    session_b
        .wire(Arc::clone(&tb) as Arc<dyn XacppTransport>)
        .await
        .unwrap();

    let request = XacppRequest::Command(XacppCommand::generic("ping", serde_json::json!({})));
    let response = ta
        .send(None, request)
        .await
        .expect("envelope round trip with a wired session");
    match response {
        XacppResponse::Generic { name, .. } => {
            assert_eq!(name, "acknowledge", "echo handler replied");
        }
        other => panic!("expected Generic response, got {other:?}"),
    }

    // Unwired session: send_binary reports NotConnected.
    let unwired = XacppSession::new(None);
    let err = unwired.send_binary(vec![1, 2, 3], None).await.unwrap_err();
    assert!(matches!(err, XacppError::NotConnected));
}

/// Frame-type-driven dispatch (SPEC §5): a RESUME_RESPONSE payload
/// arriving under the RESUME (`0x0004`) frame type is a malformed request
/// — dropped with an error log, never guessed at (no payload-shape
/// sniffing), no answer sent, connection stays up.
#[tokio::test]
async fn mismatched_resume_payload_frame_type_dropped() {
    // Quiet config: no heartbeats or clock feedback inside the observation
    // window, so "no answer frame" is observable.
    let mut cfg = test_config();
    cfg.heartbeat_interval_ms = u64::MAX;
    cfg.silent_timeout_ms = 60_000;
    cfg.ack_batch_interval_ms = u64::MAX;
    cfg.nack_feedback_interval_ms = u64::MAX;

    let (a, b) = tokio::io::duplex(DUPLEX_BUF);
    let (mut a_read, mut a_write) = tokio::io::split(a);
    let (b_read, b_write) = tokio::io::split(b);
    let transport: Arc<StdioTransport> = Arc::new(StdioTransport::new(
        Box::pin(b_write) as BoxedWriter,
        Box::pin(BufReader::new(b_read)) as BoxedReader,
    ));
    let session = Arc::new(XacppSession::new(Some(cfg)));
    transport.connect().await.unwrap();
    session
        .wire(Arc::clone(&transport) as Arc<dyn XacppTransport>)
        .await
        .unwrap();
    let closed = transport.closed();

    // Hand-crafted mismatch: response payload (17-byte entries) under the
    // request frame type — request decoding must reject it.
    let payload = ResumeResponsePayload {
        entries: vec![ResumeStatusEntry {
            transaction_id: [7u8; 16],
            status: ResumeStatus::Resumed,
        }],
    }
    .encode()
    .unwrap();
    let frame = Frame::new(FrameType::Resume, [0u8; 16], 0, 0, payload)
        .encode()
        .unwrap();
    a_write.write_all(&frame).await.unwrap();
    a_write.flush().await.unwrap();

    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !*closed.borrow(),
        "malformed control frame must not kill the connection"
    );
    let mut probe = [0u8; 1];
    let got = tokio::time::timeout(Duration::from_millis(200), a_read.read(&mut probe)).await;
    assert!(
        got.is_err(),
        "no answer frame expected for a dropped malformed request"
    );
}

/// ERROR `0x0002` with the structured 32-lowercase-hex id prefix releases
/// the named receive slot; malformed prefixes (uppercase / non-hex) match
/// nothing and release nothing. Behaviorally observable through delivery:
/// a released slot cannot be completed by the remaining frames (they start
/// above a gap with no prefix), while a retained slot completes and
/// delivers normally.
#[tokio::test]
async fn error_transaction_expired_hex_prefix_discards_slot() {
    let mut cfg = test_config();
    cfg.heartbeat_interval_ms = u64::MAX;
    cfg.silent_timeout_ms = 60_000;
    cfg.ack_batch_interval_ms = u64::MAX;
    cfg.nack_feedback_interval_ms = u64::MAX;

    let (a, b) = tokio::io::duplex(DUPLEX_BUF);
    let (mut a_read, mut a_write) = tokio::io::split(a);
    let (b_read, b_write) = tokio::io::split(b);
    let transport: Arc<StdioTransport> = Arc::new(StdioTransport::new(
        Box::pin(b_write) as BoxedWriter,
        Box::pin(BufReader::new(b_read)) as BoxedReader,
    ));
    let session = Arc::new(XacppSession::new(Some(cfg)));
    transport.connect().await.unwrap();
    session
        .wire(Arc::clone(&transport) as Arc<dyn XacppTransport>)
        .await
        .unwrap();
    let _ = &mut a_read;

    let (delivered_tx, mut delivered_rx) = tokio::sync::mpsc::unbounded_channel();
    session
        .on_binary(Arc::new(move |txid, payload| {
            let _ = delivered_tx.send((txid, payload));
        }))
        .await;

    // Feed a partial DATA_BINARY transaction (seq 0 of a 3-frame one) and
    // then the remaining frames (seq 1..2). If the ERROR released the slot,
    // the tail frames start above a gap with no prefix — the transaction
    // can never complete (no delivery). If the ERROR failed to release it,
    // the slot completes and delivers.
    async fn feed_partial(a_write: &mut (impl AsyncWrite + Unpin), txid: [u8; 16]) {
        let frame = Frame::new(FrameType::DataBinary, txid, 3072, 0, vec![1u8; 1024])
            .encode()
            .unwrap();
        a_write.write_all(&frame).await.unwrap();
        a_write.flush().await.unwrap();
    }
    async fn feed_tail(a_write: &mut (impl AsyncWrite + Unpin), txid: [u8; 16]) {
        for seq in 1..3u32 {
            let frame = Frame::new(FrameType::DataBinary, txid, 3072, seq, vec![1u8; 1024])
                .encode()
                .unwrap();
            a_write.write_all(&frame).await.unwrap();
        }
        a_write.flush().await.unwrap();
    }
    async fn feed_error(a_write: &mut (impl AsyncWrite + Unpin), payload: Vec<u8>) {
        let frame = Frame::new(FrameType::Error, [0u8; 16], 0, 0, payload)
            .encode()
            .unwrap();
        a_write.write_all(&frame).await.unwrap();
        a_write.flush().await.unwrap();
    }

    // Negative case first (uppercase hex is not the lowercase format — no
    // match, the slot stays): the transaction completes and delivers.
    let txid2 = [11u8; 16];
    feed_partial(&mut a_write, txid2).await;
    let bad = ErrorPayload {
        code: error_codes::TRANSACTION_EXPIRED,
        message: "ABCDEF0123456789ABCDEF0123456789".to_string(),
    }
    .encode()
    .unwrap();
    feed_error(&mut a_write, bad).await;
    feed_tail(&mut a_write, txid2).await;
    let got = tokio::time::timeout(Duration::from_secs(3), delivered_rx.recv())
        .await
        .expect("retained slot must complete and deliver")
        .expect("delivery channel open");
    assert_eq!(got.0, txid2, "the malformed prefix released nothing");
    assert_eq!(
        got.1.len(),
        3072,
        "payload assembled from the retained slot"
    );

    // Positive case (exact 32-lowercase-hex prefix releases the slot): the
    // tail frames can never complete the released transaction — no
    // delivery within the window.
    let txid = [9u8; 16];
    feed_partial(&mut a_write, txid).await;
    let good = ErrorPayload {
        code: error_codes::TRANSACTION_EXPIRED,
        message: uuid::Uuid::from_bytes(txid).simple().to_string(),
    }
    .encode()
    .unwrap();
    feed_error(&mut a_write, good).await;
    feed_tail(&mut a_write, txid).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        delivered_rx.try_recv().is_err(),
        "structured hex prefix must release the slot: no delivery possible from a gap"
    );
}

/// Delivery semantics vs handler presence: the final ACK is emitted on
/// completion regardless of a registered binary handler (§5: "a final ACK
/// on completion"); a payload completed without a handler is retained (not
/// silently dropped) and handed over exactly once once a handler registers.
/// A follow-up transaction takes the normal immediate path — the deferred
/// drain never duplicates either delivery.
#[tokio::test]
async fn completed_without_handler_retained_then_delivered_exactly_once() {
    let (_a, session_a, _b, session_b) =
        connected_reliable_pair(test_config(), test_config()).await;

    // A observes the final ACK via the send completion callback: its firing
    // proves B completed the transaction and emitted the final ACK; the
    // result reports the transaction id as confirmed.
    let payload = pattern(4096);
    let (acked_tx, acked_rx) = tokio::sync::oneshot::channel();
    let txid = session_a
        .send_binary(
            payload.clone(),
            Some(Box::new(move |result| {
                assert!(result.confirmed, "final ACK must confirm the delivery");
                let _ = acked_tx.send(result.transaction_id);
            })),
        )
        .await
        .unwrap();

    let acked = tokio::time::timeout(Duration::from_secs(10), acked_rx)
        .await
        .expect("final ACK within timeout")
        .expect("completion channel open");
    assert_eq!(acked, txid, "callback carries the transaction id");

    // Completed slot retained on B (no handler registered yet): observable
    // behaviorally — the payload is delivered exactly once once a handler
    // registers (below); the old internal-state probe is gone.

    // Register the handler: the retained payload is delivered exactly once.
    let (delivered_tx, mut delivered_rx) = tokio::sync::mpsc::unbounded_channel();
    session_b
        .on_binary(Arc::new(move |txid, payload| {
            let _ = delivered_tx.send((txid, payload));
        }))
        .await;

    let got = tokio::time::timeout(Duration::from_secs(10), delivered_rx.recv())
        .await
        .expect("deferred delivery within timeout")
        .expect("delivery channel open");
    assert_eq!(got.0, txid, "deferred delivery keeps txid identity");
    assert!(got.1 == payload, "deferred payload bytes equal");

    // Follow-up transaction: normal immediate path, no interference.
    let second = pattern(2048);
    let txid2 = session_a.send_binary(second.clone(), None).await.unwrap();
    let mut second_seen = false;
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline && !second_seen {
        while let Ok((id, bytes)) = delivered_rx.try_recv() {
            assert_eq!(id, txid2, "exactly-once: first txid never redelivered");
            assert!(bytes == second, "second payload bytes equal");
            second_seen = true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(second_seen, "second transaction delivered immediately");

    // Many clock ticks later: no duplicate delivery for either transaction.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        delivered_rx.try_recv().is_err(),
        "no duplicate delivery for either transaction"
    );
}

// ---- send_binary completion: terminal states on failure paths ----

type InjectWriter = tokio::io::WriteHalf<tokio::io::DuplexStream>;

/// Single-ended rig: a transfer session wired to a transport whose peer
/// is only a raw write end — frames are injected into the session's inbound
/// stream, and nothing ever answers back (no ACKs, no RESUME traffic).
async fn single_ended_session(cfg: TransferConfig) -> (Arc<XacppSession>, InjectWriter) {
    let (a, b) = tokio::io::duplex(DUPLEX_BUF);
    let (_a_read, a_write) = tokio::io::split(a);
    let (b_read, b_write) = tokio::io::split(b);
    let transport: Arc<StdioTransport> = Arc::new(StdioTransport::new(
        Box::pin(b_write) as BoxedWriter,
        Box::pin(BufReader::new(b_read)) as BoxedReader,
    ));
    let session = Arc::new(XacppSession::new(Some(cfg)));
    transport.connect().await.unwrap();
    session
        .wire(Arc::clone(&transport) as Arc<dyn XacppTransport>)
        .await
        .unwrap();
    (Arc::clone(&session), a_write)
}

async fn inject_frame(w: &mut (impl AsyncWrite + Unpin), frame: Frame) {
    let wire = frame.encode().unwrap();
    w.write_all(&wire).await.unwrap();
    w.flush().await.unwrap();
}

/// Completion timers off: no heartbeat, no silent kill, no batched ACK
/// export, no NACK feedback — the session only reacts to injected frames
/// (and, for the expiry test, its own retention sweep).
fn quiet_config() -> TransferConfig {
    let mut cfg = test_config();
    cfg.heartbeat_interval_ms = u64::MAX;
    cfg.silent_timeout_ms = 60_000;
    cfg.ack_batch_interval_ms = u64::MAX;
    cfg.nack_feedback_interval_ms = u64::MAX;
    cfg
}

/// RESUME rejected (wire-compatibility value, adjudicated like unknown):
/// the holder's transaction is fully sent (tail frame plus the completion
/// announcement went out), so the holder-side adjudication (SPEC §5)
/// closes it **confirmed** — the completion fires exactly once as
/// confirmed and the entry leaves the completions map (a replay never
/// fires again).
#[tokio::test]
async fn send_completion_fires_confirmed_on_resume_rejected() {
    let (session, mut peer_write) = single_ended_session(quiet_config()).await;

    let (results_tx, mut results_rx) = tokio::sync::mpsc::unbounded_channel();
    let txid = session
        .send_binary(
            pattern(1024),
            Some(Box::new(move |result| {
                let _ = results_tx.send(result);
            })),
        )
        .await
        .unwrap();

    let resp = ResumeResponsePayload {
        entries: vec![ResumeStatusEntry {
            transaction_id: txid,
            status: ResumeStatus::Rejected,
        }],
    };
    let frame = Frame::new(
        FrameType::ResumeResponse,
        [0u8; 16],
        0,
        0,
        resp.encode().unwrap(),
    );
    inject_frame(&mut peer_write, frame).await;

    let result = tokio::time::timeout(Duration::from_secs(5), results_rx.recv())
        .await
        .expect("completion within timeout")
        .expect("callback fired");
    assert_eq!(result.transaction_id, txid);
    assert!(
        result.confirmed,
        "a fully-sent transaction closes confirmed by holder-side adjudication"
    );

    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        results_rx.try_recv().is_err(),
        "completion must fire exactly once"
    );
}

/// No-expiry semantics (retention sweep retired): nobody ever answers, but
/// the transaction stays alive indefinitely — the completion must NOT fire
/// (pre-revision the sweep released it unconfirmed after the retention
/// window).
#[tokio::test]
async fn send_completion_never_fires_on_long_idle() {
    let (session, _peer_write) = single_ended_session(quiet_config()).await;

    let (results_tx, mut results_rx) = tokio::sync::mpsc::unbounded_channel();
    let _txid = session
        .send_binary(
            pattern(1024),
            Some(Box::new(move |result| {
                let _ = results_tx.send(result);
            })),
        )
        .await
        .unwrap();

    // Sit well beyond the historical 120 s retention default (compressed
    // here to a plain quiet window — there is no sweep left to fire it).
    tokio::time::sleep(Duration::from_millis(1_000)).await;
    assert!(
        results_rx.try_recv().is_err(),
        "no expiry may release a live transaction's completion"
    );
}

/// Repeated final ACK: the completion fires exactly once (confirmed), and
/// replaying the same ACK never fires again — the map entry is consumed by
/// the first fire.
#[tokio::test]
async fn send_completion_fires_exactly_once_on_repeated_ack() {
    let (session, mut peer_write) = single_ended_session(quiet_config()).await;

    let (results_tx, mut results_rx) = tokio::sync::mpsc::unbounded_channel();
    let txid = session
        .send_binary(
            pattern(1024),
            Some(Box::new(move |result| {
                let _ = results_tx.send(result);
            })),
        )
        .await
        .unwrap();

    // Final ACK for the single frame (1024-byte payload at a 1024-byte
    // frame size = one frame, seq 0).
    let ack = AckPayload {
        entries: vec![AckEntry {
            transaction_id: txid,
            highest_contiguous_seq: 0,
            received_frames: 1,
        }],
    };
    let frame = Frame::new(FrameType::Ack, [0u8; 16], 0, 0, ack.encode().unwrap());
    inject_frame(&mut peer_write, frame.clone()).await;

    let result = tokio::time::timeout(Duration::from_secs(5), results_rx.recv())
        .await
        .expect("completion within timeout")
        .expect("callback fired");
    assert_eq!(result.transaction_id, txid);
    assert!(result.confirmed, "final ACK must confirm the delivery");

    // Replay the identical final ACK: the entry is gone, no second fire.
    inject_frame(&mut peer_write, frame).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        results_rx.try_recv().is_err(),
        "repeated ACK must not fire the completion again"
    );
}

// ---- Expert-review fixes: envelope liveness (S1) / zombie clock (S2) / double wire (S5) ----

const FRAME_TYPE_HEARTBEAT: u16 = 0x0005;

/// Single-ended rig with both peer halves accessible: `peer_write` injects
/// frames into the session's inbound stream, `peer_read` collects the
/// session's outbound frames (heartbeats etc.).
async fn single_ended_pair(
    cfg: TransferConfig,
) -> (
    Arc<StdioTransport>,
    Arc<XacppSession>,
    tokio::io::WriteHalf<tokio::io::DuplexStream>,
    tokio::io::ReadHalf<tokio::io::DuplexStream>,
) {
    let (a, b) = tokio::io::duplex(DUPLEX_BUF);
    let (a_read, a_write) = tokio::io::split(a);
    let (b_read, b_write) = tokio::io::split(b);
    let transport: Arc<StdioTransport> = Arc::new(StdioTransport::new(
        Box::pin(b_write) as BoxedWriter,
        Box::pin(BufReader::new(b_read)) as BoxedReader,
    ));
    let session = Arc::new(XacppSession::new(Some(cfg)));
    transport.connect().await.unwrap();
    session
        .wire(Arc::clone(&transport) as Arc<dyn XacppTransport>)
        .await
        .unwrap();
    (transport, session, a_write, a_read)
}

/// Count frames of a wire type on the raw peer read half for `window`
/// (frame-boundary scan: 36-byte header, type at bytes 6..8, length at
/// bytes 32..36, 4-byte trailing CRC).
async fn count_frames(
    mut read: tokio::io::ReadHalf<tokio::io::DuplexStream>,
    frame_type: u16,
    window: Duration,
) -> u32 {
    let deadline = tokio::time::Instant::now() + window;
    let mut buf = [0u8; 4096];
    let mut scan: Vec<u8> = Vec::new();
    let mut count = 0u32;
    while let Some(remaining) = deadline.checked_duration_since(tokio::time::Instant::now()) {
        match tokio::time::timeout(remaining, read.read(&mut buf)).await {
            Ok(Ok(0)) | Err(_) => break,
            Ok(Ok(n)) => {
                scan.extend_from_slice(&buf[..n]);
                loop {
                    let Some(pos) = scan.windows(4).position(|w| w == MAGIC) else {
                        scan.clear();
                        break;
                    };
                    scan.drain(..pos);
                    if scan.len() < HEADER_LEN {
                        break;
                    }
                    let len = u32::from_be_bytes([scan[32], scan[33], scan[34], scan[35]]) as usize;
                    let total = HEADER_LEN + len + CRC_LEN;
                    if scan.len() < total {
                        break;
                    }
                    let ftype = u16::from_be_bytes([scan[6], scan[7]]);
                    if ftype == frame_type {
                        count += 1;
                    }
                    scan.drain(..total);
                }
            }
            Ok(Err(_)) => break,
        }
    }
    count
}

/// SPEC §5: any inbound frame refreshes the peer-activity clock — including
/// envelope (`DATA_JSON`) frames, which never reach the transfer dispatch.
/// A connection kept alive purely by envelope traffic (no control frames,
/// no heartbeat round-trips) must not be false-killed by the silent-peer
/// detector.
#[tokio::test]
async fn envelope_only_traffic_refreshes_activity_clock() {
    let mut cfg = test_config();
    cfg.silent_timeout_ms = 400;
    cfg.heartbeat_interval_ms = u64::MAX;
    let (transport, _session, mut peer_write, _peer_read) = single_ended_pair(cfg).await;
    let closed = transport.closed();

    // Peer keeps only the envelope channel busy: a `DATA_JSON` frame every
    // 100 ms (reader-side responses for unknown ids are dropped) and never
    // a control frame.
    let injector = tokio::spawn(async move {
        for i in 0..12 {
            let envelope = XacppEnvelope::Response {
                id: format!("e{i}"),
                session_id: None,
                payload: XacppResponse::Established {
                    session_id: "s".into(),
                    credentials: "c".into(),
                },
            };
            let json = serde_json::to_vec(&envelope).unwrap();
            let frame = Frame::new(FrameType::DataJson, [0u8; 16], json.len() as u32, 0, json);
            inject_frame(&mut peer_write, frame).await;
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    });

    // Well past three silent timeouts: the connection must still be up.
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert!(
        !*closed.borrow(),
        "envelope-only liveness must not be false-killed by the silent-peer detector"
    );
    injector.await.unwrap();
}

/// Re-wiring (unwire, then wire a fresh session onto the same transport)
/// keeps exactly one clock: heartbeats continue at the single-clock rate
/// and never double (a zombie clock left from the previous wiring would).
#[tokio::test]
async fn rewire_after_unwire_keeps_single_clock() {
    let mut cfg = test_config();
    cfg.heartbeat_interval_ms = 40;
    cfg.silent_timeout_ms = 60_000;
    let (transport, session, _peer_write, peer_read) = single_ended_pair(cfg).await;

    // Collect heartbeats over a window spanning the rewire.
    let counter = tokio::spawn(async move {
        count_frames(peer_read, FRAME_TYPE_HEARTBEAT, Duration::from_millis(600)).await
    });

    // Mid-window: swap the wiring to a fresh session (same transport).
    tokio::time::sleep(Duration::from_millis(250)).await;
    session.unwire().await;
    let session2 = Arc::new(XacppSession::new(Some(cfg)));
    session2
        .wire(Arc::clone(&transport) as Arc<dyn XacppTransport>)
        .await
        .unwrap();

    let count = counter.await.unwrap();
    // Single clock at 40 ms over ~600 ms: ~12-15 beats; a doubled (zombie)
    // clock would land at 20+. Guard both directions: alive (>= 4) and
    // single-sourced (< 19).
    assert!(
        (4..19).contains(&count),
        "expected single-clock heartbeat rate, got {count} beats in the window"
    );
}

/// Session-level wiring semantics (the narrow-interface form): re-wiring a
/// session onto a new transport replaces its stale wiring idempotently
/// (reconnect), and after `unwire` the transport is free for another
/// session to attach. (The old transport-level mount guard is gone with the
/// fat trait: the transport only stores hooks — see `TransferWiring`.)
#[tokio::test]
async fn rewire_replaces_stale_wiring_and_releases_transport() {
    let (ta, tb) = duplex_pair();
    let ta: Arc<StdioTransport> = Arc::new(ta);
    let tb: Arc<StdioTransport> = Arc::new(tb);

    let sa = Arc::new(XacppSession::new(None));
    ta.connect().await.unwrap();
    tb.connect().await.unwrap();

    // sa attaches to ta...
    sa.wire(Arc::clone(&ta) as Arc<dyn XacppTransport>)
        .await
        .unwrap();
    // ...re-attaches to tb, replacing its stale wiring (reconnect shape)...
    sa.wire(Arc::clone(&tb) as Arc<dyn XacppTransport>)
        .await
        .unwrap();
    // ...and detaches, after which sb can attach to ta cleanly.
    sa.unwire().await;
    let sb = Arc::new(XacppSession::new(None));
    sb.wire(Arc::clone(&ta) as Arc<dyn XacppTransport>)
        .await
        .unwrap();
    sb.unwire().await;
}

/// Reconnect-after-silence: the silent-timeout anchor travels with the
/// session across connections (transaction state does), but the observation
/// window belongs to the current connection. A session re-wired after a
/// silent-timeout disconnect must not inherit the previous connection's
/// accumulated silence and die on the new connection's first tick
/// (reconnect death loop): wire reopens the window, so the new connection
/// gets a full silent timeout of grace — and the detector stays armed on
/// the fresh window (no peer traffic past the deadline still kills it).
#[tokio::test]
async fn rewiring_after_silent_timeout_resets_liveness_window() {
    let mut cfg = test_config();
    cfg.silent_timeout_ms = 400;
    cfg.heartbeat_interval_ms = u64::MAX;

    // Connection 1: left fully silent → the session self-tears it down,
    // leaving the anchor ~a full window in the past.
    let (transport1, session, _w1, _r1) = single_ended_pair(cfg).await;
    let closed1 = transport1.closed();
    assert!(
        wait_until(Duration::from_millis(2_000), || *closed1.borrow()).await,
        "silent peer must tear the connection down through the clock"
    );

    // Connection 2: same session (transaction state travels), fresh
    // transport. The wire must reset the liveness anchor.
    let (a2, b2) = tokio::io::duplex(DUPLEX_BUF);
    let (a2_read, a2_write) = tokio::io::split(a2);
    let (b2_read, b2_write) = tokio::io::split(b2);
    let transport2: Arc<StdioTransport> = Arc::new(StdioTransport::new(
        Box::pin(b2_write) as BoxedWriter,
        Box::pin(BufReader::new(b2_read)) as BoxedReader,
    ));
    transport2.connect().await.unwrap();
    session
        .wire(Arc::clone(&transport2) as Arc<dyn XacppTransport>)
        .await
        .unwrap();
    let closed2 = transport2.closed();

    // Partway into the fresh window: still alive. The inherited-silence bug
    // killed the connection within the first tick (~20 ms) — a quarter of
    // the window has a wide margin either way.
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert!(
        !*closed2.borrow(),
        "re-wired session must not inherit the previous connection's silence"
    );

    // The detector stays armed on the fresh window: a full silent timeout
    // without peer traffic kills the new connection too (the reset must not
    // have disabled the deadline, only moved its origin).
    assert!(
        wait_until(Duration::from_millis(2_000), || *closed2.borrow()).await,
        "fresh window must still enforce the silent-timeout deadline"
    );
    drop((a2_write, a2_read));
}

// ---- Recovery discipline (holder declares → receiver adjudicates →
// resume instruction lifts the holder's gate) ----

type InjectReader = tokio::io::ReadHalf<tokio::io::DuplexStream>;

/// Single-ended rig with BOTH peer ends exposed: frames are injected via
/// the writer, and everything the session emits is readable on the reader.
async fn readable_peer_rig(cfg: TransferConfig) -> (Arc<XacppSession>, InjectReader, InjectWriter) {
    let (a, b) = tokio::io::duplex(DUPLEX_BUF);
    let (a_read, a_write) = tokio::io::split(a);
    let (b_read, b_write) = tokio::io::split(b);
    let transport: Arc<StdioTransport> = Arc::new(StdioTransport::new(
        Box::pin(b_write) as BoxedWriter,
        Box::pin(BufReader::new(b_read)) as BoxedReader,
    ));
    let session = Arc::new(XacppSession::new(Some(cfg)));
    transport.connect().await.unwrap();
    session
        .wire(Arc::clone(&transport) as Arc<dyn XacppTransport>)
        .await
        .unwrap();
    (session, a_read, a_write)
}

/// Collect every frame the session emits within `window`.
async fn collect_frames(r: &mut InjectReader, window: Duration) -> Vec<Frame> {
    let mut dec = xacpp::wire::frame::FrameDecoder::new();
    let mut out = Vec::new();
    let deadline = Instant::now() + window;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        let mut buf = [0u8; 8192];
        match tokio::time::timeout(remaining, r.read(&mut buf)).await {
            Ok(Ok(0)) => break,
            Ok(Ok(n)) => out.extend(dec.feed(&buf[..n]).expect("peer wire must decode")),
            Ok(Err(e)) => panic!("peer read failed: {e}"),
            Err(_) => break,
        }
    }
    out
}

/// The holder declares its pending sends on wire-up — and ONLY when it has
/// any: an empty pending set emits no RESUME frame at all, and a gated
/// transaction emits no retransmission before the verdict arrives.
#[tokio::test]
async fn resume_declaration_sent_only_when_pending() {
    let (session, mut peer_read, _peer_write) = readable_peer_rig(quiet_config()).await;

    // Wire-up with an empty pending set: no RESUME frame may appear.
    let frames = collect_frames(&mut peer_read, Duration::from_millis(300)).await;
    assert!(
        frames.iter().all(|f| f.frame_type != FrameType::Resume),
        "empty pending set must not emit a RESUME declaration"
    );

    // A pending send exists now (the peer never answers on conn 1).
    let txid = session.send_binary(pattern(1024), None).await.unwrap();
    // Drain conn-1 traffic (the data frame and any RTO retransmissions).
    let _ = collect_frames(&mut peer_read, Duration::from_millis(300)).await;

    // Re-wire to a fresh transport: the declaration must carry the txid.
    let (a2, b2) = tokio::io::duplex(DUPLEX_BUF);
    let (a2_read, _a2_write) = tokio::io::split(a2);
    let (b2_read, b2_write) = tokio::io::split(b2);
    let t2: Arc<StdioTransport> = Arc::new(StdioTransport::new(
        Box::pin(b2_write) as BoxedWriter,
        Box::pin(BufReader::new(b2_read)) as BoxedReader,
    ));
    t2.connect().await.unwrap();
    session
        .wire(Arc::clone(&t2) as Arc<dyn XacppTransport>)
        .await
        .unwrap();

    let mut peer_read2 = a2_read;
    let frames = collect_frames(&mut peer_read2, Duration::from_millis(500)).await;
    let resume = frames
        .iter()
        .find(|f| f.frame_type == FrameType::Resume)
        .expect("wire-up with a pending send must emit the RESUME declaration");
    let req = ResumeRequestPayload::decode(&resume.payload).unwrap();
    assert!(
        req.entries.iter().any(|e| e.transaction_id == txid),
        "the declaration must carry the pending transaction id"
    );
    assert!(
        frames.iter().all(|f| f.frame_type != FrameType::DataBinary),
        "a gated transaction must not retransmit before the verdict"
    );
}

/// A pending send the receiver does NOT recognize closes by the
/// holder-side adjudication (SPEC §5): the fresh receiver answers
/// `Unknown` (zero progress — no frame ever arrived) and the holder, whose
/// transaction is fully sent (tail frame plus the completion announcement
/// went out), closes it **confirmed**. The zero-progress resume deadlock
/// (Resumed → complete slot swallows the retransmission) is retired: the
/// completion fires exactly once, in bounded time.
#[tokio::test]
async fn resume_unknown_verdict_confirms_a_fully_sent_transfer() {
    let mut cfg_a = test_config();
    cfg_a.sender.rto_initial_ms = u64::MAX; // conn 1: never retransmit
    cfg_a.sender.rto_max_ms = u64::MAX;
    let mut cfg_b = test_config();
    cfg_b.nack_feedback_interval_ms = u64::MAX;

    let session_a = Arc::new(XacppSession::new(Some(cfg_a)));
    let session_b = Arc::new(XacppSession::new(Some(cfg_b)));

    // Connection 1: the single data frame never reaches the receiver.
    let (a, b) = tokio::io::duplex(DUPLEX_BUF);
    let (a_read, a_write) = tokio::io::split(a);
    let (b_read, b_write) = tokio::io::split(b);
    let ta1: Arc<StdioTransport> = Arc::new(StdioTransport::new(
        Box::pin(FrameDropper::new(a_write, Some(0))) as BoxedWriter,
        Box::pin(BufReader::new(a_read)) as BoxedReader,
    ));
    let tb1: Arc<StdioTransport> = Arc::new(StdioTransport::new(
        Box::pin(b_write) as BoxedWriter,
        Box::pin(BufReader::new(b_read)) as BoxedReader,
    ));
    ta1.connect().await.unwrap();
    tb1.connect().await.unwrap();
    session_a
        .wire(Arc::clone(&ta1) as Arc<dyn XacppTransport>)
        .await
        .unwrap();
    session_b
        .wire(Arc::clone(&tb1) as Arc<dyn XacppTransport>)
        .await
        .unwrap();

    let (results_tx, mut results_rx) = tokio::sync::mpsc::unbounded_channel();
    let txid = session_a
        .send_binary(
            pattern(1024),
            Some(Box::new(move |result| {
                let _ = results_tx.send(result);
            })),
        )
        .await
        .unwrap();

    // Let the (dropped) first transmission settle, then tear down conn 1.
    tokio::time::sleep(Duration::from_millis(500)).await;
    ta1.disconnect().await.unwrap();
    drop((ta1, tb1, session_b));

    // Connection 2: the holder is the same session; the receiver is a
    // FRESH session that has never seen the transaction.
    let session_b2 = Arc::new(XacppSession::new(Some(test_config())));
    let (ta2, tb2) = duplex_pair();
    let ta2: Arc<StdioTransport> = Arc::new(ta2);
    let tb2: Arc<StdioTransport> = Arc::new(tb2);
    ta2.connect().await.unwrap();
    tb2.connect().await.unwrap();
    session_b2
        .wire(Arc::clone(&tb2) as Arc<dyn XacppTransport>)
        .await
        .unwrap();
    session_a
        .wire(Arc::clone(&ta2) as Arc<dyn XacppTransport>)
        .await
        .unwrap();

    let result = tokio::time::timeout(Duration::from_secs(5), results_rx.recv())
        .await
        .expect("completion within timeout")
        .expect("callback fired");
    assert_eq!(result.transaction_id, txid);
    assert!(
        result.confirmed,
        "a fully-sent transaction closes confirmed by holder-side adjudication"
    );
}

/// A repeated RESUME declaration is idempotent at the receiver: every
/// declaration is answered with exactly one verdict frame (never silently
/// swallowed), and a declared open transaction is never disturbed.
#[tokio::test]
async fn repeated_resume_declaration_is_idempotent() {
    let (session, mut peer_read, mut peer_write) = readable_peer_rig(quiet_config()).await;

    let (delivered_tx, mut delivered_rx) = tokio::sync::mpsc::unbounded_channel();
    session
        .on_binary(Arc::new(move |txid, payload| {
            let _ = delivered_tx.send((txid, payload));
        }))
        .await;

    // Open a partial inbound transaction (2 of 3 frames).
    let txid = [5u8; 16];
    for (seq, byte) in [(0u32, 0u8), (1, 1)] {
        let frame = Frame::new(FrameType::DataBinary, txid, 12, seq, vec![byte; 4]);
        inject_frame(&mut peer_write, frame).await;
    }
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Two identical declarations, both covering the open transaction.
    let declare = || {
        Frame::new(
            FrameType::Resume,
            [0u8; 16],
            0,
            0,
            ResumeRequestPayload {
                entries: vec![AckEntry {
                    transaction_id: txid,
                    highest_contiguous_seq: 1,
                    received_frames: 2,
                }],
            }
            .encode()
            .unwrap(),
        )
    };
    inject_frame(&mut peer_write, declare()).await;
    inject_frame(&mut peer_write, declare()).await;

    let frames = collect_frames(&mut peer_read, Duration::from_millis(500)).await;
    let responses: Vec<&Frame> = frames
        .iter()
        .filter(|f| f.frame_type == FrameType::ResumeResponse)
        .collect();
    assert_eq!(
        responses.len(),
        2,
        "every declaration is answered exactly once (never silently swallowed)"
    );
    for resp in responses {
        let payload = ResumeResponsePayload::decode(&resp.payload).unwrap();
        assert_eq!(
            payload.entries,
            vec![ResumeStatusEntry {
                transaction_id: txid,
                status: ResumeStatus::Resumed,
            }],
            "a declared open transaction adjudicates as Resumed"
        );
    }

    // The transaction is undisturbed: the final frame completes it.
    let frame = Frame::new(FrameType::DataBinary, txid, 12, 2, vec![2u8; 4]);
    inject_frame(&mut peer_write, frame).await;
    let got = tokio::time::timeout(Duration::from_secs(5), delivered_rx.recv())
        .await
        .expect("completion within timeout")
        .expect("delivered");
    assert_eq!(got.0, txid);
    let mut expected = vec![0u8; 4];
    expected.extend_from_slice(&[1u8; 4]);
    expected.extend_from_slice(&[2u8; 4]);
    assert_eq!(got.1, expected);
}

/// Receiver adjudication, disambiguated (SPEC §5 v1.1.1): an open
/// INCOMPLETE slot answers `Resumed` (the continuation instruction); a
/// completed-but-undelivered slot answers `Unknown` — it awaits local
/// consumption and needs nothing more from the holder, and with no
/// terminal trace kept a consumed-and-delivered transaction and a
/// never-seen one are indistinguishable (the holder closes those by its
/// own adjudication). One declaration covering both shapes is answered
/// with exactly one verdict per entry.
#[tokio::test]
async fn resume_adjudicates_incomplete_as_resumed_and_completed_undelivered_as_unknown() {
    let (_session, mut peer_read, mut peer_write) = readable_peer_rig(quiet_config()).await;

    // NO on_binary handler: the completed slot stays retained
    // (completed-but-undelivered) instead of being consumed.
    // - txid_open: 1 of 2 frames — open incomplete.
    // - txid_done: both frames — completed but undelivered.
    let txid_open = [6u8; 16];
    let txid_done = [7u8; 16];
    inject_frame(
        &mut peer_write,
        Frame::new(FrameType::DataBinary, txid_open, 8, 0, vec![1u8; 4]),
    )
    .await;
    for seq in 0..2u32 {
        inject_frame(
            &mut peer_write,
            Frame::new(FrameType::DataBinary, txid_done, 8, seq, vec![2u8; 4]),
        )
        .await;
    }
    tokio::time::sleep(Duration::from_millis(200)).await;

    // One declaration covering both shapes.
    let declare = Frame::new(
        FrameType::Resume,
        [0u8; 16],
        0,
        0,
        ResumeRequestPayload {
            entries: vec![
                AckEntry {
                    transaction_id: txid_open,
                    highest_contiguous_seq: 1,
                    received_frames: 2,
                },
                AckEntry {
                    transaction_id: txid_done,
                    highest_contiguous_seq: 1,
                    received_frames: 2,
                },
            ],
        }
        .encode()
        .unwrap(),
    );
    inject_frame(&mut peer_write, declare).await;

    let frames = collect_frames(&mut peer_read, Duration::from_millis(300)).await;
    let resp = frames
        .iter()
        .find(|f| f.frame_type == FrameType::ResumeResponse)
        .expect("every declaration is answered with one verdict frame");
    let payload = ResumeResponsePayload::decode(&resp.payload).unwrap();
    assert_eq!(payload.entries.len(), 2, "one verdict per declared entry");
    for entry in &payload.entries {
        let expected = if entry.transaction_id == txid_open {
            ResumeStatus::Resumed
        } else if entry.transaction_id == txid_done {
            ResumeStatus::Unknown
        } else {
            panic!("unexpected verdict entry: {entry:?}");
        };
        assert_eq!(entry.status, expected);
    }
}

/// Both directions reconcile independently across the same reconnect:
/// each side declares its own pending send, adjudicates the peer's
/// declaration, and both interrupted transfers complete.
#[tokio::test]
async fn bidirectional_resume_reconciles_both_directions() {
    let quiet_cfg = || {
        let mut c = test_config();
        c.sender.rto_initial_ms = u64::MAX; // conn 1: no retransmission
        c.sender.rto_max_ms = u64::MAX;
        c.nack_feedback_interval_ms = u64::MAX;
        c
    };
    let session_a = Arc::new(XacppSession::new(Some(quiet_cfg())));
    let session_b = Arc::new(XacppSession::new(Some(quiet_cfg())));

    // Connection 1: lossy in both directions — a→b drops seq 3, b→a drops
    // seq 1 (each FrameDropper only scans its own direction's stream).
    let (a, b) = tokio::io::duplex(DUPLEX_BUF);
    let (a_read, a_write) = tokio::io::split(a);
    let (b_read, b_write) = tokio::io::split(b);
    let ta1: Arc<StdioTransport> = Arc::new(StdioTransport::new(
        Box::pin(FrameDropper::new(a_write, Some(3))) as BoxedWriter,
        Box::pin(BufReader::new(a_read)) as BoxedReader,
    ));
    let tb1: Arc<StdioTransport> = Arc::new(StdioTransport::new(
        Box::pin(FrameDropper::new(b_write, Some(1))) as BoxedWriter,
        Box::pin(BufReader::new(b_read)) as BoxedReader,
    ));
    ta1.connect().await.unwrap();
    tb1.connect().await.unwrap();
    session_a
        .wire(Arc::clone(&ta1) as Arc<dyn XacppTransport>)
        .await
        .unwrap();
    session_b
        .wire(Arc::clone(&tb1) as Arc<dyn XacppTransport>)
        .await
        .unwrap();

    let (ab_tx, mut ab_rx) = tokio::sync::mpsc::unbounded_channel();
    session_b
        .on_binary(Arc::new(move |txid, payload| {
            let _ = ab_tx.send((txid, payload));
        }))
        .await;
    let (ba_tx, mut ba_rx) = tokio::sync::mpsc::unbounded_channel();
    session_a
        .on_binary(Arc::new(move |txid, payload| {
            let _ = ba_tx.send((txid, payload));
        }))
        .await;

    let payload_ab = pattern(4096);
    let payload_ba = pattern(2048);
    session_a
        .send_binary(payload_ab.clone(), None)
        .await
        .unwrap();
    session_b
        .send_binary(payload_ba.clone(), None)
        .await
        .unwrap();

    // Let the deterministic partial state settle on both receivers, then
    // tear down conn 1.
    tokio::time::sleep(Duration::from_secs(1)).await;
    ta1.disconnect().await.unwrap();
    tb1.disconnect().await.unwrap();
    drop((ta1, tb1));

    // Connection 2: fresh clean transports, the SAME sessions wired again.
    let (ta2, tb2) = duplex_pair();
    let ta2: Arc<StdioTransport> = Arc::new(ta2);
    let tb2: Arc<StdioTransport> = Arc::new(tb2);
    ta2.connect().await.unwrap();
    tb2.connect().await.unwrap();
    session_a
        .wire(Arc::clone(&ta2) as Arc<dyn XacppTransport>)
        .await
        .unwrap();
    session_b
        .wire(Arc::clone(&tb2) as Arc<dyn XacppTransport>)
        .await
        .unwrap();

    let got_ab = tokio::time::timeout(Duration::from_secs(10), ab_rx.recv())
        .await
        .expect("a→b delivery within timeout")
        .expect("channel open");
    let got_ba = tokio::time::timeout(Duration::from_secs(10), ba_rx.recv())
        .await
        .expect("b→a delivery within timeout")
        .expect("channel open");
    assert!(
        got_ab.1 == payload_ab,
        "a→b bytes equal after reconciliation"
    );
    assert!(
        got_ba.1 == payload_ba,
        "b→a bytes equal after reconciliation"
    );
}

// ---- Recovery over the real establish handshake (SPEC §5) ----
//
// Pin the RESUME declaration timing against the establish topology: the
// declaration must land only after BOTH peers' transfer planes are wired.
// The bug these anchor: the declaration fired at wire-up, which on the
// initiator side precedes the responder's on_establish wire — the RESUME
// frame was dropped by the not-yet-wired receiver and the holder
// deadlocked awaiting a verdict that could never arrive.
//
// RED anchor discipline: both cases must fail by timeout before the fix
// and pass deterministically after it (loopback resume is milliseconds;
// the cap only distinguishes deadlock from success).

mod establish_topo {
    use super::*;
    use xacpp::session::handler::{
        EstablishDecision, EstablishHandler, NegotiateHandler, XacppSessionHandler,
    };
    use xacpp::session::peer::XacppPeer;
    use xacpp::wire::capability::{Capabilities, EffectiveCapabilities};
    use xacpp::wire::events::XacppActivityEvent;
    use xacpp::wire::message::XacppResponse;

    const RESUME_CAP: Duration = Duration::from_secs(5);

    fn caps() -> Capabilities {
        Capabilities {
            commands: vec![],
            produce_events: vec![],
            accept_events: vec![],
            fetch_ext: None,
        }
    }

    struct AcceptAllNegotiate;

    #[async_trait::async_trait]
    impl NegotiateHandler for AcceptAllNegotiate {
        async fn on_negotiate(&self, _effective: EffectiveCapabilities) -> Result<(), XacppError> {
            Ok(())
        }
    }

    struct AckSessionHandler;

    #[async_trait::async_trait]
    impl XacppSessionHandler for AckSessionHandler {
        async fn on_command(&self, _command: XacppCommand) -> Result<XacppResponse, XacppError> {
            Ok(XacppResponse::acknowledge())
        }

        async fn on_event(&self, _event: XacppActivityEvent) -> Result<XacppResponse, XacppError> {
            Ok(XacppResponse::acknowledge())
        }
    }

    /// Never used on the initiator side (its establish handler is only
    /// consulted by the responder role), but XacppPeer::new requires one.
    struct NeverEstablish;

    #[async_trait::async_trait]
    impl EstablishHandler for NeverEstablish {
        async fn on_establish(
            &self,
            _transport: Arc<dyn XacppTransport>,
            _credentials: Option<String>,
        ) -> Result<EstablishDecision, XacppError> {
            Err(XacppError::Internal("never an initiator".into()))
        }

        async fn on_establish_confirm(
            &self,
            _transport: Arc<dyn XacppTransport>,
        ) -> Result<(String, Arc<dyn XacppSessionHandler>, String), XacppError> {
            Err(XacppError::Internal("never an initiator".into()))
        }
    }

    /// Responder-side fixture following the documented consumer pattern:
    /// the session is built on first establish and REUSED by identity on
    /// reconnect, wired to the live transport inside on_establish.
    /// Inbound binaries are collected through the stream surface (a
    /// transaction claimed by a stream never reaches on_binary); the first
    /// chunk read flips the claim watch so tests can wait for an OPEN
    /// inbound slot before inducing the outage.
    struct ResponderFixture {
        session: tokio::sync::Mutex<Option<Arc<XacppSession>>>,
        received: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
        claimed: tokio::sync::watch::Sender<bool>,
    }

    impl ResponderFixture {
        fn new() -> (Arc<Self>, tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>) {
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            let (claimed, _rx) = tokio::sync::watch::channel(false);
            (
                Arc::new(Self {
                    session: tokio::sync::Mutex::new(None),
                    received: tx,
                    claimed,
                }),
                rx,
            )
        }

        /// Resolves once the inbound stream has been claimed (first chunk read).
        async fn wait_stream_claimed(&self) {
            let mut rx = self.claimed.subscribe();
            rx.wait_for(|c| *c).await.expect("claim watch open");
        }

        async fn session(&self) -> Arc<XacppSession> {
            let slot = self.session.lock().await;
            slot.as_ref().expect("session built at establish").clone()
        }
    }

    #[async_trait::async_trait]
    impl EstablishHandler for ResponderFixture {
        async fn on_establish(
            &self,
            transport: Arc<dyn XacppTransport>,
            _credentials: Option<String>,
        ) -> Result<EstablishDecision, XacppError> {
            let session = {
                let mut slot = self.session.lock().await;
                match &*slot {
                    Some(s) => Arc::clone(s),
                    None => {
                        let received = self.received.clone();
                        let claimed = self.claimed.clone();
                        let s = Arc::new(
                            XacppSession::new(None)
                                .with_identity(
                                    "responder-sid".to_string(),
                                    "responder-creds".to_string(),
                                )
                                .await,
                        );
                        // Stream surface only: a transaction claimed by a
                        // stream never reaches on_binary — accumulate per
                        // stream and emit the full payload at stream end
                        // for the byte-exactness check. The first chunk
                        // read flips the claim watch (OPEN slot signal).
                        s.on_binary_stream(Arc::new(move |stream| {
                            let received = received.clone();
                            let claimed = claimed.clone();
                            tokio::spawn(async move {
                                let mut acc: Vec<u8> = Vec::new();
                                while let Some(chunk) = stream.read().await {
                                    acc.extend_from_slice(&chunk.bytes);
                                    claimed.send_replace(true);
                                }
                                let _ = received.send(acc);
                            });
                        }))
                        .await;
                        *slot = Some(Arc::clone(&s));
                        s
                    }
                }
            };
            // The peer's transfer plane on the initiator side was wired at
            // connect — wiring (and declaring) here is safe.
            session.wire(transport).await?;
            Ok(EstablishDecision::Established {
                session_id: session.session_id().await,
                handler: Arc::new(AckSessionHandler),
                credentials: "responder-creds".to_string(),
            })
        }

        async fn on_establish_confirm(
            &self,
            _transport: Arc<dyn XacppTransport>,
        ) -> Result<(String, Arc<dyn XacppSessionHandler>, String), XacppError> {
            Err(XacppError::Internal("no challenge in this fixture".into()))
        }
    }

    /// One establish round over a fresh duplex; returns the peers and the
    /// initiator-side session.
    async fn establish_round(
        session_a: Arc<XacppSession>,
        responder: Arc<ResponderFixture>,
    ) -> (XacppPeer, XacppPeer) {
        let (ta, tb) = duplex_pair();
        let peer_a = XacppPeer::new(
            caps(),
            Arc::new(ta),
            Some(session_a),
            Arc::new(AcceptAllNegotiate),
            Arc::new(NeverEstablish),
        );
        let peer_b = XacppPeer::new(
            caps(),
            Arc::new(tb),
            None,
            Arc::new(AcceptAllNegotiate),
            responder,
        );
        peer_a.connect().await.unwrap();
        peer_b.connect().await.unwrap();
        peer_a.negotiate().await.unwrap();
        peer_a
            .establish(None, Arc::new(AckSessionHandler), |_| Ok(()))
            .await
            .unwrap();
        (peer_a, peer_b)
    }

    /// Inflight transfer resumes across a reconnect over the REAL establish
    /// handshake — holder = initiator side.
    #[tokio::test]
    async fn establish_topo_inflight_resume_holder_initiator() {
        let session_a = Arc::new(XacppSession::new(None));
        let (responder, mut received_rx) = ResponderFixture::new();
        let (peer_a1, peer_b1) = establish_round(Arc::clone(&session_a), responder.clone()).await;

        // 16 MiB inflight. Wait until the receiver holds an OPEN inbound
        // slot (first chunk read), then tear down mid-flight — a
        // zero-progress receive is indistinguishable from "never seen"
        // after a reconnect and is correctly adjudicated Unknown
        // (no-tombstone cost).
        let payload = pattern(16 * 1024 * 1024);
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        session_a
            .send_binary(
                payload.clone(),
                Some(Box::new(move |result| {
                    let _ = done_tx.send(result);
                })),
            )
            .await
            .unwrap();
        tokio::time::timeout(RESUME_CAP, responder.wait_stream_claimed())
            .await
            .expect("receiver must claim the inbound stream before the outage");
        peer_a1.disconnect().await.unwrap();
        peer_b1.disconnect().await.unwrap();

        // Round 2: same initiator session (pending copies retained); the
        // responder reuses its session by identity.
        let (peer_a2, peer_b2) = establish_round(Arc::clone(&session_a), responder).await;

        let result = tokio::time::timeout(RESUME_CAP, done_rx)
            .await
            .expect("RESUME resume chain deadlocked (declaration lost before the peer was wired)")
            .expect("completion channel must not close");
        assert!(
            result.confirmed,
            "resumed transfer must complete confirmed, got: {result:?}"
        );
        let got = tokio::time::timeout(RESUME_CAP, received_rx.recv())
            .await
            .expect("receiver must complete the reassembled payload")
            .expect("receiver channel open");
        assert!(got == payload, "receiver bytes bit-exact after resume");

        peer_a2.disconnect().await.unwrap();
        peer_b2.disconnect().await.unwrap();
    }

    /// Same scenario with the roles flipped — holder = responder side (the
    /// session the consumer builds and wires inside on_establish).
    #[tokio::test]
    async fn establish_topo_inflight_resume_holder_responder() {
        let session_a = Arc::new(XacppSession::new(None));
        let (responder, _received_rx) = ResponderFixture::new();
        let (peer_a1, peer_b1) = establish_round(Arc::clone(&session_a), responder.clone()).await;

        // Holder = responder's session; receiver = the initiator side.
        // The receiver must hold an OPEN inbound slot before the outage —
        // a zero-progress receive is indistinguishable from "never seen"
        // after a reconnect and is correctly adjudicated Unknown
        // (no-tombstone cost). Wait for the stream claim (= first frame
        // reassembled, slot open) before tearing down.
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        let (claimed_tx, claimed_rx) = tokio::sync::oneshot::channel::<()>();
        let (got_tx, mut got_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        let claimed_tx = Arc::new(tokio::sync::Mutex::new(Some(claimed_tx)));
        session_a
            .on_binary_stream(Arc::new(move |stream| {
                let claimed_tx = Arc::clone(&claimed_tx);
                let got_tx = got_tx.clone();
                tokio::spawn(async move {
                    let mut acc: Vec<u8> = Vec::new();
                    let mut claimed = false;
                    while let Some(chunk) = stream.read().await {
                        acc.extend_from_slice(&chunk.bytes);
                        if !claimed {
                            claimed = true;
                            if let Some(tx) = claimed_tx.lock().await.take() {
                                let _ = tx.send(());
                            }
                        }
                    }
                    let _ = got_tx.send(acc);
                });
            }))
            .await;

        let payload = pattern(16 * 1024 * 1024);
        let session_r = responder.session().await;
        session_r
            .send_binary(
                payload.clone(),
                Some(Box::new(move |result| {
                    let _ = done_tx.send(result);
                })),
            )
            .await
            .unwrap();
        // Wait until the receiver has an OPEN slot (first chunk read),
        // then tear down mid-flight.
        tokio::time::timeout(RESUME_CAP, claimed_rx)
            .await
            .expect("receiver must claim the inbound stream before the outage")
            .expect("claim channel open");
        peer_b1.disconnect().await.unwrap();
        peer_a1.disconnect().await.unwrap();

        let (peer_a2, peer_b2) = establish_round(Arc::clone(&session_a), responder).await;

        let result = tokio::time::timeout(RESUME_CAP, done_rx)
            .await
            .expect("RESUME resume chain deadlocked (declaration lost before the peer was wired)")
            .expect("completion channel must not close");
        assert!(
            result.confirmed,
            "resumed transfer must complete confirmed, got: {result:?}"
        );
        let got = tokio::time::timeout(RESUME_CAP, got_rx.recv())
            .await
            .expect("initiator side must complete the reassembled payload")
            .expect("receiver channel open");
        assert!(got == payload, "initiator bytes bit-exact after resume");

        peer_a2.disconnect().await.unwrap();
        peer_b2.disconnect().await.unwrap();
    }
}
