//! Fetch extension tests (SPEC §8 matrix + boundary coverage).
//!
//! Infrastructure mirrors `tests/fetch_tests.rs` (BinaryPipe/XacppPeer pair
//! over a stdio duplex); the disconnect/rewire case follows the manual
//! wiring discipline of `tests/stream_lifecycle_tests.rs` (receiver wires
//! first so its RESUME request reaches the duplex before the holder's
//! reader starts).
//!
//! Two sections:
//!
//! - ext1-ext12: the design-anchor matrix (happy paths, begin gate,
//!   idempotency, flow control, disconnect/resume, replay, the
//!   fetch_unavailable gate, truncation).
//! - ext13-ext24: boundary coverage ported from the xacpp-ts blueprint
//!   (`tests/ext-fetch.test.ts`, cases 12-22) — the edges that had zero
//!   coverage on both sides. Race-shaped cases use deterministic
//!   interleavings (a serving parked at a gate), never a real race. One
//!   case is RED on purpose (a product gap, documented in place).

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use sha2::{Digest, Sha256};
use tokio::io::BufReader;
use tokio::sync::Mutex as AsyncMutex;

use xacpp::carrier::XacppTransport;
use xacpp::carrier::stdio::StdioTransport;
use xacpp::ext::fetch::{
    FetchError, FetchProvider, FetchPuller, FetchService, ResolvedResource, abort,
    file as canned_file, pause, resume, verify,
};
use xacpp::reliability::stream::{BinaryMeta, BinaryStream, StreamState};
use xacpp::session::XacppSession;
use xacpp::session::handler::{
    EstablishDecision, EstablishHandler, NegotiateHandler, XacppSessionHandler,
};
use xacpp::session::peer::XacppPeer;
use xacpp::transfer::{TransferActivityHandlers, TransferConfig, TransferWiring};
use xacpp::wire::capability::{Capabilities, EffectiveCapabilities};
use xacpp::wire::commands::XacppCommand;
use xacpp::wire::error::XacppError;
use xacpp::wire::events::XacppActivityEvent;
use xacpp::wire::fetch::{error_codes, format_transaction_id};
use xacpp::wire::frame::Frame;
use xacpp::wire::message::{XacppRequest, XacppResponse};

async fn timeout<F, T>(future: F) -> T
where
    F: std::future::Future<Output = T>,
{
    tokio::time::timeout(Duration::from_secs(10), future)
        .await
        .expect("test timeout")
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn pattern(len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| ((i * 31 + (i >> 7)) & 0xff) as u8)
        .collect()
}

