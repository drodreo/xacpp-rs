//! Fetch kernel tests (SPEC §8 stateless kernel, matrix items F1..F6).
//!
//! The kernel keeps no fetch state: `fetch` commands fall through to the
//! consumer's `SessionHandler::on_command` (the Peer layer does not
//! intercept), the responder answers `fetch_ready` and streams the bytes
//! via `open_binary_sink` with the announced txid, and the requester claims
//! the stream via `on_binary_stream` by matching `fetch_ready.txid`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::io::BufReader;
use tokio::sync::Mutex as AsyncMutex;

use std::collections::HashMap as StdHashMap;
use xacpp::carrier::XacppTransport;
use xacpp::carrier::stdio::StdioTransport;
use xacpp::reliability::stream::{BinaryMeta, BinaryStream, StreamState};
use xacpp::session::XacppSession;
use xacpp::session::handler::RequestHandler;
use xacpp::session::handler::{
    EstablishDecision, EstablishHandler, NegotiateHandler, XacppSessionHandler,
};
use xacpp::session::peer::XacppPeer;
use xacpp::transfer::TransferActivityHandlers;
use xacpp::transfer::{TransferConfig, TransferWiring};
use xacpp::wire::capability::{Capabilities, EffectiveCapabilities};
use xacpp::wire::commands::XacppCommand;
use xacpp::wire::error::XacppError;
use xacpp::wire::events::XacppActivityEvent;
use xacpp::wire::fetch::error_codes;
use xacpp::wire::fetch::format_transaction_id;
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
    use sha2::Digest;
    format!("{:x}", sha2::Sha256::digest(bytes))
}

// ---- Test transport: stdio-duplex pipe with the binary channel enabled ----

struct BinaryPipe {
    inner: Arc<StdioTransport>,
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
        self.inner.write_data_frame(frame).await
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

// ---- Responder side: consumer SessionHandler that serves fetch (F1/F2) ----

/// The fetch responder IS a consumer handler: it answers `fetch` commands
/// from `on_command` and streams the resource through a `BinarySink` opened
/// with the announced txid. Hand-rolled responders must declare themselves
/// via `is_fetch_responder` — otherwise the peer gates fetch-family
/// commands with `fetch_unavailable` (SPEC §8.2) before they reach
/// `on_command`.
struct FetchServeHandler {
    resources: Arc<StdHashMap<String, Vec<u8>>>,
    mime: String,
    session: std::sync::Weak<XacppSession>,
}

#[async_trait]
impl XacppSessionHandler for FetchServeHandler {
    fn is_fetch_responder(&self) -> bool {
        true
    }

    async fn on_command(&self, command: XacppCommand) -> Result<XacppResponse, XacppError> {
        let XacppCommand::Fetch { uri } = command else {
            return Ok(XacppResponse::acknowledge());
        };
        let Some(bytes) = self.resources.get(&uri) else {
            return Ok(XacppResponse::error(
                error_codes::FETCH_NOT_FOUND,
                format!("unknown uri: {uri}"),
            ));
        };
        let Some(session) = self.session.upgrade() else {
            return Ok(XacppResponse::error(
                error_codes::FETCH_REFUSED,
                "responder session is gone",
            ));
        };
        let txid = uuid::Uuid::new_v4().into_bytes();
        let sink = session
            .open_binary_sink(
                BinaryMeta {
                    total_len: bytes.len() as u32,
                },
                Some(txid),
            )
            .await
            .map_err(|e| XacppError::Internal(e.to_string()))?;
        let response = XacppResponse::FetchReady {
            txid: format_transaction_id(txid),
            sha256: sha256_hex(bytes),
            size_bytes: bytes.len() as u64,
            mime: self.mime.clone(),
        };
        // SPEC §8.4: the response MUST complete before the binary
        // transaction starts — the response travels with this return value;
        // the stream task yields first so the response frame is queued
        // ahead of the data frames on the single FIFO egress.
        let bytes = bytes.clone();
        tokio::spawn(async move {
            for _ in 0..10 {
                tokio::task::yield_now().await;
            }
            let mut offset = 0usize;
            while offset < bytes.len() {
                let end = (offset + 64 * 1024).min(bytes.len());
                if sink.write(&bytes[offset..end]).await.is_err() {
                    return;
                }
                offset = end;
            }
            let _ = sink.finish().await;
        });
        Ok(response)
    }

