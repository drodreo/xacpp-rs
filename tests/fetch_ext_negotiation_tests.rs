//! fetchExt negotiation (SPEC §8.7) — architecture counterexample tests.
//!
//! The invariants asserted here (each one exists to falsify a specific wrong
//! model of the negotiation gate):
//!
//! - n1: absence on EITHER side disables fetch loudly but never the
//!   connection — negotiate, establish and unrelated commands all keep
//!   working, and the outbound fetch fails before touching the wire.
//! - n2: an unsupported declared version disables fetch without rejecting
//!   the negotiation; the declaration itself is still reported.
//! - n3: enablement follows the two declarations, not local assembly — a
//!   requester-only side (no fetch responder loaded anywhere on that end)
//!   can enable fetch and really pull from a serving peer.
//! - n4: a loaded FetchService cannot resurrect fetch while negotiation has
//!   not enabled it — the inbound gate stays shut (proven wire-level,
//!   bypassing the initiator's own outbound gate).
//! - n5: once negotiation HAS enabled fetch, the same wire-level fetch
//!   reaches the loaded responder and is answered `fetch_ready` (the gate
//!   does not over-refuse).
//! - n6: disconnect clears the negotiated fact — the session-level
//!   availability is per-connection, and a post-disconnect fetch fails
//!   `fetch_unavailable` (not `NotConnected`).
//! - n7: unified user input (SPEC §12) — one command carries mixed
//!   text/image/file parts as one ordered list; no per-part splitting, no
//!   parallel payload types, wire keys camelCase.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio::io::BufReader;

use xacpp::carrier::XacppTransport;
use xacpp::carrier::stdio::StdioTransport;
use xacpp::ext::fetch::{FetchProvider, FetchPuller, FetchService, ResolvedResource};
use xacpp::reliability::stream::BinarySink;
use xacpp::session::XacppSession;
use xacpp::session::handler::{
    EstablishDecision, EstablishHandler, NegotiateHandler, XacppSessionHandler,
};
use xacpp::session::peer::XacppPeer;
use xacpp::transfer::TransferConfig;
use xacpp::wire::capability::{Capabilities, EffectiveCapabilities};
use xacpp::wire::commands::InvokeActivityPayload;
use xacpp::wire::commands::XacppCommand;
use xacpp::wire::error::XacppError;
use xacpp::wire::events::content::ContentPart;
use xacpp::wire::fetch::error_codes;
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

fn payload_bytes() -> Vec<u8> {
    (0..2048usize)
        .map(|i| ((i * 7 + (i >> 6)) & 0xff) as u8)
        .collect()
}

fn caps_declared() -> Capabilities {
    Capabilities {
        fetch_ext: Some(1),
        ..Capabilities::default()
    }
}

fn caps_silent() -> Capabilities {
    Capabilities::default()
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

// ---- Shared fixtures ----

/// Records the effective capabilities the protocol layer computed, on
/// whichever side it is installed. This IS the responder-side backfill
/// source: `ServingEstablish` injects `effective.fetch_enabled` into the
/// session it constructs (the negotiated fact, not an assembly decision).
struct RecordNegotiate {
    effective: tokio::sync::Mutex<Option<EffectiveCapabilities>>,
}

impl RecordNegotiate {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            effective: tokio::sync::Mutex::new(None),
        })
    }

    async fn verdict(&self) -> EffectiveCapabilities {
        self.effective
            .lock()
            .await
            .clone()
            .expect("negotiate handler was not called")
    }
}

#[async_trait]
impl NegotiateHandler for RecordNegotiate {
    async fn on_negotiate(&self, effective: EffectiveCapabilities) -> Result<(), XacppError> {
        *self.effective.lock().await = Some(effective);
        Ok(())
    }
}

/// Generic session handler: answers everything with acknowledge (carries NO
/// fetch responder — `is_fetch_responder` stays default `false`).
struct NullSessionHandler;

#[async_trait]
impl XacppSessionHandler for NullSessionHandler {
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

/// Canned-bytes provider: resolves any uri to the same payload.
struct StaticProvider {
    data: Vec<u8>,
}

#[async_trait]
impl FetchProvider for StaticProvider {
    async fn resolve(&self, _uri: &str) -> Result<ResolvedResource, XacppError> {
        Ok(ResolvedResource {
            size_bytes: self.data.len() as u64,
            sha256: sha256_hex(&self.data),
            mime: "application/octet-stream".into(),
        })
    }