fn assert_32hex(s: &str) {
    assert_eq!(s.len(), 32);
    assert!(s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')));
}

// ---- Test transport: stdio-duplex pipe with the binary channel enabled ----

/// Observable state of the first data frame written on a pipe.
/// The txid lives behind a tokio lock; the presence flag is atomic so the
/// synchronous `wait_until` probe can observe it without awaiting.
struct FirstDataTxid {
    value: AsyncMutex<Option<[u8; 16]>>,
    seen: AtomicBool,
}

impl FirstDataTxid {
    fn new() -> Self {
        Self {
            value: AsyncMutex::new(None),
            seen: AtomicBool::new(false),
        }
    }
}

struct BinaryPipe {
    inner: Arc<StdioTransport>,
    /// Txid of the first data frame written on this pipe (test observation
    /// only — `None` disables the observation).
    first_data_txid: Option<Arc<FirstDataTxid>>,
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
    fn on_request(
        &self,
        handler: xacpp::session::handler::RequestHandler,
    ) -> Result<(), XacppError> {
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
        let txid = frame.transaction_id;
        let result = self.inner.write_data_frame(frame).await;
        // Observe the transaction id of the first data frame — recorded
        // *after* the write, so the frame is already queued when the
        // observer sees it (an injected frame enqueued afterwards is
        // therefore ordered behind it).
        if let Some(slot) = &self.first_data_txid {
            let mut guard = slot.value.lock().await;
            if guard.is_none() {
                *guard = Some(txid);
                slot.seen.store(true, Ordering::SeqCst);
            }
        }
        result
    }
    async fn on_transfer_activity(&self, handlers: Option<TransferActivityHandlers>) {
        self.inner.on_transfer_activity(handlers).await;
    }
}

fn stdio_duplex() -> (Arc<StdioTransport>, Arc<StdioTransport>) {
    let (stream_a, stream_b) = tokio::io::duplex(256 * 1024);
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

// ---- Providers under test ----

/// Canned bytes provider with observable counters, an optional resolve delay
/// (to hold the resolving placeholder open in the race test), and a
/// configurable write chunk (must stay ≤ the session's backpressure cap —
/// the kernel rejects whole chunks that would overflow it).
struct CountingBytes {
    data: Vec<u8>,
    mime: String,
    resolve_count: AtomicUsize,
    serve_count: AtomicUsize,
    resolve_delay: Option<Duration>,
    chunk_size: usize,
}

impl CountingBytes {
    fn new(data: Vec<u8>) -> Arc<Self> {
        Arc::new(Self {
            data,
            mime: "application/octet-stream".into(),
            resolve_count: AtomicUsize::new(0),
            serve_count: AtomicUsize::new(0),
            resolve_delay: None,
            chunk_size: 64 * 1024,
        })
    }

    fn with_delay(data: Vec<u8>, delay: Duration) -> Arc<Self> {
        Arc::new(Self {
            data,
            mime: "application/octet-stream".into(),
            resolve_count: AtomicUsize::new(0),
            serve_count: AtomicUsize::new(0),
            resolve_delay: Some(delay),
            chunk_size: 64 * 1024,
        })
    }

    fn with_chunk(data: Vec<u8>, chunk_size: usize) -> Arc<Self> {
        Arc::new(Self {
            data,
            mime: "application/octet-stream".into(),
            resolve_count: AtomicUsize::new(0),
            serve_count: AtomicUsize::new(0),
            resolve_delay: None,
            chunk_size,
        })
    }
}

#[async_trait]
impl FetchProvider for CountingBytes {
    async fn resolve(&self, _uri: &str) -> Result<ResolvedResource, XacppError> {
        self.resolve_count.fetch_add(1, Ordering::SeqCst);
        if let Some(d) = self.resolve_delay {
            tokio::time::sleep(d).await;
        }
        Ok(ResolvedResource {
            size_bytes: self.data.len() as u64,
            sha256: sha256_hex(&self.data),
            mime: self.mime.clone(),
        })
    }

    async fn serve(
        &self,
        _uri: &str,
        sink: Arc<xacpp::reliability::stream::BinarySink>,
    ) -> Result<(), XacppError> {
        self.serve_count.fetch_add(1, Ordering::SeqCst);
        for chunk in self.data.chunks(self.chunk_size) {
            sink.write(chunk).await?;
        }
        Ok(())
    }
}

/// Item 3: a custom provider writing straight into the real sink.
struct DirectWriter {
    data: Vec<u8>,
}

#[async_trait]
impl FetchProvider for DirectWriter {
    async fn resolve(&self, _uri: &str) -> Result<ResolvedResource, XacppError> {
        Ok(ResolvedResource {
            size_bytes: self.data.len() as u64,
            sha256: sha256_hex(&self.data),
            mime: "text/plain".into(),
        })
    }

    async fn serve(
        &self,
        _uri: &str,
        sink: Arc<xacpp::reliability::stream::BinarySink>,
    ) -> Result<(), XacppError> {
        // One deliberate tiny write + the rest, all through the bare sink API.
        sink.write(&self.data[..16]).await?;
        for chunk in self.data[16..].chunks(1024) {
            sink.write(chunk).await?;
        }
        Ok(())
    }
}

// ---- Handshake plumbing ----

struct AcceptNegotiate;

#[async_trait]
impl NegotiateHandler for AcceptNegotiate {
    async fn on_negotiate(&self, _effective: EffectiveCapabilities) -> Result<(), XacppError> {
        Ok(())
    }
}

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

/// Responder establish: builds the session (custom transfer config), wraps it
/// in the FetchService, wires, answers Established, retains the session in
/// `slot` for the re-wire test.
struct ServeEstablish {
    provider: Arc<dyn FetchProvider>,
    inner: Option<Arc<dyn XacppSessionHandler>>,
    config: Option<TransferConfig>,
    slot: Arc<AsyncMutex<Option<Arc<XacppSession>>>>,
}

#[async_trait]
impl EstablishHandler for ServeEstablish {
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
        let service: Arc<dyn XacppSessionHandler> = Arc::new(FetchService::new(
            Arc::clone(&session),
            Arc::clone(&self.provider),
            self.inner.clone(),
        ));
        session.wire(transport).await?;
        *self.slot.lock().await = Some(Arc::clone(&session));
        Ok(EstablishDecision::Established {
            session_id: "sess-b".into(),
            handler: service,
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

struct Fixture {
    session_a: Arc<XacppSession>,
    session_b: Arc<XacppSession>,
    ta: Arc<StdioTransport>,
    tb: Arc<StdioTransport>,
    /// Txid of the first data frame the responder put on the wire.
    b_first_data_txid: Arc<FirstDataTxid>,
    _peer_a: XacppPeer,
    _peer_b: XacppPeer,
}

async fn connect_fixture(
    provider: Arc<dyn FetchProvider>,
    b_config: Option<TransferConfig>,
) -> Fixture {
    let (ta_inner, tb_inner) = stdio_duplex();
    let b_first_data_txid = Arc::new(FirstDataTxid::new());
    let ta = Arc::new(BinaryPipe {
        inner: ta_inner,
        first_data_txid: None,
    });
    let tb = Arc::new(BinaryPipe {
        inner: tb_inner,
        first_data_txid: Some(Arc::clone(&b_first_data_txid)),
    });

    let session_a = Arc::new(XacppSession::new(Some(TransferConfig {
        ack_batch_interval_ms: 50,
        tick_interval_ms: 20,
        ..TransferConfig::default()
    })));
    let b_slot: Arc<AsyncMutex<Option<Arc<XacppSession>>>> = Arc::new(AsyncMutex::new(None));

    // Both sides declare the fetch extension (SPEC §8.7): every ext case in
    // this file negotiates fetch enabled before exercising real transfers.
    let fetch_caps = Capabilities {
        fetch_ext: Some(1),
        ..Capabilities::default()
    };

    let peer_a = XacppPeer::new(
        fetch_caps.clone(),
        ta.clone() as Arc<dyn XacppTransport>,
        Some(Arc::clone(&session_a)),
        Arc::new(AcceptNegotiate),
        Arc::new(NoEstablish),
    );
    let peer_b = XacppPeer::new(
        fetch_caps,
        tb.clone() as Arc<dyn XacppTransport>,
        None,
        Arc::new(AcceptNegotiate),
        Arc::new(ServeEstablish {
            provider,
            inner: None,
            config: b_config,
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
    // Unwrap the BinaryPipe wrappers for the raw transports (teardown path).
    let ta_raw = Arc::clone(&ta.inner);
    let tb_raw = Arc::clone(&tb.inner);
    Fixture {
        session_a,
        session_b,
        ta: ta_raw,
        tb: tb_raw,
        b_first_data_txid,
        _peer_a: peer_a,
        _peer_b: peer_b,
    }
}

/// Fast-ACK config variant with small frames and a tight backpressure cap —
/// makes the serving writes park quickly so flow-control commands win the
/// race against completion.
fn throttled_b_config() -> TransferConfig {
    TransferConfig {
        sender: xacpp::reliability::transaction::SenderConfig {
            frame_size: 512,
            backpressure_cap_bytes: 16 * 1024,
            ..Default::default()
        },
        ack_batch_interval_ms: 50,
        tick_interval_ms: 20,
        ..TransferConfig::default()
    }
}

/// Manual re-wire (stream_lifecycle discipline): fresh duplex, receiver
/// wires first (its RESUME request precedes the holder's reader).
async fn rewire_pair(fixture: &Fixture) {
    let (ta, tb) = stdio_duplex();
    let ta = Arc::new(BinaryPipe {
        inner: ta,
        first_data_txid: None,
    });
    let tb = Arc::new(BinaryPipe {
        inner: tb,
        first_data_txid: None,
    });
    tb.connect().await.unwrap();
    fixture
        .session_b
        .wire(tb.clone() as Arc<dyn XacppTransport>)
        .await
        .unwrap();
    ta.connect().await.unwrap();
    fixture
        .session_a
        .wire(ta.clone() as Arc<dyn XacppTransport>)
        .await
        .unwrap();
}

async fn teardown(fixture: &Fixture) {
    fixture.ta.disconnect().await.unwrap();
    fixture.tb.disconnect().await.unwrap();
    // Give the transfer clock task a beat to observe the closed watch and
    // detach the wiring (tick 20 ms; 250 ms is a wide margin).
    tokio::time::sleep(Duration::from_millis(250)).await;
}

fn temp_file(name: &str, data: &[u8]) -> PathBuf {
    let p = std::env::temp_dir().join(format!("xacpp-ext-fetch-{}-{name}", std::process::id()));
    std::fs::write(&p, data).unwrap();
    p
}

async fn wait_until(mut f: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !f() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "condition never became true"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

// ===========================================================================
// Matrix 1: happy_bytes
// ===========================================================================

#[tokio::test]
async fn ext1_happy_bytes() {
    let data = pattern(150_000);
    let provider = CountingBytes::new(data.clone());
    let fixture = connect_fixture(provider, None).await;
    let puller = FetchPuller::new(Arc::clone(&fixture.session_a)).await;

    let (collected, meta) = timeout(puller.pull_to_vec("res://a")).await.unwrap();
    assert_eq!(collected, data);
    assert_32hex(&format_transaction_id(meta.txid));
    assert_eq!(meta.sha256, sha256_hex(&data));
    assert_eq!(meta.size_bytes, data.len() as u64);
}

// ===========================================================================
// Matrix 2: happy_file (temp file > 64 KiB, spans multiple read chunks)
// ===========================================================================

#[tokio::test]
async fn ext2_happy_file() {
    let data = pattern(200_000);
    let path = temp_file("happy.bin", &data);
    let fixture =
        connect_fixture(canned_file(path.clone(), "application/octet-stream"), None).await;
    let puller = FetchPuller::new(Arc::clone(&fixture.session_a)).await;

    let (collected, meta) = timeout(puller.pull_to_vec("res://file")).await.unwrap();
    assert_eq!(collected, data);
    assert_eq!(meta.sha256, sha256_hex(&data));
    assert_eq!(meta.size_bytes, data.len() as u64);

    let _ = std::fs::remove_file(&path);
}

// ===========================================================================
// Matrix 3: sink_access — both exposed faces are the bare kernel objects
// (custom provider writes the real sink; puller returns the real stream)
// ===========================================================================

#[tokio::test]
async fn ext3_sink_access() {
    let data = pattern(120_000);
    let fixture = connect_fixture(Arc::new(DirectWriter { data: data.clone() }), None).await;
    let puller = FetchPuller::new(Arc::clone(&fixture.session_a)).await;

    let (stream, meta) = timeout(puller.pull("res://direct")).await.unwrap();
    // Bare kernel stream object: native accessors work.
    assert_eq!(stream.txid(), meta.txid);
    assert_eq!(stream.total_len() as u64, meta.size_bytes);

    let mut hasher = Sha256::new();
    while let Some(chunk) = timeout(stream.read()).await {
        hasher.update(&chunk.bytes);
    }
    assert_eq!(stream.state(), StreamState::Complete);
    verify(&meta, &format!("{:x}", hasher.finalize())).unwrap();
}

// ===========================================================================
// Matrix 4: duplicate_fetch_idempotent — pull-in-flight + concurrent fetches
// (resolving placeholder serializes; resolve runs exactly once per uri)
// ===========================================================================

#[tokio::test]
async fn ext4_duplicate_fetch_idempotent() {
    // Throttled b-side: the serving writes slowly enough that it is still
    // alive (and thus in the idempotency tables) when the duplicate fetch
    // arrives.
    let data = pattern(512 * 1024);
    let provider = CountingBytes::with_chunk(data.clone(), 4096);
    let fixture = connect_fixture(
        Arc::clone(&provider) as Arc<dyn FetchProvider>,
        Some(throttled_b_config()),
    )
    .await;
    let puller = Arc::new(FetchPuller::new(Arc::clone(&fixture.session_a)).await);

    // Part 1: pull in flight, then a bare fetch of the same uri.
    let pull = {
        let puller = Arc::clone(&puller);
        tokio::spawn(async move { timeout(puller.pull("res://dup")).await })
    };
    tokio::time::sleep(Duration::from_millis(300)).await; // resolve + serving built
    let bare = timeout(fixture.session_a.fetch("res://dup")).await.unwrap();
    let (stream, meta) = pull.await.unwrap().unwrap();
    assert_eq!(bare.txid, meta.txid, "same uri → same serving txid");
    drop(stream);
    assert_eq!(provider.resolve_count.load(Ordering::SeqCst), 1);

    // Part 2: two concurrent bare fetches of a fresh uri (placeholder race);
    // resolve holds 100 ms so the first requester is still inside its
    // resolve await when the second arrives.
    let provider2 = CountingBytes::with_delay(pattern(10_000), Duration::from_millis(100));
    let fixture_b = connect_fixture(provider2.clone(), None).await;
    let (m1, m2) = tokio::join!(
        timeout(fixture_b.session_a.fetch("res://race")),
        timeout(fixture_b.session_a.fetch("res://race"))
    );
    assert_eq!(m1.unwrap().txid, m2.unwrap().txid);
    assert_eq!(provider2.resolve_count.load(Ordering::SeqCst), 1);
}

// ===========================================================================
// Matrix 5: begin_gate — no frame before fetch_begin
// ===========================================================================

#[tokio::test]
async fn ext5_begin_gate() {
    let data = pattern(30_000);
    let provider = CountingBytes::new(data);
    let fixture = connect_fixture(provider, None).await;

    let arrivals = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&arrivals);
    fixture
        .session_a
        .on_binary_stream(Arc::new(move |stream: BinaryStream| {
            counter.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(async move { while stream.read().await.is_some() {} });
        }))
        .await;

    let meta = timeout(fixture.session_a.fetch("res://gate"))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        arrivals.load(Ordering::SeqCst),
        0,
        "begin gate: no stream may arrive before fetch_begin"
    );

    let resp = timeout(fixture.session_a.request_command(XacppCommand::FetchBegin {
        txid: format_transaction_id(meta.txid),
    }))
    .await
    .unwrap();
    assert!(matches!(resp, XacppResponse::Generic { .. }));
    wait_until(|| arrivals.load(Ordering::SeqCst) == 1).await;
}

// ===========================================================================
// Matrix 6: begin_unknown → refused; begin_twice → success, serve called once
// ===========================================================================

#[tokio::test]
async fn ext6_begin_unknown_and_twice() {
    let data = pattern(40_000);
    let provider = CountingBytes::new(data.clone());
    let fixture = connect_fixture(provider.clone(), None).await;

    // Register a bare collector BEFORE any traffic (the puller is not used
    // here — we exercise the begin gate directly).
    let collected: Arc<AsyncMutex<Vec<u8>>> = Arc::new(AsyncMutex::new(Vec::new()));
    {
        let collected = Arc::clone(&collected);
        fixture
            .session_a
            .on_binary_stream(Arc::new(move |stream: BinaryStream| {
                let collected = Arc::clone(&collected);
                tokio::spawn(async move {
                    while let Some(chunk) = stream.read().await {
                        collected.lock().await.extend_from_slice(&chunk.bytes);
                    }
                });
            }))
            .await;
    }

    // Unknown txid → fetch_refused.
    let resp = timeout(fixture.session_a.request_command(XacppCommand::FetchBegin {
        txid: format_transaction_id([7u8; 16]),
    }))
    .await
    .unwrap();
    match resp {
        XacppResponse::Error { code, .. } => assert_eq!(code, error_codes::FETCH_REFUSED),
        other => panic!("expected error response, got {other:?}"),
    }

    // Double begin → both success, serve runs exactly once, one stream flows.
    let meta = timeout(fixture.session_a.fetch("res://twice"))
        .await
        .unwrap();
    let session = Arc::clone(&fixture.session_a);
    let txid = meta.txid;
    let b1 = tokio::spawn(async move {
        timeout(session.request_command(XacppCommand::FetchBegin {
            txid: format_transaction_id(txid),
        }))
        .await
    });
    let session = Arc::clone(&fixture.session_a);
    let txid = meta.txid;
    let b2 = tokio::spawn(async move {
        timeout(session.request_command(XacppCommand::FetchBegin {
            txid: format_transaction_id(txid),
        }))
        .await
    });
    let r1 = b1.await.unwrap().unwrap();
    let r2 = b2.await.unwrap().unwrap();
    assert!(matches!(r1, XacppResponse::Generic { .. }));
    assert!(matches!(r2, XacppResponse::Generic { .. }));

    // Exactly one serve, and the single stream delivers the full payload.
    wait_until(|| provider.serve_count.load(Ordering::SeqCst) == 1).await;
    wait_until(|| {
        collected
            .try_lock()
            .map(|g| g.len() == data.len())
            .unwrap_or(false)
    })
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        provider.serve_count.load(Ordering::SeqCst),
        1,
        "double begin must not spawn a second serve"
    );
    assert_eq!(*collected.lock().await, data);
}

// ===========================================================================
// Matrix 7: abort_mid_stream — bare-stream abort flows to Cancelled, the
// holder evicts the serving (fresh txid on re-fetch), abort is idempotent
// ===========================================================================

#[tokio::test]
async fn ext7_abort_mid_stream() {
    // 4 KiB write chunks under the 16 KiB cap: the serving parks quickly and
    // stays mid-stream long enough for the abort to arrive.
    let data = pattern(512 * 1024);
    let provider = CountingBytes::with_chunk(data, 4096);
    let fixture = connect_fixture(
        Arc::clone(&provider) as Arc<dyn FetchProvider>,
        Some(throttled_b_config()),
    )
    .await;
    let puller = FetchPuller::new(Arc::clone(&fixture.session_a)).await;

    let (stream, meta) = timeout(puller.pull("res://abort")).await.unwrap();
    let _first = timeout(stream.read()).await.unwrap();

    // Abort through the companion command.
    timeout(abort(&fixture.session_a, stream.txid())).await;

    // The stream reaches the Cancelled terminal state (TRANSACTION_ABORT
    // arrives from the holder's forced abort).
    while timeout(stream.read()).await.is_some() {}
    assert_eq!(stream.state(), StreamState::Cancelled);

    // Holder evicted: a fresh fetch of the same uri gets a NEW txid.
    let fresh = timeout(fixture.session_a.fetch("res://abort"))
        .await
        .unwrap();
    assert_ne!(fresh.txid, meta.txid);
    assert_eq!(provider.resolve_count.load(Ordering::SeqCst), 2);

    // Repeated abort → idempotent success (no error surfaced).
    timeout(abort(&fixture.session_a, meta.txid)).await;
}

// ===========================================================================
// Matrix 8: pause_resume — pause holds the flow (no new chunks, Paused
// state), resume completes it
// ===========================================================================

#[tokio::test]
async fn ext8_pause_resume() {
    let data = pattern(256 * 1024);
    let provider = CountingBytes::with_chunk(data, 4096);
    let fixture = connect_fixture(provider, Some(throttled_b_config())).await;
    let puller = FetchPuller::new(Arc::clone(&fixture.session_a)).await;

    let (stream, meta) = timeout(puller.pull("res://flow")).await.unwrap();
    // Every chunk read (first, in-flight drain, post-resume) rolls into one
    // hash — the full payload must arrive exactly once.
    let mut hasher = Sha256::new();
    {
        let first = timeout(stream.read()).await.unwrap();
        hasher.update(&first.bytes);
    }

    timeout(pause(&fixture.session_a, stream.txid()))
        .await
        .unwrap();

    // FIFO guarantees: once the pause frame lands, every in-flight DATA frame
    // was already delivered — but chunks delivered before the pause may still
    // sit queued in the stream's channel. Drain them, then assert the flow is
    // actually held: no chunk may arrive within the window.
    wait_until(|| stream.state() == StreamState::Paused).await;
    loop {
        match tokio::time::timeout(Duration::from_millis(300), stream.read()).await {
            Ok(Some(chunk)) => hasher.update(&chunk.bytes), // delivered before the pause landed
            Ok(None) => panic!("stream terminated while paused"),
            Err(_elapsed) => break, // nothing arrived within 300 ms — flow held
        }
    }

    timeout(resume(&fixture.session_a, stream.txid()))
        .await
        .unwrap();
    while let Some(chunk) = timeout(stream.read()).await {
        hasher.update(&chunk.bytes);
    }
    assert_eq!(stream.state(), StreamState::Complete);
    verify(&meta, &format!("{:x}", hasher.finalize())).unwrap();
}

// ===========================================================================
// Matrix 9: disconnect_resume — the same bare BinaryStream object continues
// across a connection drop + re-wire, completes, verifies
// ===========================================================================

#[tokio::test]
async fn ext9_disconnect_resume() {
    // Throttled b-side: the transfer is still mid-stream when the connection
    // is torn down (default-config servos finish within milliseconds).
    let data = pattern(200_000);
    let provider = CountingBytes::with_chunk(data.clone(), 4096);
    let fixture = connect_fixture(provider, Some(throttled_b_config())).await;
    let puller = FetchPuller::new(Arc::clone(&fixture.session_a)).await;

    let (stream, meta) = timeout(puller.pull("res://resume")).await.unwrap();
    let first = timeout(stream.read()).await.unwrap();

    teardown(&fixture).await;
    // The stream object survives: stalled, not dead.
    assert_eq!(stream.state(), StreamState::Stalled);

    rewire_pair(&fixture).await;

    // SAME object continues: every remaining byte flows through it.
    let mut hasher = Sha256::new();
    hasher.update(&first.bytes);
    while let Some(chunk) = timeout(stream.read()).await {
        hasher.update(&chunk.bytes);
    }
    assert_eq!(stream.state(), StreamState::Complete);
    verify(&meta, &format!("{:x}", hasher.finalize())).unwrap();
}

// ===========================================================================
// Matrix 10: completed_replay_not_redelivered — a replayed stream for a
// terminal transaction is dropped at dispatch; the consumer never sees a
// second delivery, and the session stays healthy for the next fetch.
// (Semantic carried over from the retired kernel-layer guard
// `c_duplicate_binary_frames_delivered_once_wire`: the promise holder is
// the extension's armed-set dispatch, not the bare kernel surface.)
// ===========================================================================

#[tokio::test]
async fn ext10_completed_replay_not_redelivered() {
    let data = pattern(20_000);
    let provider = CountingBytes::new(data.clone());
    let fixture = connect_fixture(provider, None).await;
    let puller = FetchPuller::new(Arc::clone(&fixture.session_a)).await;

    // One full fetch round-trip completes and verifies.
    let (collected, meta) = timeout(puller.pull_to_vec("res://once")).await.unwrap();
    assert_eq!(collected, data);

    // A pathological holder replays the terminal transaction: a bare sink
    // re-opened with the SAME txid re-streams the full body (what an RTO
    // replay looks like on the wire after the receiver completed).
    let sink = timeout(fixture.session_b.open_binary_sink(
        BinaryMeta {
            total_len: data.len() as u32,
        },
        Some(meta.txid),
    ))
    .await
    .unwrap();
    timeout(sink.write(&data)).await.unwrap();
    timeout(sink.finish()).await.unwrap();

    // Let every replayed frame arrive; dispatch has no waiting pull for the
    // terminal txid, so the replayed stream is dropped on the floor.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // The consumer surface is undisturbed and the session healthy: the next
    // fetch round-trip delivers exactly its own bytes.
    let (again, _) = timeout(puller.pull_to_vec("res://twice")).await.unwrap();
    assert_eq!(again, data);
}

// ===========================================================================
// Matrix 11: fetch_unavailable gate — NEITHER side declares the fetch
// extension (SPEC §8.7: absent = no support), so negotiation leaves fetch
// disabled: the initiator's outbound gate answers `fetch_unavailable`
// before anything reaches the wire, and the responder (whose session has
// NO fetch responder loaded either) answers the same loudly per SPEC §8.2.
// Non-fetch commands pass through untouched.
// ===========================================================================

/// Bare establish: the responder session runs WITHOUT the fetch extension
/// loaded (plain NullHandler as the session handler).
struct BareEstablish;

#[async_trait]
impl EstablishHandler for BareEstablish {
    async fn on_establish(
        &self,
        transport: Arc<dyn XacppTransport>,
        _credentials: Option<String>,
    ) -> Result<EstablishDecision, XacppError> {
        let session = Arc::new(
            XacppSession::new(None)
                .with_identity("sess-b".into(), "creds-b".into())
                .await,
        );
        session.wire(transport).await?;
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

#[tokio::test]
async fn ext11_fetch_unavailable_gate() {
    let (ta_inner, tb_inner) = stdio_duplex();
    let ta = Arc::new(BinaryPipe {
        inner: ta_inner,
        first_data_txid: None,
    });
    let tb = Arc::new(BinaryPipe {
        inner: tb_inner,
        first_data_txid: None,
    });

    let session_a = Arc::new(XacppSession::new(Some(TransferConfig {
        ack_batch_interval_ms: 50,
        tick_interval_ms: 20,
        ..TransferConfig::default()
    })));

    let peer_a = XacppPeer::new(
        Capabilities::default(),
        ta.clone() as Arc<dyn XacppTransport>,
        Some(Arc::clone(&session_a)),
        Arc::new(AcceptNegotiate),
        Arc::new(NoEstablish),
    );
    let peer_b = XacppPeer::new(
        Capabilities::default(),
        tb.clone() as Arc<dyn XacppTransport>,
        None,
        Arc::new(AcceptNegotiate),
        Arc::new(BareEstablish),
    );
    peer_a.connect().await.unwrap();
    peer_b.connect().await.unwrap();
    peer_a.negotiate().await.unwrap();
    peer_a
        .establish(None, Arc::new(NullHandler), |_| Ok(()))
        .await
        .unwrap();

    // `fetch` on a peer without a loaded responder: the library answers
    // `fetch_unavailable` (surfaced as an Application error).
    let err = timeout(session_a.fetch("res://anything"))
        .await
        .expect_err("fetch without a loaded responder must fail");
    assert_eq!(err.code(), error_codes::FETCH_UNAVAILABLE);

    // The gate covers the whole fetch family — begin/pause/resume/abort get
    // the same loud answer. Under SPEC §8.7 the initiator-side outbound gate
    // fails them locally (same error shape) without touching the wire.
    for cmd in [
        XacppCommand::FetchBegin {
            txid: "0".repeat(32),
        },
        XacppCommand::FetchPause {
            txid: "0".repeat(32),
        },
        XacppCommand::FetchResume {
            txid: "0".repeat(32),
        },
        XacppCommand::FetchAbort {
            txid: "0".repeat(32),
        },
    ] {
        let err = timeout(session_a.request_command(cmd)).await.unwrap_err();
        assert_eq!(err.code(), error_codes::FETCH_UNAVAILABLE);
    }

    // Non-fetch commands are not the gate's business: they reach the
    // consumer handler untouched.
    match timeout(session_a.request_command(XacppCommand::generic("ping", serde_json::json!({}))))
        .await
        .unwrap()
    {
        XacppResponse::Generic { name, .. } => assert_eq!(name, "acknowledge"),
        other => panic!("non-fetch command must pass through, got {other:?}"),
    }
}

// ===========================================================================
// Matrix 12: sha256 wiring — a truncated transfer is not an integrity
// mismatch
// ===========================================================================

/// A transfer that ends short of the announced size is **truncated**, not
/// corrupted: the payload was never fully delivered, so there is nothing
/// to compare a hash against. The extension must surface the truncation
/// (with the byte gap) instead of reporting a hash mismatch.
#[tokio::test]
async fn ext12_truncated_transfer_is_not_an_integrity_mismatch() {
    let data = pattern(64 * 1024);
    let provider = CountingBytes::with_chunk(data.clone(), 4096);
    // Small frames + a tight backpressure cap: the serving parks mid-stream,
    // so the transfer is still incomplete when the completion is declared.
    let fixture = connect_fixture(
        Arc::clone(&provider) as Arc<dyn FetchProvider>,
        Some(throttled_b_config()),
    )
    .await;
    let puller = FetchPuller::new(Arc::clone(&fixture.session_a)).await;

    let pull = tokio::spawn(async move { puller.pull_to_vec("res://truncated").await });

    // Wait for the responder's first data frame (its txid is observable on
    // the wire) — the frame is already queued at that point, so the
    // injected completion is ordered behind it.
    wait_until(|| fixture.b_first_data_txid.seen.load(Ordering::SeqCst)).await;
    let txid = fixture.b_first_data_txid.value.lock().await.unwrap();

    // Abnormal injection: the holder declares the transaction completed
    // while bytes are still missing.
    let payload = xacpp::wire::control::TransactionCompletedPayload {
        transaction_id: txid,
    }
    .encode()
    .unwrap();
    fixture
        .tb
        .write_data_frame(xacpp::wire::frame::Frame::new(
            xacpp::wire::frame::FrameType::TransactionCompleted,
            [0u8; 16],
            0,
            0,
            payload,
        ))
        .await
        .unwrap();

    // The pull ends with the truncation — not with a hash mismatch.
    match timeout(pull).await.unwrap() {
        Err(FetchError::Incomplete { received, expected }) => {
            assert_eq!(expected, data.len() as u32, "declared total reported");
            assert!(
                received < expected as u64,
                "the transfer really was short ({received} of {expected})"
            );
        }
        Err(other) => panic!("expected an incomplete transfer, got {other:?}"),
        Ok(_) => panic!("a truncated transfer must not succeed"),
    }

    teardown(&fixture).await;
}

// ===========================================================================
// Boundary coverage — the eight edges with zero coverage on both sides.
//
// Ported from the xacpp-ts blueprint (`tests/ext-fetch.test.ts`, cases
// 12-22) and mapped onto the rs API surface. The rs twin differs where
// the API differs:
//
// - ts cancels a pull with an `AbortSignal`; rs cancels by **dropping the
//   pull future** (the pending registration's drop guard fires).
// - ts reports a cancelled stream as `cancelled`; rs reports it as
//   `StreamState::Cancelled`.
// - ts has no "the serving was aborted before it began" wire shape beyond
//   the companion command; rs answers `fetch_abort` for a known-not-begun
//   transaction by evicting the slot and aborting the sink.
//
// Race-shaped cases use **deterministic interleavings** (a serving parked
// at a gate), never a real concurrency race.
// ===========================================================================

// ---- Boundary-coverage providers ----

/// One-way gate: the test opens it, a gated serving waits on it.
///
/// `watch` keeps the state, so a release that happens *before* the waiter
/// arrives is never lost (a plain notify would drop it).
#[derive(Clone)]
struct Gate {
    tx: tokio::sync::watch::Sender<bool>,
    rx: tokio::sync::watch::Receiver<bool>,
}

impl Gate {
    /// A closed gate.
    fn closed() -> Self {
        let (tx, rx) = tokio::sync::watch::channel(false);
        Self { tx, rx }
    }

    /// Opens the gate — every waiter continues.
    fn open(&self) {
        self.tx.send_replace(true);
    }

    /// Waits until the gate is open.
    async fn wait_open(&self) {
        let mut rx = self.rx.clone();
        while !*rx.borrow() {
            if rx.changed().await.is_err() {
                break;
            }
        }
    }
}

/// Where a gated serving parks.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum GateAt {
    /// Before the first byte is written.
    BeforeWrite,
    /// After the payload was written, before the provider returns.
    AfterWrite,
}

/// Provider whose serving parks at a gate: the abort/finish interleavings
/// need a deterministic hold point instead of a real race.
struct GatedBytes {
    data: Vec<u8>,
    /// Uri whose serving parks at the gate (every other uri serves at once).
    gated_uri: String,
    gate_at: GateAt,
    gate: Gate,
    resolve_count: AtomicUsize,
    serve_count: AtomicUsize,
}

impl GatedBytes {
    fn new(data: Vec<u8>, gated_uri: &str, gate_at: GateAt) -> Arc<Self> {
        Arc::new(Self {
            data,
            gated_uri: gated_uri.to_string(),
            gate_at,
            gate: Gate::closed(),
            resolve_count: AtomicUsize::new(0),
            serve_count: AtomicUsize::new(0),
        })
    }

    /// Opens the gate — the parked serving continues.
    fn open_gate(&self) {
        self.gate.open();
    }
}

#[async_trait]
impl FetchProvider for GatedBytes {
    async fn resolve(&self, _uri: &str) -> Result<ResolvedResource, XacppError> {
        self.resolve_count.fetch_add(1, Ordering::SeqCst);
        Ok(ResolvedResource {
            size_bytes: self.data.len() as u64,
            sha256: sha256_hex(&self.data),
            mime: "application/octet-stream".into(),
        })
    }

    async fn serve(
        &self,
        uri: &str,
        sink: Arc<xacpp::reliability::stream::BinarySink>,
    ) -> Result<(), XacppError> {
        self.serve_count.fetch_add(1, Ordering::SeqCst);
        let gated = uri == self.gated_uri;
        if gated && self.gate_at == GateAt::BeforeWrite {
            self.gate.wait_open().await;
        }
        sink.write(&self.data).await?;
        if gated && self.gate_at == GateAt::AfterWrite {
            self.gate.wait_open().await;
        }
        Ok(())
    }
}

/// Provider whose `resolve` fails for one uri (with the peer's wire code)
/// and answers normally for the others. The failing resolve can be delayed
/// to widen the resolving-placeholder window.
struct FailingResolve {
    /// Uri whose resolve fails.
    failing_uri: String,
    /// Wire error code the failing resolve reports.
    code: String,
    /// Delay before the failing resolve answers.
    delay: Option<Duration>,
    /// Payload served for the other uris.
    data: Vec<u8>,
    resolve_count: AtomicUsize,
    serve_count: AtomicUsize,
}

impl FailingResolve {
    fn new(failing_uri: &str, code: &str, data: Vec<u8>, delay: Option<Duration>) -> Arc<Self> {
        Arc::new(Self {
            failing_uri: failing_uri.to_string(),
            code: code.to_string(),
            delay,
            data,
            resolve_count: AtomicUsize::new(0),
            serve_count: AtomicUsize::new(0),
        })
    }
}

#[async_trait]
impl FetchProvider for FailingResolve {
    async fn resolve(&self, uri: &str) -> Result<ResolvedResource, XacppError> {
        self.resolve_count.fetch_add(1, Ordering::SeqCst);
        if uri == self.failing_uri {
            if let Some(d) = self.delay {
                tokio::time::sleep(d).await;
            }
            return Err(XacppError::Application {
                code: self.code.clone(),
                message: "no such resource".into(),
            });
        }
        Ok(ResolvedResource {
            size_bytes: self.data.len() as u64,
            sha256: sha256_hex(&self.data),
            mime: "application/octet-stream".into(),
        })
    }

    async fn serve(
        &self,
        _uri: &str,
        sink: Arc<xacpp::reliability::stream::BinarySink>,
    ) -> Result<(), XacppError> {
        self.serve_count.fetch_add(1, Ordering::SeqCst);
        sink.write(&self.data).await?;
        Ok(())
    }
}

/// Provider that announces the digest of *other* bytes than it serves: the
/// transfer itself is intact, the announced metadata is a lie.
struct LyingDigest {
    data: Vec<u8>,
    declared_sha256: String,
}

#[async_trait]
impl FetchProvider for LyingDigest {
    async fn resolve(&self, _uri: &str) -> Result<ResolvedResource, XacppError> {
        Ok(ResolvedResource {
            size_bytes: self.data.len() as u64,
            sha256: self.declared_sha256.clone(),
            mime: "application/octet-stream".into(),
        })
    }

    async fn serve(
        &self,
        _uri: &str,
        sink: Arc<xacpp::reliability::stream::BinarySink>,
    ) -> Result<(), XacppError> {
        sink.write(&self.data).await?;
        Ok(())
    }
}

/// Provider that announces a scripted size: the size gates must refuse
/// before anything is served, so the payload never matters.
struct ScriptedSize {
    size_bytes: u64,
    sha256: String,
    serve_count: AtomicUsize,
}

impl ScriptedSize {
    fn new(size_bytes: u64, sha256: String) -> Arc<Self> {
        Arc::new(Self {
            size_bytes,
            sha256,
            serve_count: AtomicUsize::new(0),
        })
    }
}

#[async_trait]
impl FetchProvider for ScriptedSize {
    async fn resolve(&self, _uri: &str) -> Result<ResolvedResource, XacppError> {
        Ok(ResolvedResource {
            size_bytes: self.size_bytes,
            sha256: self.sha256.clone(),
            mime: "application/octet-stream".into(),
        })
    }

    async fn serve(
        &self,
        _uri: &str,
        _sink: Arc<xacpp::reliability::stream::BinarySink>,
    ) -> Result<(), XacppError> {
        self.serve_count.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

/// Provider that writes a prefix and then fails: the serving must be forced
/// into the abort terminal state (never "completed" — the declared bytes
/// never arrived).
struct FailingServe {
    data: Vec<u8>,
    /// Bytes written before the failure.
    prefix: usize,
    resolve_count: AtomicUsize,
}

#[async_trait]
impl FetchProvider for FailingServe {
    async fn resolve(&self, _uri: &str) -> Result<ResolvedResource, XacppError> {
        self.resolve_count.fetch_add(1, Ordering::SeqCst);
        Ok(ResolvedResource {
            size_bytes: self.data.len() as u64,
            sha256: sha256_hex(&self.data),
            mime: "application/octet-stream".into(),
        })
    }

    async fn serve(
        &self,
        _uri: &str,
        sink: Arc<xacpp::reliability::stream::BinarySink>,
    ) -> Result<(), XacppError> {
        sink.write(&self.data[..self.prefix]).await?;
        Err(XacppError::Internal("provider exploded mid-serve".into()))
    }
}

/// Provider that declares `declared` bytes but serves only `written`: the
/// short-write case — the holder must announce the shortfall instead of
/// leaving the peer waiting for bytes that will never arrive.
struct ShortWrite {
    /// The announced payload (its length is the announced size).
    declared: Vec<u8>,
    /// How many bytes are actually written.
    written: usize,
    resolve_count: AtomicUsize,
}

#[async_trait]
impl FetchProvider for ShortWrite {
    async fn resolve(&self, _uri: &str) -> Result<ResolvedResource, XacppError> {
        self.resolve_count.fetch_add(1, Ordering::SeqCst);
        Ok(ResolvedResource {
            size_bytes: self.declared.len() as u64,
            sha256: sha256_hex(&self.declared),
            mime: "application/octet-stream".into(),
        })
    }

    async fn serve(
        &self,
        _uri: &str,
        sink: Arc<xacpp::reliability::stream::BinarySink>,
    ) -> Result<(), XacppError> {
        sink.write(&self.declared[..self.written]).await?;
        Ok(())
    }
}

// ===========================================================================
// Boundary 12 (ts case 12): the consumer abandons a pull — the pending
// registration is torn down and the late stream is dropped, never
// delivered. rs cancels by dropping the pull future (the pending
// registration's drop guard fires).
// ===========================================================================

#[tokio::test]
async fn ext13_pull_cancelled_by_drop() {
    let data = pattern(8 * 1024);
    let provider = GatedBytes::new(data.clone(), "res://cancel", GateAt::BeforeWrite);
    let fixture = connect_fixture(Arc::clone(&provider) as Arc<dyn FetchProvider>, None).await;
    let puller = Arc::new(FetchPuller::new(Arc::clone(&fixture.session_a)).await);

    // The pull authorizes the serving; the provider is parked at the gate,
    // so not a single byte has flown yet.
    let pull = {
        let puller = Arc::clone(&puller);
        tokio::spawn(async move { puller.pull("res://cancel").await })
    };
    wait_until(|| provider.serve_count.load(Ordering::SeqCst) == 1).await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    // The consumer cancels: the pull future is dropped → the pending
    // registration's guard fires (the entry is dead from now on).
    pull.abort();
    match pull.await {
        Err(join_err) => assert!(
            join_err.is_cancelled(),
            "the cancelled pull must not deliver a stream"
        ),
        Ok(_) => panic!("the cancelled pull must not deliver a stream"),
    }

    // The serving continues: the bytes reach the peer — but the pull is
    // gone, so the late stream is dropped instead of being delivered.
    provider.open_gate();
    tokio::time::sleep(Duration::from_millis(300)).await;

    // The session stays healthy: the next pull completes with exactly its
    // own bytes (the dropped stream leaked into nothing).
    let (collected, _) = timeout(puller.pull_to_vec("res://after-cancel"))
        .await
        .unwrap();
    assert_eq!(collected, data);
    assert_eq!(provider.resolve_count.load(Ordering::SeqCst), 2);
}

// ===========================================================================
// Boundary 13 (ts case 13): a dead peer never resolves the pull — the pull
// has no timeout by design (the pending registration survives disconnects),
// so the consumer's own cancellation is the only way out.
// ===========================================================================

#[tokio::test]
async fn ext14_dead_peer_keeps_pull_pending() {
    let data = pattern(4 * 1024);
    let provider = GatedBytes::new(data.clone(), "res://dead", GateAt::BeforeWrite);
    let fixture = connect_fixture(Arc::clone(&provider) as Arc<dyn FetchProvider>, None).await;
    let puller = Arc::new(FetchPuller::new(Arc::clone(&fixture.session_a)).await);

    let pull = {
        let puller = Arc::clone(&puller);
        tokio::spawn(async move { puller.pull("res://dead").await })
    };
    wait_until(|| provider.serve_count.load(Ordering::SeqCst) == 1).await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    // The peer dies for good: the serving is parked, nothing will arrive.
    teardown(&fixture).await;

    // The pull stays suspended — no timeout, no error, no spontaneous
    // resolution. The consumer's own cancellation is the only way out.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!pull.is_finished(), "a dead peer must not resolve the pull");

    // Self-rescue: the consumer drops the pull future (rs's cancellation).
    pull.abort();
    match pull.await {
        Err(join_err) => assert!(
            join_err.is_cancelled(),
            "the abandoned pull must not deliver a stream"
        ),
        Ok(_) => panic!("the abandoned pull must not deliver a stream"),
    }

    // The dead session fails explicitly too — it does not hang either.
    match timeout(puller.pull("res://after-death")).await {
        Err(FetchError::Transport(_)) => {}
        Err(other) => panic!("expected a transport failure, got {other:?}"),
        Ok(_) => panic!("a dead session must not deliver a stream"),
    }
}

// ===========================================================================
// Boundary 14 (ts case 14): a provider that declares a digest it never
// serves is rejected by the consumer — the transfer itself is intact, the
// refusal is the integrity check.
// ===========================================================================

#[tokio::test]
async fn ext15_integrity_mismatch_end_to_end() {
    let data = pattern(4 * 1024);
    // The announced digest belongs to different bytes than the served ones.
    let declared = sha256_hex(b"not the bytes that get served");
    let provider = LyingDigest {
        data: data.clone(),
        declared_sha256: declared.clone(),
    };
    let fixture = connect_fixture(Arc::new(provider), None).await;
    let puller = FetchPuller::new(Arc::clone(&fixture.session_a)).await;

    match timeout(puller.pull_to_vec("res://liar")).await {
        Err(FetchError::IntegrityMismatch {
            expected,
            actual,
            received,
        }) => {
            assert_eq!(expected, declared, "the announced digest is reported");
            assert_eq!(actual, sha256_hex(&data), "the served digest is reported");
            assert_eq!(received, data.len() as u64, "the served byte count");
        }
        Err(other) => panic!("expected an integrity mismatch, got {other:?}"),
        Ok(_) => panic!("a lying digest must not succeed"),
    }
}

// ===========================================================================
// Boundary 15 (ts case 15): a provider that cannot resolve answers a wire
// error; the pull surfaces a refusal and leaves no residue.
// ===========================================================================

#[tokio::test]
async fn ext16_resolve_failure_leaves_no_residue() {
    let data = pattern(2 * 1024);
    let provider = FailingResolve::new(
        "res://missing",
        error_codes::FETCH_NOT_FOUND,
        data.clone(),
        None,
    );
    let fixture = connect_fixture(Arc::clone(&provider) as Arc<dyn FetchProvider>, None).await;

    // Wire level: the fetch command answers an error response, never a ready.
    match timeout(fixture.session_a.request_command(XacppCommand::Fetch {
        uri: "res://missing".into(),
    }))
    .await
    .unwrap()
    {
        XacppResponse::Error { code, .. } => assert_eq!(code, error_codes::FETCH_NOT_FOUND),
        other => panic!("expected an error response, got {other:?}"),
    }

    // Puller level: the peer's code is surfaced as a refusal.
    let puller = FetchPuller::new(Arc::clone(&fixture.session_a)).await;
    match timeout(puller.pull("res://missing")).await {
        Err(FetchError::Refused { code, .. }) => assert_eq!(code, error_codes::FETCH_NOT_FOUND),
        Err(other) => panic!("expected a refusal, got {other:?}"),
        Ok(_) => panic!("a failed resolve must not yield a stream"),
    }

    // No residue: the failed attempt evicted its slot — the retry resolved
    // again (resolve ran twice, not once).
    assert_eq!(provider.resolve_count.load(Ordering::SeqCst), 2);

    // The session is healthy: another resource pulls through.
    let (collected, _) = timeout(puller.pull_to_vec("res://ok")).await.unwrap();
    assert_eq!(collected, data);
}

// ===========================================================================
// Boundary 16 (ts case 16): a provider that fails mid-serve ends the
// transaction in the abort terminal state — never "completed": the declared
// bytes never arrived.
// ===========================================================================

#[tokio::test]
async fn ext17_serve_failure_forces_abort() {
    let data = pattern(8 * 1024);
    let provider = Arc::new(FailingServe {
        data: data.clone(),
        prefix: 1024,
        resolve_count: AtomicUsize::new(0),
    });
    let fixture = connect_fixture(
        Arc::clone(&provider) as Arc<dyn FetchProvider>,
        Some(throttled_b_config()),
    )
    .await;
    let puller = FetchPuller::new(Arc::clone(&fixture.session_a)).await;

    let (stream, meta) = timeout(puller.pull("res://servefail")).await.unwrap();

    // The failed serving is forced into the abort terminal state.
    let mut received = 0usize;
    while let Some(chunk) = timeout(stream.read()).await {
        received += chunk.bytes.len();
    }
    assert_eq!(stream.state(), StreamState::Cancelled);
    assert!(
        received < data.len(),
        "the transfer really was short ({received} of {})",
        data.len()
    );

    // The failed serving is evicted: a retry re-resolves under a new txid.
    let ready2 = timeout(fixture.session_a.fetch("res://servefail"))
        .await
        .unwrap();
    assert_ne!(ready2.txid, meta.txid);
    assert_eq!(provider.resolve_count.load(Ordering::SeqCst), 2);
}

// ===========================================================================
// Boundary 17 (ts case 17): aborting an announced-but-never-begun
// transaction succeeds, serves nothing and evicts the slot.
// ===========================================================================

#[tokio::test]
async fn ext18_abort_not_begun() {
    let data = pattern(2 * 1024);
    let provider = CountingBytes::new(data.clone());
    let fixture = connect_fixture(Arc::clone(&provider) as Arc<dyn FetchProvider>, None).await;

    // Watch the consumer side: an aborted-before-begun transaction must
    // never deliver a stream.
    let arrivals = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&arrivals);
    fixture
        .session_a
        .on_binary_stream(Arc::new(move |stream: BinaryStream| {
            counter.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(async move { while stream.read().await.is_some() {} });
        }))
        .await;

    let meta = timeout(fixture.session_a.fetch("res://nobegin"))
        .await
        .unwrap();

    // The abort answers success (SPEC §8.3 idempotency).
    let ack = timeout(fixture.session_a.request_command(XacppCommand::FetchAbort {
        txid: format_transaction_id(meta.txid),
    }))
    .await
    .unwrap();
    assert!(matches!(ack, XacppResponse::Generic { .. }));

    // No serving task ever existed → the provider never ran, no frame flew.
    assert_eq!(provider.serve_count.load(Ordering::SeqCst), 0);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(arrivals.load(Ordering::SeqCst), 0);

    // The slot is gone: a later begin for that txid is refused as unknown.
    let late = timeout(fixture.session_a.request_command(XacppCommand::FetchBegin {
        txid: format_transaction_id(meta.txid),
    }))
    .await
    .unwrap();
    match late {
        XacppResponse::Error { code, .. } => assert_eq!(code, error_codes::FETCH_REFUSED),
        other => panic!("expected a refusal, got {other:?}"),
    }

    // A retry re-resolves under a new txid.
    let meta2 = timeout(fixture.session_a.fetch("res://nobegin"))
        .await
        .unwrap();
    assert_ne!(meta2.txid, meta.txid);
    assert_eq!(provider.resolve_count.load(Ordering::SeqCst), 2);
}

// ===========================================================================
// Boundary 18 (ts case 18): a zero-length resource is refused loudly and
// never served.
// ===========================================================================

#[tokio::test]
async fn ext19_size_zero_rejected() {
    let provider = ScriptedSize::new(0, sha256_hex(&[]));
    let fixture = connect_fixture(Arc::clone(&provider) as Arc<dyn FetchProvider>, None).await;

    // Wire level: a loud, registered refusal — never a fetch_ready.
    match timeout(fixture.session_a.request_command(XacppCommand::Fetch {
        uri: "res://zero".into(),
    }))
    .await
    .unwrap()
    {
        XacppResponse::Error { code, .. } => assert_eq!(code, error_codes::FETCH_REFUSED),
        other => panic!("expected a refusal, got {other:?}"),
    }
    assert_eq!(provider.serve_count.load(Ordering::SeqCst), 0);

    // Puller level: the refusal surfaces as a FetchError.
    let puller = FetchPuller::new(Arc::clone(&fixture.session_a)).await;
    match timeout(puller.pull("res://zero")).await {
        Err(FetchError::Refused { code, .. }) => assert_eq!(code, error_codes::FETCH_REFUSED),
        Err(other) => panic!("expected a refusal, got {other:?}"),
        Ok(_) => panic!("a zero-length resource must not be served"),
    }
}

// ===========================================================================
// Boundary 19 (ts case 19): a size beyond the wire's u32 totalLen must be
// refused, not advertised. The rs twin owns both size gates (SPEC §3 caps
// a transaction at 4 GiB — the frame header carries total_len as u32).
// ===========================================================================

#[tokio::test]
async fn ext20_size_oversized_refused() {
    let oversize = u32::MAX as u64 + 1; // one past the u32 totalLen
    let provider = ScriptedSize::new(oversize, "aa".repeat(32));
    let fixture = connect_fixture(Arc::clone(&provider) as Arc<dyn FetchProvider>, None).await;

    // Wire level: refused loudly, never advertised as a fetch_ready.
    match timeout(fixture.session_a.request_command(XacppCommand::Fetch {
        uri: "res://huge".into(),
    }))
    .await
    .unwrap()
    {
        XacppResponse::Error { code, .. } => assert_eq!(code, error_codes::FETCH_REFUSED),
        other => panic!("expected a refusal, got {other:?}"),
    }
    assert_eq!(
        provider.serve_count.load(Ordering::SeqCst),
        0,
        "an unannounceable size must never be served"
    );
}

// ===========================================================================
// Boundary 20 (ts case 20): the abort command and the provider's completion
// — exactly one terminal state, healthy session. Two deterministic
// interleavings of the same pair (no real race):
//   A: the abort lands while the serving task is still alive;
//   B: the serving completes first, the abort finds the slot already
//      evicted and answers idempotently.
// ===========================================================================

#[tokio::test]
async fn ext21_abort_finish_interleaving() {
    let data = pattern(8 * 1024);
    // The serving parks *after* the payload was written — the completion
    // (the provider's return) is held right before the finish.
    let provider = GatedBytes::new(data.clone(), "res://race", GateAt::AfterWrite);
    // Small frames (512 B): the payload leaves the holder while the
    // provider is still parked — with the default 1 MiB frame size the
    // bytes would sit in the sender's partial buffer until the finish.
    let fixture = connect_fixture(
        Arc::clone(&provider) as Arc<dyn FetchProvider>,
        Some(throttled_b_config()),
    )
    .await;
    let puller = FetchPuller::new(Arc::clone(&fixture.session_a)).await;

    // Part A: the abort lands while the serving task is still alive.
    let (stream, meta) = timeout(puller.pull("res://race")).await.unwrap();
    let mut received = 0usize;
    while let Some(chunk) = timeout(stream.read()).await {
        received += chunk.bytes.len();
    }
    assert_eq!(received, data.len(), "the whole resource arrived");
    assert_eq!(stream.state(), StreamState::Complete);

    let ack = timeout(fixture.session_a.request_command(XacppCommand::FetchAbort {
        txid: format_transaction_id(meta.txid),
    }))
    .await
    .unwrap();
    assert!(matches!(ack, XacppResponse::Generic { .. }));

    // The late abort adds no second terminal state.
    provider.open_gate();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(stream.state(), StreamState::Complete);
    assert!(timeout(stream.read()).await.is_none());

    // Part B: the serving completes first — the abort then finds the slot
    // already evicted and answers idempotently (SPEC §8.3).
    let (stream2, meta2) = timeout(puller.pull("res://race2")).await.unwrap();
    let mut received2 = 0usize;
    while let Some(chunk) = timeout(stream2.read()).await {
        received2 += chunk.bytes.len();
    }
    assert_eq!(received2, data.len());
    assert_eq!(stream2.state(), StreamState::Complete);
    tokio::time::sleep(Duration::from_millis(300)).await; // the serving evicts its slot
    let ack2 = timeout(fixture.session_a.request_command(XacppCommand::FetchAbort {
        txid: format_transaction_id(meta2.txid),
    }))
    .await
    .unwrap();
    assert!(matches!(ack2, XacppResponse::Generic { .. }));
    assert_eq!(stream2.state(), StreamState::Complete);

    // The session and the serving table survive the race.
    let (again, _) = timeout(puller.pull_to_vec("res://after-race"))
        .await
        .unwrap();
    assert_eq!(again, data);
}

// ===========================================================================
// Boundary 21 (ts case 21): the resolving placeholder — a concurrent fetch
// waits out the in-flight resolve and the placeholder's failure reaches
// every waiter with the same wire code.
// ===========================================================================

#[tokio::test]
async fn ext22_failed_resolve_reaches_the_waiter() {
    let data = pattern(2 * 1024);
    let provider = FailingResolve::new(
        "res://boom",
        error_codes::FETCH_NOT_FOUND,
        data.clone(),
        Some(Duration::from_millis(300)),
    );
    let fixture = connect_fixture(Arc::clone(&provider) as Arc<dyn FetchProvider>, None).await;

    // The first fetch starts the (slow) resolve.
    let first = {
        let session = Arc::clone(&fixture.session_a);
        tokio::spawn(async move { session.fetch("res://boom").await })
    };
    wait_until(|| provider.resolve_count.load(Ordering::SeqCst) == 1).await;

    // The second fetch asks for the same uri while the resolve is in flight.
    let second = {
        let session = Arc::clone(&fixture.session_a);
        tokio::spawn(async move { session.fetch("res://boom").await })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        !first.is_finished() && !second.is_finished(),
        "both fetches must wait out the in-flight resolve"
    );

    // The failure reaches every waiter with the same wire code.
    let e1 = first.await.unwrap().unwrap_err();
    let e2 = second.await.unwrap().unwrap_err();
    assert_eq!(e1.code(), error_codes::FETCH_NOT_FOUND);
    assert_eq!(e2.code(), error_codes::FETCH_NOT_FOUND);

    // No residue: the failed attempt evicted its slot — the retry resolved
    // again (the count grows by exactly one resolve).
    let before = provider.resolve_count.load(Ordering::SeqCst);
    let e3 = timeout(fixture.session_a.fetch("res://boom"))
        .await
        .unwrap_err();
    assert_eq!(e3.code(), error_codes::FETCH_NOT_FOUND);
    assert_eq!(
        provider.resolve_count.load(Ordering::SeqCst),
        before + 1,
        "the failed attempt left no residue — the retry re-resolved"
    );
}

// ===========================================================================
// Boundary 22 (ts case 22), part A: a provider that delivers fewer bytes
// than it declared ends the stream explicitly — the consumer is never left
// waiting for bytes that will not arrive. The holder announced the abort
// (a short write is never announced as a completion).
// ===========================================================================

#[tokio::test]
async fn ext23_short_write_ends_the_stream_explicitly() {
    let declared = pattern(8 * 1024);
    let provider = Arc::new(ShortWrite {
        declared: declared.clone(),
        written: 1024,
        resolve_count: AtomicUsize::new(0),
    });
    let fixture = connect_fixture(
        Arc::clone(&provider) as Arc<dyn FetchProvider>,
        Some(throttled_b_config()),
    )
    .await;
    let puller = FetchPuller::new(Arc::clone(&fixture.session_a)).await;

    let (stream, _meta) = timeout(puller.pull("res://shortwrite")).await.unwrap();

    // The stream terminates explicitly (no hang): the holder announced the
    // abort, so the consumer is not left waiting for the missing bytes.
    let mut received = 0usize;
    while let Some(chunk) = timeout(stream.read()).await {
        received += chunk.bytes.len();
    }
    assert_eq!(stream.state(), StreamState::Cancelled);
    assert!(
        received < declared.len(),
        "the transfer really was short ({received} of {})",
        declared.len()
    );
}

// ===========================================================================
// Boundary 22 (ts case 22), part B: the convenience pull fails explicitly
// — the digest verdict is never issued for a transaction that did not
// complete.
//
// RED on purpose (rs product gap, not a test defect): the rs
// `FetchPuller::pull_to_vec` (src/ext/fetch.rs) only inspects the
// `Incomplete` terminal via `BinaryStream::incomplete_detail()`. A stream
// that ended `Cancelled` — the holder's short-write abort — falls through
// to the digest comparison and is reported as an integrity mismatch. The
// ts twin refuses it explicitly ("terminal state first: the digest verdict
// is only meaningful for a transaction that actually completed").
// The fix belongs in src/ext/fetch.rs, which this task must not touch.
// ===========================================================================

#[tokio::test]
async fn ext24_short_write_never_produces_an_integrity_verdict() {
    let declared = pattern(8 * 1024);
    let provider = Arc::new(ShortWrite {
        declared: declared.clone(),
        written: 1024,
        resolve_count: AtomicUsize::new(0),
    });
    let fixture = connect_fixture(
        Arc::clone(&provider) as Arc<dyn FetchProvider>,
        Some(throttled_b_config()),
    )
    .await;
    let puller = FetchPuller::new(Arc::clone(&fixture.session_a)).await;

    // The pull ends explicitly (the timeout would fire on a hang) …
    let outcome = timeout(puller.pull_to_vec("res://shortwrite")).await;
    assert!(outcome.is_err(), "a short write must not succeed");

    // … and never as an integrity verdict.
    assert!(
        !matches!(outcome, Err(FetchError::IntegrityMismatch { .. })),
        "a transaction that never completed must not produce an integrity verdict"
    );
}

// ===========================================================================
// Boundary 25: a file that does not exist fails explicitly at resolve —
// the tokio::fs open error surfaces as a fetch error, never a hang.
// ===========================================================================

#[tokio::test]
async fn ext25_missing_file_fails_explicitly() {
    let path = std::env::temp_dir().join(format!(
        "xacpp-ext-fetch-{}-does-not-exist.bin",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);

    let fixture = connect_fixture(canned_file(path, "application/octet-stream"), None).await;
    let puller = FetchPuller::new(Arc::clone(&fixture.session_a)).await;

    let outcome = timeout(puller.pull_to_vec("res://missing")).await;
    assert!(
        outcome.is_err(),
        "a missing file must fail the pull, not hang or succeed"
    );
}

// ===========================================================================
// Boundary 26: two concurrent file pulls over one session keep their own
// streams — no cross-talk, both digests verify.
// ===========================================================================

#[tokio::test]
async fn ext26_concurrent_file_streams_stay_isolated() {
    let data = pattern(300_000);
    let path = temp_file("concurrent.bin", &data);
    let fixture =
        connect_fixture(canned_file(path.clone(), "application/octet-stream"), None).await;
    let puller = Arc::new(FetchPuller::new(Arc::clone(&fixture.session_a)).await);

    let (ra, rb) = tokio::join!(
        timeout(puller.pull_to_vec("res://a")),
        timeout(puller.pull_to_vec("res://b")),
    );

    let (da, ma) = ra.expect("pull a must succeed");
    let (db, mb) = rb.expect("pull b must succeed");
    assert_eq!(da, data, "stream a must carry the whole file");
    assert_eq!(db, data, "stream b must carry the whole file");
    assert_eq!(ma.sha256, mb.sha256);
    verify(&ma, &sha256_hex(&data)).unwrap();
    verify(&mb, &sha256_hex(&data)).unwrap();

    let _ = std::fs::remove_file(&path);
}