    async fn on_event(&self, _event: XacppActivityEvent) -> Result<XacppResponse, XacppError> {
        Ok(XacppResponse::acknowledge())
    }
}

struct RefuseHandler;

#[async_trait]
impl XacppSessionHandler for RefuseHandler {
    // Hand-rolled refusal is still a fetch responder: declaring it lets the
    // `fetch_refused` answer come from the consumer instead of the peer's
    // `fetch_unavailable` gate.
    fn is_fetch_responder(&self) -> bool {
        true
    }

    async fn on_command(&self, command: XacppCommand) -> Result<XacppResponse, XacppError> {
        let XacppCommand::Fetch { uri } = command else {
            return Ok(XacppResponse::acknowledge());
        };
        Ok(XacppResponse::error(
            error_codes::FETCH_REFUSED,
            format!("uri outside the servable scope: {uri}"),
        ))
    }

    async fn on_event(&self, _event: XacppActivityEvent) -> Result<XacppResponse, XacppError> {
        Ok(XacppResponse::acknowledge())
    }
}

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

/// Responder establish: builds the session, arms the fetch-serving handler
/// with a Weak back-reference, wires, and answers Established.
struct ServeEstablish {
    resources: Arc<StdHashMap<String, Vec<u8>>>,
    mime: String,
    handler_override: Option<Arc<dyn XacppSessionHandler>>,
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
            XacppSession::new(None)
                .with_identity("sess-b".into(), "creds-b".into())
                .await,
        );
        let handler: Arc<dyn XacppSessionHandler> = match &self.handler_override {
            Some(h) => Arc::clone(h),
            None => Arc::new(FetchServeHandler {
                resources: Arc::clone(&self.resources),
                mime: self.mime.clone(),
                session: Arc::downgrade(&session),
            }),
        };
        session.wire(transport).await?;
        *self.slot.lock().await = Some(Arc::clone(&session));
        Ok(EstablishDecision::Established {
            session_id: "sess-b".into(),
            handler,
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

// ---- Fixture ----

struct Fixture {
    session_a: Arc<XacppSession>,
    _peer_a: XacppPeer,
    _peer_b: XacppPeer,
}

async fn connect_pair(
    handler_override: Option<Arc<dyn XacppSessionHandler>>,
    a_plain_stdio: bool,
) -> Fixture {
    let (ta_inner, tb_inner) = stdio_duplex();
    let ta: Arc<dyn XacppTransport> = if a_plain_stdio {
        ta_inner
    } else {
        Arc::new(BinaryPipe { inner: ta_inner })
    };
    let tb: Arc<dyn XacppTransport> = Arc::new(BinaryPipe { inner: tb_inner });

    let session_a = Arc::new(XacppSession::new(Some(TransferConfig {
        ack_batch_interval_ms: 50,
        tick_interval_ms: 20,
        ..TransferConfig::default()
    })));
    let b_slot: Arc<AsyncMutex<Option<Arc<XacppSession>>>> = Arc::new(AsyncMutex::new(None));

    let mut resources = HashMap::new();
    resources.insert("file:///tmp/a.png".to_string(), vec![1u8; 200_000]);
    let peer_a = XacppPeer::new(
        Capabilities::default(),
        Arc::clone(&ta),
        Some(Arc::clone(&session_a)),
        Arc::new(AcceptNegotiate),
        Arc::new(NoEstablish),
    );
    let peer_b = XacppPeer::new(
        Capabilities::default(),
        tb,
        None,
        Arc::new(AcceptNegotiate),
        Arc::new(ServeEstablish {
            resources: Arc::new(resources),
            mime: "image/png".into(),
            handler_override,
            slot: b_slot,
        }),
    );
    peer_a.connect().await.unwrap();
    peer_b.connect().await.unwrap();
    peer_a.negotiate().await.unwrap();
    peer_a
        .establish(None, Arc::new(NullHandler), |_| Ok(()))
        .await
        .unwrap();
    Fixture {
        session_a,
        _peer_a: peer_a,
        _peer_b: peer_b,
    }
}

/// Claims the next inbound stream on `session` (registered before any
/// traffic) and returns (txid, total_len, chunks, terminal state).
async fn claim_stream(session: &Arc<XacppSession>) -> Arc<StreamClaim> {
    let claim = Arc::new(StreamClaim {
        meta: tokio::sync::Mutex::new(None),
        chunks: tokio::sync::Mutex::new(Vec::new()),
        terminal: tokio::sync::Mutex::new(None),
    });
    {
        let claim = Arc::clone(&claim);
        session
            .on_binary_stream(Arc::new(move |stream: BinaryStream| {
                let claim = Arc::clone(&claim);
                tokio::spawn(async move {
                    *claim.meta.lock().await = Some((stream.txid(), stream.total_len()));
                    while let Some(chunk) = stream.read().await {
                        claim.chunks.lock().await.push(chunk.bytes);
                    }
                    *claim.terminal.lock().await = Some(stream.state());
                });
            }))
            .await;
    }
    claim
}

struct StreamClaim {
    meta: tokio::sync::Mutex<Option<([u8; 16], u32)>>,
    chunks: tokio::sync::Mutex<Vec<Vec<u8>>>,
    terminal: tokio::sync::Mutex<Option<StreamState>>,
}

impl StreamClaim {
    async fn wait_terminal(&self, want: StreamState) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            if *self.terminal.lock().await == Some(want) {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "stream never reached {want:?}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

// ---- F1: fetch reaches the consumer handler; full flow over the stream ----

#[tokio::test]
async fn f1_fetch_flows_through_consumer_handler_and_stream() {
    let fixture = connect_pair(None, false).await;
    let claim = claim_stream(&fixture.session_a).await;

    let meta = timeout(fixture.session_a.fetch("file:///tmp/a.png"))
        .await
        .expect("fetch meta");
    assert_eq!(meta.size_bytes, 200_000);
    assert_eq!(meta.mime, "image/png");
    assert_eq!(meta.sha256, sha256_hex(&vec![1u8; 200_000]));

    // The requester claims the stream by matching the announced txid.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if let Some((txid, total_len)) = claim.meta.lock().await.as_ref() {
            assert_eq!(*txid, meta.txid, "stream matches fetch_ready txid");
            assert_eq!(*total_len, 200_000);
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "no stream opened");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    timeout(claim.wait_terminal(StreamState::Complete)).await;
    let got = claim.chunks.lock().await.concat();
    assert_eq!(got.len() as u64, meta.size_bytes);
    assert_eq!(sha256_hex(&got), meta.sha256);
}

// ---- F2: handler errors surface through the generic error channel ----

#[tokio::test]
async fn f2_handler_error_codes_pass_through() {
    let fixture = connect_pair(Some(Arc::new(RefuseHandler)), false).await;
    let err = timeout(fixture.session_a.fetch("file:///tmp/anything"))
        .await
        .expect_err("refused");
    match err {
        XacppError::Application { code, .. } => assert_eq!(code, error_codes::FETCH_REFUSED),
        other => panic!("unexpected error: {other:?}"),
    }
}

#[tokio::test]
async fn f2b_not_found_error_passes_through() {
    let fixture = connect_pair(None, false).await;
    let err = timeout(fixture.session_a.fetch("file:///tmp/missing.png"))
        .await
        .expect_err("not found");
    match err {
        XacppError::Application { code, .. } => assert_eq!(code, error_codes::FETCH_NOT_FOUND),
        other => panic!("unexpected error: {other:?}"),
    }
}

// ---- F5: fetch() returns metadata only — the bytes travel on the stream ----

#[tokio::test]
async fn f5_fetch_returns_metadata_not_bytes() {
    let fixture = connect_pair(None, false).await;
    let claim = claim_stream(&fixture.session_a).await;
    let meta = timeout(fixture.session_a.fetch("file:///tmp/a.png"))
        .await
        .expect("fetch meta");
    // The kernel shell carries exactly the response fields; any byte
    // delivery happens exclusively through the claimed stream.
    timeout(claim.wait_terminal(StreamState::Complete)).await;
    assert_eq!(
        claim.chunks.lock().await.concat().len() as u64,
        meta.size_bytes
    );
}

// ---- F6: unregistered streams fall back to the aggregate surface ----

#[tokio::test]
async fn f6_binary_without_stream_handler_reaches_on_binary() {
    let fixture = connect_pair(None, false).await;

    // The aggregate handler is a synchronous `Fn` — it cannot await — and
    // the payload length is sampled while the transfer is still running,
    // so the slot is a watch cell: a synchronous append, a synchronous
    // length snapshot (the value is never consumed).
    let received = tokio::sync::watch::channel(Vec::<u8>::new()).0;
    fixture
        .session_a
        .on_binary({
            let received = received.clone();
            Arc::new(move |_txid, payload| {
                received.send_modify(|buf| buf.extend_from_slice(&payload));
            })
        })
        .await;

    let meta = timeout(fixture.session_a.fetch("file:///tmp/a.png"))
        .await
        .expect("fetch meta");
    // With no stream handler registered, the transaction is delivered
    // through the aggregate on_binary surface (legacy semantics intact).
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let received_len = (*received.borrow()).len() as u64;
        if received_len == meta.size_bytes {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "aggregate surface never received the payload"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