    async fn serve(&self, _uri: &str, sink: Arc<BinarySink>) -> Result<(), XacppError> {
        for chunk in self.data.chunks(512) {
            sink.write(chunk).await?;
        }
        Ok(())
    }
}

/// Responder establish with a loaded FetchService. The session's fetch
/// availability is injected from the RECORDED NEGOTIATION (the responder
/// side of the peer/session backfill chain) — never from the fact that a
/// provider happens to be assembled here.
struct ServingEstablish {
    provider: Arc<dyn FetchProvider>,
    record: Arc<RecordNegotiate>,
}

#[async_trait]
impl EstablishHandler for ServingEstablish {
    async fn on_establish(
        &self,
        transport: Arc<dyn XacppTransport>,
        _credentials: Option<String>,
    ) -> Result<EstablishDecision, XacppError> {
        let session = Arc::new(
            XacppSession::new(Some(TransferConfig {
                ack_batch_interval_ms: 50,
                tick_interval_ms: 20,
                ..TransferConfig::default()
            }))
            .with_identity("sess-b".into(), "creds-b".into())
            .await,
        );
        let service: Arc<dyn XacppSessionHandler> = Arc::new(FetchService::new(
            Arc::clone(&session),
            Arc::clone(&self.provider),
            // Non-fetch commands pass through to a plain acknowledge handler.
            Some(Arc::new(NullSessionHandler) as Arc<dyn XacppSessionHandler>),
        ));
        // The negotiated fact drives the session gate (both sides of the
        // same chain: initiator backfill is automatic, responder injection
        // comes from on_negotiate).
        let fetch_enabled = self.record.verdict().await.fetch_enabled;
        session.set_fetch_enabled(fetch_enabled).await;
        session.wire(transport).await?;
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

struct Pair {
    peer_a: XacppPeer,
    session_a: Arc<XacppSession>,
    record_a: Arc<RecordNegotiate>,
    /// Raw initiator-side transport: lets tests inject wire-level commands
    /// (bypassing the initiator's session-side outbound gate).
    ta: Arc<StdioTransport>,
    /// Kept alive so the responder side keeps draining its transport.
    _peer_b: XacppPeer,
}

async fn connect_pair(
    caps_a: Capabilities,
    caps_b: Capabilities,
    provider: Option<Arc<dyn FetchProvider>>,
) -> Pair {
    let (ta, tb) = stdio_duplex();
    let session_a = Arc::new(XacppSession::new(Some(TransferConfig {
        ack_batch_interval_ms: 50,
        tick_interval_ms: 20,
        ..TransferConfig::default()
    })));
    let record_a = RecordNegotiate::new();
    let record_b = RecordNegotiate::new();
    let establish_b: Arc<dyn EstablishHandler> = match provider {
        Some(p) => Arc::new(ServingEstablish {
            provider: p,
            record: Arc::clone(&record_b),
        }),
        None => Arc::new(NoEstablish),
    };
    let peer_a = XacppPeer::new(
        caps_a,
        Arc::clone(&ta) as Arc<dyn XacppTransport>,
        Some(Arc::clone(&session_a)),
        Arc::clone(&record_a) as Arc<dyn NegotiateHandler>,
        Arc::new(NoEstablish),
    );
    let peer_b = XacppPeer::new(
        caps_b,
        tb as Arc<dyn XacppTransport>,
        None,
        Arc::clone(&record_b) as Arc<dyn NegotiateHandler>,
        establish_b,
    );
    peer_a.connect().await.unwrap();
    peer_b.connect().await.unwrap();
    Pair {
        peer_a,
        session_a,
        record_a,
        ta,
        _peer_b: peer_b,
    }
}

async fn establish_from_initiator(pair: &Pair) {
    pair.peer_a
        .establish(None, Arc::new(NullSessionHandler), |_| Ok(()))
        .await
        .unwrap();
}

// ---- n1: absence on either side — fetch off, connection alive ----

#[tokio::test]
async fn n1_undeclared_disables_fetch_but_not_the_connection() {
    let combos = [
        (caps_silent(), caps_declared()),
        (caps_declared(), caps_silent()),
        (caps_silent(), caps_silent()),
    ];
    for (idx, (a_caps, b_caps)) in combos.into_iter().enumerate() {
        let pair = connect_pair(
            a_caps,
            b_caps,
            Some(Arc::new(StaticProvider {
                data: payload_bytes(),
            })),
        )
        .await;
        // Negotiation itself is NOT blocked.
        pair.peer_a.negotiate().await.unwrap();
        // Establish keeps working.
        establish_from_initiator(&pair).await;
        // The verdict is "disabled" in every undeclared combination.
        assert!(
            !pair.record_a.verdict().await.fetch_enabled,
            "combo {idx}: fetch must stay disabled"
        );
        // Unrelated commands keep flowing.
        let resp = pair
            .session_a
            .request_command(XacppCommand::generic("ping", json!({})))
            .await
            .unwrap();
        match resp {
            XacppResponse::Generic { name, .. } => assert_eq!(name, "acknowledge"),
            other => panic!("combo {idx}: non-fetch command must pass, got {other:?}"),
        }
        // Outbound fetch fails loudly BEFORE reaching the wire.
        let err = pair.session_a.fetch("res://x").await.unwrap_err();
        assert_eq!(err.code(), error_codes::FETCH_UNAVAILABLE);
        pair.peer_a.disconnect().await.unwrap();
    }
}

// ---- n2: unsupported declared version — disabled, negotiation alive ----

#[tokio::test]
async fn n2_unsupported_version_disables_without_rejecting_negotiation() {
    let mut remote_v2 = caps_silent();
    remote_v2.fetch_ext = Some(2);
    let pair = connect_pair(
        caps_declared(),
        remote_v2,
        Some(Arc::new(StaticProvider {
            data: payload_bytes(),
        })),
    )
    .await;
    // Negotiate succeeds — a version mismatch never refuses the connection.
    pair.peer_a.negotiate().await.unwrap();
    establish_from_initiator(&pair).await;
    let verdict = pair.record_a.verdict().await;
    assert!(!verdict.fetch_enabled);
    assert_eq!(verdict.remote_fetch_ext, Some(2));
    // Fetch is loudly unavailable; the session is otherwise healthy.
    let err = pair.session_a.fetch("res://x").await.unwrap_err();
    assert_eq!(err.code(), error_codes::FETCH_UNAVAILABLE);
    pair.peer_a.disconnect().await.unwrap();
}

// ---- n3: requester-only — declarations alone enable real fetching ----

#[tokio::test]
async fn n3_requester_only_enables_real_fetch_by_declaration() {
    let data = payload_bytes();
    let pair = connect_pair(
        caps_declared(),
        caps_declared(),
        Some(Arc::new(StaticProvider { data: data.clone() })),
    )
    .await;
    pair.peer_a.negotiate().await.unwrap();
    // The initiator establishes with a handler that has NO fetch responder
    // loaded — enablement must not care.
    establish_from_initiator(&pair).await;
    assert!(pair.record_a.verdict().await.fetch_enabled);
    assert!(pair.session_a.fetch_enabled().await);
    // And the fetch is real end-to-end: bytes served, sha verified.
    let puller = FetchPuller::new(Arc::clone(&pair.session_a)).await;
    let (collected, meta) = timeout(puller.pull_to_vec("res://a")).await.unwrap();
    assert_eq!(collected, data);
    assert_eq!(meta.size_bytes as usize, data.len());
    pair.peer_a.disconnect().await.unwrap();
}

// ---- n4: loaded responder cannot resurrect a disabled negotiation ----

#[tokio::test]
async fn n4_loaded_responder_still_refuses_while_not_enabled() {
    let pair = connect_pair(
        caps_silent(),
        caps_silent(),
        Some(Arc::new(StaticProvider {
            data: payload_bytes(),
        })),
    )
    .await;
    pair.peer_a.negotiate().await.unwrap();
    establish_from_initiator(&pair).await;
    // Outbound: the initiator's own gate answers before the wire.
    let err = pair.session_a.fetch("res://x").await.unwrap_err();
    assert_eq!(err.code(), error_codes::FETCH_UNAVAILABLE);
    // Inbound: inject a fetch command straight down the wire (bypassing the
    // initiator's session gate) — the responder HAS a FetchService loaded,
    // yet the un-enabled negotiation keeps the inbound gate shut.
    let resp = pair
        .ta
        .send(
            Some("sess-b"),
            XacppRequest::Command(XacppCommand::Fetch {
                uri: "res://x".into(),
            }),
        )
        .await
        .unwrap();
    match resp {
        XacppResponse::Error { code, .. } => {
            assert_eq!(code, error_codes::FETCH_UNAVAILABLE)
        }
        other => panic!("inbound fetch must stay gated, got {other:?}"),
    }
    pair.peer_a.disconnect().await.unwrap();
}

// ---- n5: enabled negotiation lets the same wire-level fetch through ----

#[tokio::test]
async fn n5_enabled_negotiation_lets_loaded_responder_answer() {
    let pair = connect_pair(
        caps_declared(),
        caps_declared(),
        Some(Arc::new(StaticProvider {
            data: payload_bytes(),
        })),
    )
    .await;
    pair.peer_a.negotiate().await.unwrap();
    establish_from_initiator(&pair).await;
    // The same wire-level injection now reaches FetchService and is
    // answered with fetch_ready — the gate refuses only what negotiation
    // did not enable, not fetch itself.
    let resp = pair
        .ta
        .send(
            Some("sess-b"),
            XacppRequest::Command(XacppCommand::Fetch {
                uri: "res://x".into(),
            }),
        )
        .await
        .unwrap();
    match resp {
        XacppResponse::FetchReady { .. } => {}
        other => panic!("enabled negotiation must serve fetch_ready, got {other:?}"),
    }
    pair.peer_a.disconnect().await.unwrap();
}

// ---- n6: disconnect clears the negotiated fact ----

#[tokio::test]
async fn n6_disconnect_clears_the_negotiated_fact() {
    let pair = connect_pair(
        caps_declared(),
        caps_declared(),
        Some(Arc::new(StaticProvider {
            data: payload_bytes(),
        })),
    )
    .await;
    pair.peer_a.negotiate().await.unwrap();
    establish_from_initiator(&pair).await;
    assert!(pair.session_a.fetch_enabled().await);
    pair.peer_a.disconnect().await.unwrap();
    // Session-level availability dies with the connection: a fetch now
    // answers fetch_unavailable (the negotiated-fact shape), not a
    // transport error.
    assert!(!pair.session_a.fetch_enabled().await);
    let err = pair.session_a.fetch("res://x").await.unwrap_err();
    assert_eq!(err.code(), error_codes::FETCH_UNAVAILABLE);
}

// ---- n7: unified input — one command, mixed parts, order preserved ----

#[tokio::test]
async fn n7_unified_input_payload_mixed_parts_order_preserved() {
    let wire = json!({
        "activity": "act-1",
        "messages": [
            { "type": "text", "text": "look at this" },
            { "type": "image", "source": {
                "remoteUrl": "", "localUri": "/tmp/a.png",
                "mimeType": "image/png", "requireOrganized": false,
                "sizeBytes": 3, "sha256": "aa"
            } },
            { "type": "file", "source": {
                "remoteUrl": "", "localUri": "/tmp/a.bin",
                "mimeType": "application/octet-stream", "requireOrganized": false,
                "sizeBytes": 3, "sha256": "bb"
            } }
        ]
    });
    // ONE payload type: InvokeActivityPayload (steer/push_task reuse it —
    // there is no SteerActivityPayload/PushTaskPayload to alias).
    let payload: InvokeActivityPayload = serde_json::from_value(wire).unwrap();
    assert_eq!(payload.activity, "act-1");
    assert_eq!(payload.messages.len(), 3);
    assert!(matches!(&payload.messages[0], ContentPart::Text(_)));
    assert!(matches!(&payload.messages[1], ContentPart::Image(_)));
    assert!(matches!(&payload.messages[2], ContentPart::File(_)));
    // Round-trip keeps the complete ordered list intact (no reorder, no
    // split, camelCase wire keys).
    let back = serde_json::to_value(&payload).unwrap();
    let types: Vec<&str> = back["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["type"].as_str().unwrap())
        .collect();
    assert_eq!(types, vec!["text", "image", "file"]);
    assert_eq!(back["activity"], "act-1");
    let reparsed: InvokeActivityPayload = serde_json::from_value(back).unwrap();
    assert_eq!(reparsed.activity, payload.activity);
    assert_eq!(reparsed.messages, payload.messages);
}
