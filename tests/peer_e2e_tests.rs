//! Transport + Peer + Session end-to-end tests.
//!
//! Core scenarios covered:
//! 1. send: send request, handler callback processes and auto-replies (acknowledge / business data / Error)
//! 2. routing: session_id routes correctly to corresponding Session handler
//! 3. Establish: handshake flow between initiator and responder
//! 4. Disconnect detection
//! 5. Interaction Command lifecycle (action_request / question / sensitive_info)

use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use tokio::io::BufReader;
use tokio::sync::mpsc;

use xacpp::carrier::XacppTransport;
use xacpp::carrier::stdio::StdioTransport;
use xacpp::session::XacppSession;
use xacpp::session::handler::{
    EstablishDecision, EstablishHandler, NegotiateHandler, XacppSessionHandler,
};
use xacpp::session::peer::{PeerState, XacppPeer};
use xacpp::wire::activity_ref::ActivityRef;
use xacpp::wire::capability::{Capabilities, EffectiveCapabilities};
use xacpp::wire::commands::XacppCommand;
use xacpp::wire::error::XacppError;
use xacpp::wire::events::content::{ContentPart, TextPart};
use xacpp::wire::events::interaction::{
    ActionRequestPayload, ActionResponse, QuestionPayload, QuestionResponse, SensitiveInfoItem,
    SensitiveInfoOperation, SensitiveInfoOperationPayload, SensitiveInfoOperationResponse,
    SensitiveInfoResult, SensitiveInfoType, action_request_command, question_command,
    sensitive_info_command,
};
use xacpp::wire::events::payload::AlertLevel;
use xacpp::wire::events::{XacppActivityEvent, XacppEvent};
use xacpp::wire::message::{XacppRequest, XacppResponse};

// ---- Test Handler Implementations ----

/// Generic Session handler: both Command and Event return acknowledge.
struct TestSessionHandler;

#[async_trait::async_trait]
impl XacppSessionHandler for TestSessionHandler {
    async fn on_command(&self, _command: XacppCommand) -> Result<XacppResponse, XacppError> {
        Ok(XacppResponse::acknowledge())
    }

    async fn on_event(&self, _event: XacppActivityEvent) -> Result<XacppResponse, XacppError> {
        Ok(XacppResponse::acknowledge())
    }
}

/// Accepts any negotiation (records effective capabilities).
struct AcceptNegotiateHandler {
    received: tokio::sync::Mutex<Option<EffectiveCapabilities>>,
}

impl AcceptNegotiateHandler {
    fn new() -> Self {
        Self {
            received: tokio::sync::Mutex::new(None),
        }
    }

    async fn get_received(&self) -> Option<EffectiveCapabilities> {
        self.received.lock().await.clone()
    }
}

#[async_trait::async_trait]
impl NegotiateHandler for AcceptNegotiateHandler {
    async fn on_negotiate(&self, effective: EffectiveCapabilities) -> Result<(), XacppError> {
        *self.received.lock().await = Some(effective);
        Ok(())
    }
}

/// Rejects all negotiations.
struct RejectNegotiateHandler;

#[async_trait::async_trait]
impl NegotiateHandler for RejectNegotiateHandler {
    async fn on_negotiate(&self, _effective: EffectiveCapabilities) -> Result<(), XacppError> {
        Err(XacppError::Internal("negotiation rejected".into()))
    }
}

/// Auto-approves Establish and returns TestSessionHandler.
struct AutoApproveEstablishHandler;

#[async_trait::async_trait]
impl EstablishHandler for AutoApproveEstablishHandler {
    async fn on_establish(
        &self,
        _transport: Arc<dyn XacppTransport>,
        _credentials: Option<String>,
    ) -> Result<EstablishDecision, XacppError> {
        Ok(EstablishDecision::Established {
            session_id: "auto-sid".into(),
            handler: Arc::new(TestSessionHandler),
            credentials: "auto-creds".into(),
        })
    }

    async fn on_establish_confirm(
        &self,
        _transport: Arc<dyn XacppTransport>,
    ) -> Result<(String, Arc<dyn XacppSessionHandler>, String), XacppError> {
        Ok((
            "auto-sid".into(),
            Arc::new(TestSessionHandler),
            "issued-creds".into(),
        ))
    }
}

/// Challenge-aware handler: on_establish returns ChallengeRequired, on_establish_confirm returns (sid, handler, creds).
struct ChallengeEstablishHandler;

#[async_trait::async_trait]
impl EstablishHandler for ChallengeEstablishHandler {
    async fn on_establish(
        &self,
        _transport: Arc<dyn XacppTransport>,
        _credentials: Option<String>,
    ) -> Result<EstablishDecision, XacppError> {
        Ok(EstablishDecision::ChallengeRequired {
            challenge: "test-challenge".into(),
        })
    }

    async fn on_establish_confirm(
        &self,
        _transport: Arc<dyn XacppTransport>,
    ) -> Result<(String, Arc<dyn XacppSessionHandler>, String), XacppError> {
        Ok((
            "challenge-sid".into(),
            Arc::new(TestSessionHandler),
            "issued-creds".into(),
        ))
    }
}

/// Session handler with ID: identifies itself through Generic response.
struct IdentifiedHandler {
    id: String,
}

#[async_trait::async_trait]
impl XacppSessionHandler for IdentifiedHandler {
    async fn on_command(&self, _command: XacppCommand) -> Result<XacppResponse, XacppError> {
        Ok(XacppResponse::generic(
            "handler_id",
            json!({ "id": self.id.clone() }),
        ))
    }

    async fn on_event(&self, _event: XacppActivityEvent) -> Result<XacppResponse, XacppError> {
        Ok(XacppResponse::generic(
            "handler_id",
            json!({ "id": self.id.clone() }),
        ))
    }
}

/// EstablishHandler that allocates IdentifiedHandler by sequence number.
struct SequencedEstablishHandler {
    counter: std::sync::atomic::AtomicU64,
}

impl SequencedEstablishHandler {
    fn new() -> Self {
        Self {
            counter: std::sync::atomic::AtomicU64::new(1),
        }
    }
}

#[async_trait::async_trait]
impl EstablishHandler for SequencedEstablishHandler {
    async fn on_establish(
        &self,
        _transport: Arc<dyn XacppTransport>,
        _credentials: Option<String>,
    ) -> Result<EstablishDecision, XacppError> {
        let n = self
            .counter
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let sid = format!("handler-{n}");
        Ok(EstablishDecision::Established {
            session_id: sid.clone(),
            handler: Arc::new(IdentifiedHandler { id: sid }),
            credentials: "auto-creds".into(),
        })
    }

    async fn on_establish_confirm(
        &self,
        _transport: Arc<dyn XacppTransport>,
    ) -> Result<(String, Arc<dyn XacppSessionHandler>, String), XacppError> {
        let n = self
            .counter
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let sid = format!("handler-{n}");
        Ok((
            sid.clone(),
            Arc::new(IdentifiedHandler { id: sid }),
            "issued-creds".into(),
        ))
    }
}

// ---- Helper Functions ----

/// Creates a pair of Transport connected via memory duplex (not connected).
fn duplex_pair() -> (Arc<dyn XacppTransport>, Arc<dyn XacppTransport>) {
    let (stream_a, stream_b) = tokio::io::duplex(4096);
    let (a_read, a_write) = tokio::io::split(stream_a);
    let (b_read, b_write) = tokio::io::split(stream_b);

    let transport_a: Arc<dyn XacppTransport> = Arc::new(StdioTransport::new(
        Box::pin(a_write),
        Box::pin(BufReader::new(a_read)),
    ));
    let transport_b: Arc<dyn XacppTransport> = Arc::new(StdioTransport::new(
        Box::pin(b_write),
        Box::pin(BufReader::new(b_read)),
    ));

    (transport_a, transport_b)
}

/// Creates a pair of connected Peers (B side auto-approves Establish, accepts negotiation).
async fn connected_peers() -> (XacppPeer, XacppPeer) {
    let (transport_a, transport_b) = duplex_pair();

    let caps_agent = Capabilities {
        commands: vec![
            serde_json::json!({"name": "new_activity"}),
            serde_json::json!({"name": "last_activity"}),
            serde_json::json!({"name": "list_activity"}),
            serde_json::json!({"name": "switch_activity"}),
            serde_json::json!({"name": "invoke_activity"}),
            serde_json::json!({"name": "compact_activity"}),
            serde_json::json!({"name": "cancel_activity"}),
            serde_json::json!({"name": "message"}),
            serde_json::json!({"name": "action_request"}),
            serde_json::json!({"name": "question"}),
            serde_json::json!({"name": "sensitive_info_operation"}),
        ],
        produce_events: vec![
            serde_json::json!({"name": "content_delta"}),
            serde_json::json!({"name": "content_part"}),
            serde_json::json!({"name": "think"}),
            serde_json::json!({"name": "info"}),
            serde_json::json!({"name": "warn"}),
            serde_json::json!({"name": "error"}),
            serde_json::json!({"name": "notify"}),
            serde_json::json!({"name": "activity_start"}),
            serde_json::json!({"name": "activity_updates"}),
            serde_json::json!({"name": "activity_done"}),
            serde_json::json!({"name": "activity_aborted"}),
            serde_json::json!({"name": "tool_use"}),
            serde_json::json!({"name": "tool_result"}),
            serde_json::json!({"name": "security_alert"}),
            serde_json::json!({"name": "upload"}),
            serde_json::json!({"name": "pair_complete"}),
            serde_json::json!({"name": "complete"}),
        ],
        accept_events: Vec::new(),
        fetch_ext: None,
    };
    let caps_bot = Capabilities {
        commands: Vec::new(),
        produce_events: Vec::new(),
        accept_events: Vec::new(),
        fetch_ext: None,
    };

    let peer_a = XacppPeer::new(
        caps_agent,
        transport_a,
        Some(Arc::new(XacppSession::new(None))),
        Arc::new(AcceptNegotiateHandler::new()),
        Arc::new(AutoApproveEstablishHandler),
    );
    let peer_b = XacppPeer::new(
        caps_bot,
        transport_b,
        None,
        Arc::new(AcceptNegotiateHandler::new()),
        Arc::new(AutoApproveEstablishHandler),
    );
    peer_a.connect().await.unwrap();
    peer_b.connect().await.unwrap();
    (peer_a, peer_b)
}

/// Creates a pair of connected + negotiated Peers ready for Establish.
async fn negotiated_peers() -> (XacppPeer, XacppPeer) {
    let (peer_a, peer_b) = connected_peers().await;
    peer_a.negotiate().await.unwrap();
    (peer_a, peer_b)
}

/// Timeout wrapper (5s).
async fn timeout<F, T>(future: F) -> T
where
    F: std::future::Future<Output = T>,
{
    tokio::time::timeout(Duration::from_secs(5), future)
        .await
        .expect("timeout")
}

// ---- Transport Layer Tests ----

#[tokio::test]
async fn test_transport_send_establish() {
    let (transport_a, transport_b) = duplex_pair();

    transport_b
        .on_request(Arc::new(|session_id, payload| {
            Box::pin(async move {
                match (session_id, payload) {
                    (None, XacppRequest::Command(XacppCommand::Establish { .. })) => {
                        Ok(XacppResponse::Established {
                            session_id: "sid-1".into(),
                            credentials: "auto-creds".into(),
                        })
                    }
                    _ => Ok(XacppResponse::acknowledge()),
                }
            })
        }))
        .unwrap();

    transport_a.connect().await.unwrap();
    transport_b.connect().await.unwrap();

    let response = timeout(transport_a.send(
        None,
        XacppRequest::Command(XacppCommand::Establish { credentials: None }),
    ))
    .await
    .unwrap();

    match response {
        XacppResponse::Established { session_id, .. } => assert_eq!(session_id, "sid-1"),
        other => panic!("expected Established response, got: {other:?}"),
    }
}

#[tokio::test]
async fn test_transport_send_event_acknowledge() {
    let (transport_a, transport_b) = duplex_pair();

    let (notify_tx, mut notify_rx) = mpsc::channel::<XacppActivityEvent>(10);
    transport_b
        .on_request(Arc::new(move |_session_id, payload| {
            let tx = notify_tx.clone();
            Box::pin(async move {
                if let XacppRequest::Event(evt) = payload {
                    let _ = tx.send(evt).await;
                }
                Ok(XacppResponse::acknowledge())
            })
        }))
        .unwrap();

    transport_a.connect().await.unwrap();
    transport_b.connect().await.unwrap();

    let response = timeout(transport_a.send(
        Some("s1"),
        XacppRequest::Event(XacppActivityEvent {
            activity: ActivityRef::new("test-act"),
            event: XacppEvent::new("think", json!({ "content": "hello" })),
        }),
    ))
    .await
    .unwrap();

    assert!(matches!(response, XacppResponse::Generic { .. }));

    let evt = timeout(notify_rx.recv()).await.unwrap();
    assert_eq!(evt.event.name, "think");
    assert_eq!(evt.event.data["content"], "hello");
}

#[tokio::test]
async fn test_transport_handler_error() {
    let (transport_a, transport_b) = duplex_pair();

    transport_b
        .on_request(Arc::new(|_session_id, _payload| {
            Box::pin(async { Err(XacppError::Internal("something went wrong".into())) })
        }))
        .unwrap();

    transport_a.connect().await.unwrap();
    transport_b.connect().await.unwrap();

    let response = timeout(transport_a.send(
        None,
        XacppRequest::Command(XacppCommand::Establish { credentials: None }),
    ))
    .await
    .unwrap();

    match response {
        XacppResponse::Error { code, message } => {
            assert_eq!(code, "internal_error");
            assert_eq!(message, "internal error: something went wrong");
        }
        other => panic!("expected Error response, got: {other:?}"),
    }
}

#[tokio::test]
async fn test_transport_no_handler_returns_error() {
    // B does not register handler, but connects
    let (transport_a, transport_b) = duplex_pair();
    transport_a.connect().await.unwrap();
    transport_b.connect().await.unwrap();

    let response = timeout(transport_a.send(
        None,
        XacppRequest::Command(XacppCommand::Establish { credentials: None }),
    ))
    .await
    .unwrap();

    match response {
        XacppResponse::Error { code, .. } => {
            assert_eq!(code, "no_handler");
        }
        other => panic!("expected Error response, got: {other:?}"),
    }
}

#[tokio::test]
async fn test_transport_bidirectional() {
    let (transport_a, transport_b) = duplex_pair();

    transport_a
        .on_request(Arc::new(|_session_id, _payload| {
            Box::pin(async move { Ok(XacppResponse::generic("from", json!({ "side": "a" }))) })
        }))
        .unwrap();

    transport_b
        .on_request(Arc::new(|_session_id, _payload| {
            Box::pin(async move { Ok(XacppResponse::generic("from", json!({ "side": "b" }))) })
        }))
        .unwrap();

    transport_a.connect().await.unwrap();
    transport_b.connect().await.unwrap();

    // A → B
    let resp_ab = timeout(transport_a.send(
        None,
        XacppRequest::Command(XacppCommand::Establish { credentials: None }),
    ))
    .await
    .unwrap();
    match resp_ab {
        XacppResponse::Generic { name: _, data } => {
            assert_eq!(data["side"], "b");
        }
        other => panic!("unexpected: {other:?}"),
    }

    // B → A
    let resp_ba = timeout(transport_b.send(
        None,
        XacppRequest::Command(XacppCommand::Establish { credentials: None }),
    ))
    .await
    .unwrap();
    match resp_ba {
        XacppResponse::Generic { name: _, data } => {
            assert_eq!(data["side"], "a");
        }
        other => panic!("unexpected: {other:?}"),
    }
}

// ---- Peer Layer Tests ----

#[tokio::test]
async fn test_peer_connect_state() {
    let (peer_a, _peer_b) = connected_peers().await;
    assert_eq!(peer_a.state().await, PeerState::Connected);
}

#[tokio::test]
async fn test_peer_disconnect_state() {
    let (peer_a, _peer_b) = connected_peers().await;
    peer_a.disconnect().await.unwrap();
    assert_eq!(peer_a.state().await, PeerState::Disconnected);
}

#[tokio::test]
async fn test_peer_establish() {
    let (peer_a, _peer_b) = negotiated_peers().await;

    let handler: Arc<dyn XacppSessionHandler> = Arc::new(TestSessionHandler);
    let session = timeout(peer_a.establish(None, handler, |_| Ok(())))
        .await
        .unwrap();

    assert!(!session.session_id().await.is_empty());
    assert_eq!(session.credentials().await, "auto-creds");
}

#[tokio::test]
async fn test_session_request_command() {
    let (peer_a, _peer_b) = negotiated_peers().await;

    let handler: Arc<dyn XacppSessionHandler> = Arc::new(TestSessionHandler);
    let session = timeout(peer_a.establish(None, handler, |_| Ok(())))
        .await
        .unwrap();

    let response =
        timeout(session.request_command(XacppCommand::generic("new_activity", json!({}))))
            .await
            .unwrap();

    match response {
        XacppResponse::Generic { name, .. } => assert_eq!(name, "acknowledge"),
        other => panic!("expected acknowledge, got: {other:?}"),
    }
}

// ============================================================================
// Negotiate Phase Tests
// ============================================================================

#[tokio::test]
async fn test_negotiate_full_flow() {
    let (transport_a, transport_b) = duplex_pair();

    let caps_a = Capabilities {
        commands: vec![
            serde_json::json!({"name": "new_activity"}),
            serde_json::json!({"name": "switch_activity"}),
        ],
        produce_events: vec![
            serde_json::json!({"name": "content_delta"}),
            serde_json::json!({"name": "activity_done"}),
        ],
        accept_events: Vec::new(),
        fetch_ext: None,
    };
    let caps_b = Capabilities {
        commands: Vec::new(),
        produce_events: Vec::new(),
        accept_events: Vec::new(),
        fetch_ext: None,
    };

    let negotiate_a = Arc::new(AcceptNegotiateHandler::new());
    let negotiate_b = Arc::new(AcceptNegotiateHandler::new());
    let negotiate_a_clone = negotiate_a.clone();
    let negotiate_b_clone = negotiate_b.clone();

    let peer_a = XacppPeer::new(
        caps_a.clone(),
        transport_a,
        Some(Arc::new(XacppSession::new(None))),
        negotiate_a,
        Arc::new(AutoApproveEstablishHandler),
    );
    let peer_b = XacppPeer::new(
        caps_b.clone(),
        transport_b,
        None,
        negotiate_b,
        Arc::new(AutoApproveEstablishHandler),
    );
    peer_a.connect().await.unwrap();
    peer_b.connect().await.unwrap();

    // Initiator sends negotiate
    timeout(peer_a.negotiate()).await.unwrap();

    // Verify state
    assert_eq!(peer_a.state().await, PeerState::Negotiated);

    // Initiator received responder's capabilities (empty bot)
    let remote_caps_a = peer_a.remote_capabilities().await;
    assert!(remote_caps_a.commands.is_empty());
    assert!(remote_caps_a.produce_events.is_empty());

    // Responder's handler received initiator's effective capabilities
    let received_by_b = negotiate_b_clone.get_received().await.unwrap();
    assert_eq!(received_by_b.remote_commands.len(), 2);
    assert_eq!(received_by_b.remote_commands[0]["name"], "new_activity");
    assert_eq!(received_by_b.remote_commands[1]["name"], "switch_activity");
    // responder 端 emit_events 是空的（bot 没有 produce_events）
    assert!(received_by_b.emit_events.is_empty());

    // Initiator's handler received responder's effective capabilities (empty bot)
    let received_by_a = negotiate_a_clone.get_received().await.unwrap();
    assert!(received_by_a.remote_commands.is_empty());
    assert!(received_by_a.emit_events.is_empty());

    // Establish should now succeed
    let session = timeout(peer_a.establish(None, Arc::new(TestSessionHandler), |_| Ok(())))
        .await
        .unwrap();
    assert!(!session.session_id().await.is_empty());
}

#[tokio::test]
async fn test_negotiate_responder_rejects() {
    let (transport_a, transport_b) = duplex_pair();

    let caps_a = Capabilities {
        commands: vec![serde_json::json!({"name": "new_activity"})],
        produce_events: vec![serde_json::json!({"name": "content_delta"})],
        accept_events: Vec::new(),
        fetch_ext: None,
    };
    let peer_a = XacppPeer::new(
        caps_a,
        transport_a,
        Some(Arc::new(XacppSession::new(None))),
        Arc::new(AcceptNegotiateHandler::new()),
        Arc::new(AutoApproveEstablishHandler),
    );
    let peer_b = XacppPeer::new(
        Capabilities {
            commands: Vec::new(),
            produce_events: Vec::new(),
            accept_events: Vec::new(),
            fetch_ext: None,
        },
        transport_b,
        None,
        Arc::new(RejectNegotiateHandler),
        Arc::new(AutoApproveEstablishHandler),
    );
    peer_a.connect().await.unwrap();
    peer_b.connect().await.unwrap();

    // Negotiate should fail because responder rejects
    let result = timeout(peer_a.negotiate()).await;
    assert!(
        result.is_err(),
        "negotiate should fail when responder rejects"
    );

    // State should remain Connected, not Negotiated
    assert_eq!(peer_a.state().await, PeerState::Connected);

    // Establish should fail because state is not Negotiated
    let establish_result =
        timeout(peer_a.establish(None, Arc::new(TestSessionHandler), |_| Ok(()))).await;
    assert!(
        establish_result.is_err(),
        "establish should fail without negotiation"
    );
}

#[tokio::test]
async fn test_negotiate_initiator_rejects() {
    let (transport_a, transport_b) = duplex_pair();

    let peer_a = XacppPeer::new(
        Capabilities {
            commands: Vec::new(),
            produce_events: Vec::new(),
            accept_events: Vec::new(),
            fetch_ext: None,
        },
        transport_a,
        Some(Arc::new(XacppSession::new(None))),
        Arc::new(RejectNegotiateHandler),
        Arc::new(AutoApproveEstablishHandler),
    );
    let caps_b = Capabilities {
        commands: vec![serde_json::json!({"name": "new_activity"})],
        produce_events: vec![serde_json::json!({"name": "content_delta"})],
        accept_events: Vec::new(),
        fetch_ext: None,
    };
    let peer_b = XacppPeer::new(
        caps_b,
        transport_b,
        None,
        Arc::new(AcceptNegotiateHandler::new()),
        Arc::new(AutoApproveEstablishHandler),
    );
    peer_a.connect().await.unwrap();
    peer_b.connect().await.unwrap();

    // Negotiate should fail because initiator's handler rejects responder's capabilities
    let result = timeout(peer_a.negotiate()).await;
    assert!(
        result.is_err(),
        "negotiate should fail when initiator rejects"
    );

    // State should remain Connected
    assert_eq!(peer_a.state().await, PeerState::Connected);
}

#[tokio::test]
async fn test_establish_without_negotiate_fails() {
    let (peer_a, _peer_b) = connected_peers().await;

    // Skip negotiate, try establish directly
    let result = timeout(peer_a.establish(None, Arc::new(TestSessionHandler), |_| Ok(()))).await;
    assert!(result.is_err(), "establish should fail without negotiate");
}

#[tokio::test]
async fn test_negotiate_capabilities_preserved() {
    let (transport_a, transport_b) = duplex_pair();

    let caps_a = Capabilities {
        commands: vec![
            serde_json::json!({"name": "new_activity", "version": "1.0"}),
            serde_json::json!({"name": "cancel_activity"}),
        ],
        produce_events: vec![serde_json::json!({"name": "content_delta"})],
        accept_events: Vec::new(),
        fetch_ext: None,
    };
    let caps_b = Capabilities {
        commands: Vec::new(),
        produce_events: vec![
            serde_json::json!({"name": "action_request", "version": "2.0"}),
            serde_json::json!({"name": "question"}),
        ],
        accept_events: Vec::new(),
        fetch_ext: None,
    };

    let peer_a = XacppPeer::new(
        caps_a,
        transport_a,
        Some(Arc::new(XacppSession::new(None))),
        Arc::new(AcceptNegotiateHandler::new()),
        Arc::new(AutoApproveEstablishHandler),
    );
    let peer_b = XacppPeer::new(
        caps_b,
        transport_b,
        None,
        Arc::new(AcceptNegotiateHandler::new()),
        Arc::new(AutoApproveEstablishHandler),
    );
    peer_a.connect().await.unwrap();
    peer_b.connect().await.unwrap();

    timeout(peer_a.negotiate()).await.unwrap();

    // Verify initiator has full responder capabilities
    let remote = peer_a.remote_capabilities().await;
    assert!(remote.commands.is_empty());
    assert_eq!(remote.produce_events.len(), 2);
    assert_eq!(remote.produce_events[0]["name"], "action_request");
    assert_eq!(remote.produce_events[0]["version"], "2.0");
    assert_eq!(remote.produce_events[1]["name"], "question");
}

// ============================================================================
// Emit Events Intersection Tests
// ============================================================================

#[tokio::test]
async fn test_emit_events_intersection() {
    // caps_a: produce [content_delta, think], accept []
    // caps_b: produce [], accept [content_delta, info]
    // After negotiation:
    //   - peer_a.emit_events = [content_delta] (think filtered out, not in b.accept)
    //   - peer_b.emit_events = [] (b produces nothing)
    let (transport_a, transport_b) = duplex_pair();

    let caps_a = Capabilities {
        commands: vec![],
        produce_events: vec![
            serde_json::json!({ "name": "content_delta" }),
            serde_json::json!({ "name": "think" }),
        ],
        accept_events: Vec::new(),
        fetch_ext: None,
    };
    let caps_b = Capabilities {
        commands: vec![],
        produce_events: Vec::new(),
        accept_events: vec![
            serde_json::json!({ "name": "content_delta" }),
            serde_json::json!({ "name": "info" }),
        ],
        fetch_ext: None,
    };

    let negotiate_a = Arc::new(AcceptNegotiateHandler::new());
    let negotiate_b = Arc::new(AcceptNegotiateHandler::new());
    let negotiate_a_clone = negotiate_a.clone();
    let negotiate_b_clone = negotiate_b.clone();

    let peer_a = XacppPeer::new(
        caps_a,
        transport_a,
        Some(Arc::new(XacppSession::new(None))),
        negotiate_a,
        Arc::new(AutoApproveEstablishHandler),
    );
    let peer_b = XacppPeer::new(
        caps_b,
        transport_b,
        None,
        negotiate_b,
        Arc::new(AutoApproveEstablishHandler),
    );
    peer_a.connect().await.unwrap();
    peer_b.connect().await.unwrap();

    timeout(peer_a.negotiate()).await.unwrap();

    // peer_a receives b's effective capabilities (intersection)
    let received_by_a = negotiate_a_clone.get_received().await.unwrap();
    assert_eq!(received_by_a.emit_events, vec!["content_delta"]);
    // think was filtered out: not in caps_b.accept_events

    // peer_b receives a's effective capabilities (intersection)
    let received_by_b = negotiate_b_clone.get_received().await.unwrap();
    assert_eq!(received_by_b.emit_events, Vec::<String>::new());
    // caps_b produces nothing, so emit_events is empty
}

#[tokio::test]
async fn test_request_event_rejected_by_emit_events() {
    // caps_a: produce [content_delta], caps_b: accept [content_delta]
    // After negotiation, emit_events = [content_delta]
    // Sending "think" event should be rejected (not in emit_events)
    let (transport_a, transport_b) = duplex_pair();

    let caps_a = Capabilities {
        commands: vec![],
        produce_events: vec![serde_json::json!({ "name": "content_delta" })],
        accept_events: Vec::new(),
        fetch_ext: None,
    };
    let caps_b = Capabilities {
        commands: vec![],
        produce_events: Vec::new(),
        accept_events: vec![serde_json::json!({ "name": "content_delta" })],
        fetch_ext: None,
    };

    let peer_a = XacppPeer::new(
        caps_a,
        transport_a,
        Some(Arc::new(XacppSession::new(None))),
        Arc::new(AcceptNegotiateHandler::new()),
        Arc::new(AutoApproveEstablishHandler),
    );
    let peer_b = XacppPeer::new(
        caps_b,
        transport_b,
        None,
        Arc::new(AcceptNegotiateHandler::new()),
        Arc::new(AutoApproveEstablishHandler),
    );
    peer_a.connect().await.unwrap();
    peer_b.connect().await.unwrap();

    timeout(peer_a.negotiate()).await.unwrap();

    // Establish: the initiator session gets the negotiated emit_events
    // snapshot backfilled ([content_delta]).
    let handler: Arc<dyn XacppSessionHandler> = Arc::new(TestSessionHandler);
    let session = timeout(peer_a.establish(None, handler, |_| Ok(())))
        .await
        .unwrap();

    // Try to send "think" event via session.request_event (not in the
    // negotiated emit_events snapshot) — must be rejected client-side.
    let result = session
        .request_event(XacppActivityEvent {
            activity: ActivityRef::new("test-act"),
            event: XacppEvent::new("think", json!({ "content": "hello" })),
        })
        .await;

    assert!(result.is_err(), "sending non-emit event should be rejected");
}

#[tokio::test]
async fn test_session_request_event() {
    let (peer_a, _peer_b) = negotiated_peers().await;

    let handler: Arc<dyn XacppSessionHandler> = Arc::new(TestSessionHandler);
    let session = timeout(peer_a.establish(None, handler, |_| Ok(())))
        .await
        .unwrap();

    let response = timeout(session.request_event(XacppActivityEvent {
        activity: ActivityRef::new("test-act"),
        event: XacppEvent::new("think", json!({ "content": "hi" })),
    }))
    .await
    .unwrap();

    match response {
        XacppResponse::Generic { name, .. } => assert_eq!(name, "acknowledge"),
        other => panic!("expected acknowledge, got: {other:?}"),
    }
}

// ---- Disconnect Scenarios Tests ----

#[tokio::test]
async fn test_disconnect_then_send_returns_error() {
    let (transport_a, transport_b) = duplex_pair();

    transport_b
        .on_request(Arc::new(|_session_id, _payload| {
            Box::pin(async { Ok(XacppResponse::acknowledge()) })
        }))
        .unwrap();

    transport_a.connect().await.unwrap();
    transport_b.connect().await.unwrap();

    // Normal communication
    let response = timeout(transport_a.send(
        None,
        XacppRequest::Command(XacppCommand::Establish { credentials: None }),
    ))
    .await
    .unwrap();
    assert!(matches!(response, XacppResponse::Generic { .. }));

    // Disconnect B
    transport_b.disconnect().await.unwrap();

    // A sends request again should receive error
    let result = timeout(transport_a.send(
        None,
        XacppRequest::Command(XacppCommand::Establish { credentials: None }),
    ))
    .await;
    assert!(result.is_err(), "expected error after disconnect");
}

#[tokio::test]
async fn test_on_handler_after_connect_returns_error() {
    let (transport_a, _transport_b) = duplex_pair();
    transport_a.connect().await.unwrap();

    // Registering handler after connect should return error
    let result = transport_a.on_request(Arc::new(|_session_id, _payload| {
        Box::pin(async { Ok(XacppResponse::acknowledge()) })
    }));
    assert!(result.is_err(), "on_request after connect should fail");
    assert!(matches!(result.unwrap_err(), XacppError::AlreadyConnected));
}

#[tokio::test]
async fn test_connect_disconnect_cycle() {
    let (stream_a, stream_b) = tokio::io::duplex(4096);
    let (a_read, a_write) = tokio::io::split(stream_a);
    let (b_read, b_write) = tokio::io::split(stream_b);

    let transport_a: Arc<dyn XacppTransport> = Arc::new(StdioTransport::new(
        Box::pin(a_write),
        Box::pin(BufReader::new(a_read)),
    ));
    let transport_b: Arc<dyn XacppTransport> = Arc::new(StdioTransport::new(
        Box::pin(b_write),
        Box::pin(BufReader::new(b_read)),
    ));

    // First connection
    transport_a.connect().await.unwrap();
    transport_b.connect().await.unwrap();

    // Disconnect
    transport_a.disconnect().await.unwrap();
    transport_b.disconnect().await.unwrap();

    // Reconnect should fail - reader/writer already consumed
    let result = transport_a.connect().await;
    assert!(result.is_err(), "reconnect after disconnect should fail");
}

// ---- Concurrent Requests Tests ----

#[tokio::test]
async fn test_concurrent_requests_id_matching() {
    let (transport_a, transport_b) = duplex_pair();

    transport_b
        .on_request(Arc::new(|_session_id, payload| {
            Box::pin(async move {
                // Echo back the command name to verify matching correctness
                let name: String = match payload {
                    XacppRequest::Command(XacppCommand::Establish { .. }) => "establish".into(),
                    XacppRequest::Command(XacppCommand::EstablishConfirm) => {
                        "establish_confirm".into()
                    }
                    XacppRequest::Command(XacppCommand::Negotiate { .. }) => "negotiate".into(),
                    XacppRequest::Command(XacppCommand::Fetch { .. }) => "fetch".into(),
                    XacppRequest::Command(XacppCommand::FetchBegin { .. }) => "fetch_begin".into(),
                    XacppRequest::Command(XacppCommand::FetchPause { .. }) => "fetch_pause".into(),
                    XacppRequest::Command(XacppCommand::FetchResume { .. }) => {
                        "fetch_resume".into()
                    }
                    XacppRequest::Command(XacppCommand::FetchAbort { .. }) => "fetch_abort".into(),
                    XacppRequest::Command(XacppCommand::Generic { name, .. }) => name,
                    XacppRequest::Event(_) => "event".into(),
                };
                Ok(XacppResponse::generic("echo", json!({ "command": name })))
            })
        }))
        .unwrap();

    transport_a.connect().await.unwrap();
    transport_b.connect().await.unwrap();

    let commands = [
        XacppCommand::Establish { credentials: None },
        XacppCommand::generic("new_activity", json!({})),
        XacppCommand::generic(
            "invoke_activity",
            json!({ "activity": "act-1", "messages": [] }),
        ),
        XacppCommand::generic("compact_activity", json!({ "activity": "act-1" })),
        XacppCommand::generic("cancel_activity", json!({ "activity": "act-1" })),
        XacppCommand::generic("last_activity", json!({})),
        XacppCommand::generic("list_activity", json!({ "pageNum": 1, "pageSize": 10 })),
        XacppCommand::generic("switch_activity", json!({ "activity": "act-1" })),
    ];

    // Concurrently send all requests
    let mut handles = Vec::new();
    for cmd in commands {
        let t = Arc::clone(&transport_a);
        handles.push(tokio::spawn(async move {
            timeout(t.send(None, XacppRequest::Command(cmd)))
                .await
                .unwrap()
        }));
    }

    // Collect all responses, verify each response matches its request
    let mut responses = Vec::with_capacity(handles.len());
    for h in handles {
        responses.push(h.await.unwrap());
    }

    let names: Vec<String> = responses
        .iter()
        .map(|r| match r {
            XacppResponse::Generic { name: _, data } => {
                data["command"].as_str().unwrap().to_string()
            }
            other => panic!("expected Generic echo, got: {other:?}"),
        })
        .collect();

    // All 8 different names received, no duplicates, no missing
    let mut sorted = names.clone();
    sorted.sort();
    assert_eq!(
        sorted,
        [
            "cancel_activity",
            "compact_activity",
            "establish",
            "invoke_activity",
            "last_activity",
            "list_activity",
            "new_activity",
            "switch_activity",
        ],
        "all 8 responses must be present and matched correctly"
    );
}

#[tokio::test]
async fn test_inflight_request_cancelled_on_disconnect() {
    // A sends request, B's handler intentionally does not respond (stall), then B disconnects.
    // A's pending send should receive Closed error.
    let (transport_a, transport_b) = duplex_pair();

    transport_b
        .on_request(Arc::new(move |_session_id, _payload| {
            Box::pin(std::future::pending::<Result<XacppResponse, XacppError>>())
        }))
        .unwrap();

    transport_a.connect().await.unwrap();
    transport_b.connect().await.unwrap();

    // A sends request (handler will block without responding)
    let t = Arc::clone(&transport_a);
    let send_handle = tokio::spawn(async move {
        timeout(t.send(
            None,
            XacppRequest::Command(XacppCommand::Establish { credentials: None }),
        ))
        .await
    });

    // Wait briefly to ensure request is sent and received by handler
    tokio::time::sleep(Duration::from_millis(50)).await;

    // B disconnects, handler is aborted, accept_loop exits, pending is cleaned up
    transport_b.disconnect().await.unwrap();

    // A's send should receive error
    let result = send_handle.await.unwrap();
    assert!(
        result.is_err(),
        "inflight request should fail after peer disconnect"
    );
}

// ---- Multi-Session Routing Isolation Tests ----

#[tokio::test]
async fn test_multi_session_routing_isolation() {
    // peer_b uses SequencedEstablishHandler, each session gets an ID'd handler
    let (transport_a, transport_b) = duplex_pair();
    // Kept aside for a direct (session-less) send under the first session id.
    let raw_transport_a = Arc::clone(&transport_a);
    let peer_a = XacppPeer::new(
        Capabilities {
            commands: Vec::new(),
            produce_events: Vec::new(),
            accept_events: Vec::new(),
            fetch_ext: None,
        },
        transport_a,
        Some(Arc::new(XacppSession::new(None))),
        Arc::new(AcceptNegotiateHandler::new()),
        Arc::new(SequencedEstablishHandler::new()),
    );
    let peer_b = XacppPeer::new(
        Capabilities {
            commands: Vec::new(),
            produce_events: Vec::new(),
            accept_events: Vec::new(),
            fetch_ext: None,
        },
        transport_b,
        None,
        Arc::new(AcceptNegotiateHandler::new()),
        Arc::new(SequencedEstablishHandler::new()),
    );
    peer_a.connect().await.unwrap();
    peer_b.connect().await.unwrap();
    peer_a.negotiate().await.unwrap();

    // A initiator: establish two sessions (responder registers an ID'd
    // handler per session). The responder's answers name two DIFFERENT
    // identities, so the second establish binds a new logical session (the
    // first one keeps its own identity and its envelope path), while the
    // responder's routing table keeps both handlers isolated.
    let handler_a: Arc<dyn XacppSessionHandler> = Arc::new(TestSessionHandler);
    let first = timeout(peer_a.establish(None, Arc::clone(&handler_a), |_| Ok(())))
        .await
        .unwrap();
    let sid_1_prev = first.session_id().await.to_owned();
    let session_2 = timeout(peer_a.establish(None, Arc::clone(&handler_a), |_| Ok(())))
        .await
        .unwrap();

    // The second establish bound a new session carrying the second identity.
    let sid_2 = session_2.session_id().await.to_owned();
    assert!(
        !sid_2.is_empty(),
        "second establish must backfill the initiator session identity"
    );
    assert_ne!(sid_1_prev, sid_2, "two sessions must have different IDs");

    // Send command via session_2 → B side routes to handler-2 → response identifies as "handler-2"
    let resp_2 =
        timeout(session_2.request_command(XacppCommand::generic("new_activity", json!({}))))
            .await
            .unwrap();
    match resp_2 {
        XacppResponse::Generic { name, data } => {
            assert_eq!(name, "handler_id");
            assert_eq!(data["id"], "handler-2", "session_2 must route to handler-2");
        }
        other => panic!("expected handler_id, got: {other:?}"),
    }

    // Cross-validation: the responder still routes the FIRST session id to
    // handler-1 (routing-table isolation across sessions on one connection).
    // Direct transport send under the first session id.
    let resp_1_again = timeout(raw_transport_a.send(
        Some(&sid_1_prev),
        XacppRequest::Command(XacppCommand::generic(
            "cancel_activity",
            json!({ "activity": "act-1" }),
        )),
    ))
    .await
    .unwrap();
    match resp_1_again {
        XacppResponse::Generic { name, data } => {
            assert_eq!(name, "handler_id");
            assert_eq!(
                data["id"], "handler-1",
                "first session id must still route to handler-1"
            );
        }
        other => panic!("expected handler_id, got: {other:?}"),
    }
}

// ============================================================================
// Establish: answer-driven session reuse (ts symmetry)
// ============================================================================

/// The responder's answer is the ruling on identity continuity: an answer
/// naming the held session's bound identity reuses that session — the
/// transfer mount is re-attached idempotently and the session's
/// transaction state survives the re-establish.
#[tokio::test]
async fn test_establish_same_identity_reuses_held_session() {
    // The responder always answers with the same identity ("auto-sid").
    let (peer_a, _peer_b) = negotiated_peers().await;

    let handler: Arc<dyn XacppSessionHandler> = Arc::new(TestSessionHandler);
    let first = timeout(peer_a.establish(None, Arc::clone(&handler), |_| Ok(())))
        .await
        .unwrap();
    let second = timeout(peer_a.establish(None, Arc::clone(&handler), |_| Ok(())))
        .await
        .unwrap();

    assert_eq!(first.session_id().await, "auto-sid");
    assert_eq!(second.session_id().await, "auto-sid");
    assert!(
        Arc::ptr_eq(&first, &second),
        "an answer naming the held identity must reuse the held session"
    );
    assert!(
        first.transfer_wired().await,
        "the reused session keeps its transfer mount"
    );
}

/// An answer naming a DIFFERENT identity births a new logical session: the
/// held session's transfer mount is released (its clock would never see
/// inbound activity again and would tear the shared connection down), while
/// its envelope identity stays routed under its own id.
#[tokio::test]
async fn test_establish_different_identity_binds_new_session() {
    let (transport_a, transport_b) = duplex_pair();
    let peer_a = XacppPeer::new(
        Capabilities {
            commands: Vec::new(),
            produce_events: Vec::new(),
            accept_events: Vec::new(),
            fetch_ext: None,
        },
        transport_a,
        Some(Arc::new(XacppSession::new(None))),
        Arc::new(AcceptNegotiateHandler::new()),
        Arc::new(AutoApproveEstablishHandler),
    );
    // The responder hands out a fresh identity per establish.
    let peer_b = XacppPeer::new(
        Capabilities {
            commands: Vec::new(),
            produce_events: Vec::new(),
            accept_events: Vec::new(),
            fetch_ext: None,
        },
        transport_b,
        None,
        Arc::new(AcceptNegotiateHandler::new()),
        Arc::new(SequencedEstablishHandler::new()),
    );
    peer_a.connect().await.unwrap();
    peer_b.connect().await.unwrap();
    peer_a.negotiate().await.unwrap();

    let handler: Arc<dyn XacppSessionHandler> = Arc::new(TestSessionHandler);
    let first = timeout(peer_a.establish(None, Arc::clone(&handler), |_| Ok(())))
        .await
        .unwrap();
    let second = timeout(peer_a.establish(None, Arc::clone(&handler), |_| Ok(())))
        .await
        .unwrap();

    assert_eq!(first.session_id().await, "handler-1");
    assert_eq!(second.session_id().await, "handler-2");
    assert!(
        !Arc::ptr_eq(&first, &second),
        "a different identity must bind a new session"
    );
    assert!(
        !first.transfer_wired().await,
        "the held session's transfer mount must be released"
    );
    assert!(
        second.transfer_wired().await,
        "the new session holds the transfer mount"
    );

    // The held session's envelope path is untouched: it still routes under
    // its own identity.
    let resp_1 = timeout(first.request_command(XacppCommand::generic("new_activity", json!({}))))
        .await
        .unwrap();
    match resp_1 {
        XacppResponse::Generic { name, data } => {
            assert_eq!(name, "handler_id");
            assert_eq!(data["id"], "handler-1");
        }
        other => panic!("expected handler_id, got: {other:?}"),
    }

    // The new session routes under the new identity.
    let resp_2 = timeout(second.request_command(XacppCommand::generic("new_activity", json!({}))))
        .await
        .unwrap();
    match resp_2 {
        XacppResponse::Generic { name, data } => {
            assert_eq!(name, "handler_id");
            assert_eq!(data["id"], "handler-2");
        }
        other => panic!("expected handler_id, got: {other:?}"),
    }
}

// ---- Challenge Handshake Flow Tests ----

#[tokio::test]
async fn test_peer_establish_challenge_flow() {
    let (transport_a, transport_b) = duplex_pair();
    let peer_a = XacppPeer::new(
        Capabilities {
            commands: Vec::new(),
            produce_events: Vec::new(),
            accept_events: Vec::new(),
            fetch_ext: None,
        },
        transport_a,
        Some(Arc::new(XacppSession::new(None))),
        Arc::new(AcceptNegotiateHandler::new()),
        Arc::new(ChallengeEstablishHandler),
    );
    let peer_b = XacppPeer::new(
        Capabilities {
            commands: Vec::new(),
            produce_events: Vec::new(),
            accept_events: Vec::new(),
            fetch_ext: None,
        },
        transport_b,
        None,
        Arc::new(AcceptNegotiateHandler::new()),
        Arc::new(ChallengeEstablishHandler),
    );
    peer_a.connect().await.unwrap();
    peer_b.connect().await.unwrap();
    peer_a.negotiate().await.unwrap();

    let handler: Arc<dyn XacppSessionHandler> = Arc::new(TestSessionHandler);
    let mut challenge_received = false;
    let session = timeout(peer_a.establish(None, handler, |challenge| {
        assert_eq!(challenge, "test-challenge");
        challenge_received = true;
        Ok(())
    }))
    .await
    .unwrap();

    assert!(challenge_received, "verify_challenge must be called");
    assert_eq!(session.session_id().await, "challenge-sid");
}

#[tokio::test]
async fn test_peer_establish_challenge_issues_credentials() {
    let (transport_a, transport_b) = duplex_pair();
    let peer_a = XacppPeer::new(
        Capabilities {
            commands: Vec::new(),
            produce_events: Vec::new(),
            accept_events: Vec::new(),
            fetch_ext: None,
        },
        transport_a,
        Some(Arc::new(XacppSession::new(None))),
        Arc::new(AcceptNegotiateHandler::new()),
        Arc::new(ChallengeEstablishHandler),
    );
    let peer_b = XacppPeer::new(
        Capabilities {
            commands: Vec::new(),
            produce_events: Vec::new(),
            accept_events: Vec::new(),
            fetch_ext: None,
        },
        transport_b,
        None,
        Arc::new(AcceptNegotiateHandler::new()),
        Arc::new(ChallengeEstablishHandler),
    );
    peer_a.connect().await.unwrap();
    peer_b.connect().await.unwrap();
    peer_a.negotiate().await.unwrap();

    let handler: Arc<dyn XacppSessionHandler> = Arc::new(TestSessionHandler);
    let session = timeout(peer_a.establish(None, handler, |challenge| {
        assert_eq!(challenge, "test-challenge");
        Ok(())
    }))
    .await
    .unwrap();

    assert_eq!(session.session_id().await, "challenge-sid");
    assert_eq!(session.credentials().await, "issued-creds");
}

#[tokio::test]
async fn test_session_send_message() {
    let (peer_a, _peer_b) = negotiated_peers().await;
    let session = timeout(peer_a.establish(None, Arc::new(TestSessionHandler), |_| Ok(())))
        .await
        .unwrap();
    let response = timeout(session.request_command(XacppCommand::generic(
        "message",
        json!({
            "content": [ContentPart::Text(TextPart { text: "hello".into(), part_id: None, part_type: "default".into() })]
        }),
    )))
    .await
    .unwrap();
    match response {
        XacppResponse::Generic { name, .. } => assert_eq!(name, "acknowledge"),
        other => panic!("expected acknowledge, got: {other:?}"),
    }
}

// ============================================================================
// Interaction Command Lifecycle Tests
// ============================================================================

/// Session handler that processes interaction commands and returns typed responses.
struct InteractionSessionHandler;

#[async_trait::async_trait]
impl XacppSessionHandler for InteractionSessionHandler {
    async fn on_command(&self, command: XacppCommand) -> Result<XacppResponse, XacppError> {
        match &command {
            XacppCommand::Generic {
                name, arguments, ..
            } => {
                match name.as_str() {
                    "action_request" => {
                        // Verify the payload is a valid ActionRequestPayload
                        let _: ActionRequestPayload = serde_json::from_value(arguments.clone())
                            .map_err(|e| XacppError::Internal(e.to_string()))?;
                        Ok(XacppResponse::generic(
                            "action",
                            serde_json::to_value(&ActionResponse::Approve).unwrap(),
                        ))
                    }
                    "question" => {
                        let payload: QuestionPayload = serde_json::from_value(arguments.clone())
                            .map_err(|e| XacppError::Internal(e.to_string()))?;
                        Ok(XacppResponse::generic(
                            "question",
                            serde_json::to_value(&QuestionResponse::Answer {
                                content: format!("answer to: {}", payload.question),
                            })
                            .unwrap(),
                        ))
                    }
                    "sensitive_info_operation" => {
                        let payload: SensitiveInfoOperationPayload =
                            serde_json::from_value(arguments.clone())
                                .map_err(|e| XacppError::Internal(e.to_string()))?;
                        let mut results = Vec::new();
                        match &payload.operation {
                            SensitiveInfoOperation::Collect { items } => {
                                for item in items {
                                    results.push(SensitiveInfoResult::Provided {
                                        key: item.key.clone(),
                                        value: "mock-value".into(),
                                    });
                                }
                            }
                            SensitiveInfoOperation::Delete { items } => {
                                for item in items {
                                    if let Some(id) = &item.id {
                                        results
                                            .push(SensitiveInfoResult::Deleted { id: id.clone() });
                                    }
                                }
                            }
                        }
                        Ok(XacppResponse::generic(
                            "sensitive_info_operation",
                            serde_json::to_value(&SensitiveInfoOperationResponse { results })
                                .unwrap(),
                        ))
                    }
                    _ => Ok(XacppResponse::acknowledge()),
                }
            }
            _ => Ok(XacppResponse::acknowledge()),
        }
    }

    async fn on_event(&self, _event: XacppActivityEvent) -> Result<XacppResponse, XacppError> {
        Ok(XacppResponse::acknowledge())
    }
}

/// EstablishHandler that returns InteractionSessionHandler for interaction tests.
struct InteractionEstablishHandler;

#[async_trait::async_trait]
impl EstablishHandler for InteractionEstablishHandler {
    async fn on_establish(
        &self,
        _transport: Arc<dyn XacppTransport>,
        _credentials: Option<String>,
    ) -> Result<EstablishDecision, XacppError> {
        Ok(EstablishDecision::Established {
            session_id: "interaction-sid".into(),
            handler: Arc::new(InteractionSessionHandler),
            credentials: "auto-creds".into(),
        })
    }

    async fn on_establish_confirm(
        &self,
        _transport: Arc<dyn XacppTransport>,
    ) -> Result<(String, Arc<dyn XacppSessionHandler>, String), XacppError> {
        Ok((
            "interaction-sid".into(),
            Arc::new(InteractionSessionHandler),
            "issued-creds".into(),
        ))
    }
}

#[tokio::test]
async fn test_action_request_command_lifecycle() {
    let (transport_a, transport_b) = duplex_pair();
    let peer_a = XacppPeer::new(
        Capabilities {
            commands: Vec::new(),
            produce_events: Vec::new(),
            accept_events: Vec::new(),
            fetch_ext: None,
        },
        transport_a,
        Some(Arc::new(XacppSession::new(None))),
        Arc::new(AcceptNegotiateHandler::new()),
        Arc::new(AutoApproveEstablishHandler),
    );
    // B side uses InteractionEstablishHandler → registers InteractionSessionHandler
    let peer_b = XacppPeer::new(
        Capabilities {
            commands: Vec::new(),
            produce_events: Vec::new(),
            accept_events: Vec::new(),
            fetch_ext: None,
        },
        transport_b,
        None,
        Arc::new(AcceptNegotiateHandler::new()),
        Arc::new(InteractionEstablishHandler),
    );
    peer_a.connect().await.unwrap();
    peer_b.connect().await.unwrap();
    peer_a.negotiate().await.unwrap();

    // B-side establishes with InteractionSessionHandler
    let session = timeout(peer_a.establish(None, Arc::new(InteractionSessionHandler), |_| Ok(())))
        .await
        .unwrap();

    // Build action_request command using convenience function
    let payload = ActionRequestPayload {
        tool_name: "bash".into(),
        arguments: r#"{"command":"ls"}"#.into(),
        action_id: "act-1".into(),
        description: "list files".into(),
        alert: AlertLevel::Warn,
        intent: "list files".into(),
        origin: vec![],
        request_id: String::new(),
    };
    let cmd = action_request_command("activity-1", &payload);

    let response = timeout(session.request_command(cmd)).await.unwrap();

    match response {
        XacppResponse::Generic { name, data } => {
            assert_eq!(name, "action");
            // Deserialize and verify the action response
            let action_resp: ActionResponse = serde_json::from_value(data).unwrap();
            assert!(matches!(action_resp, ActionResponse::Approve));
        }
        other => panic!("expected action response, got: {other:?}"),
    }
}

#[tokio::test]
async fn test_question_command_lifecycle() {
    let (transport_a, transport_b) = duplex_pair();
    let peer_a = XacppPeer::new(
        Capabilities {
            commands: Vec::new(),
            produce_events: Vec::new(),
            accept_events: Vec::new(),
            fetch_ext: None,
        },
        transport_a,
        Some(Arc::new(XacppSession::new(None))),
        Arc::new(AcceptNegotiateHandler::new()),
        Arc::new(AutoApproveEstablishHandler),
    );
    // B side uses InteractionEstablishHandler
    let peer_b = XacppPeer::new(
        Capabilities {
            commands: Vec::new(),
            produce_events: Vec::new(),
            accept_events: Vec::new(),
            fetch_ext: None,
        },
        transport_b,
        None,
        Arc::new(AcceptNegotiateHandler::new()),
        Arc::new(InteractionEstablishHandler),
    );
    peer_a.connect().await.unwrap();
    peer_b.connect().await.unwrap();
    peer_a.negotiate().await.unwrap();

    let session = timeout(peer_a.establish(None, Arc::new(InteractionSessionHandler), |_| Ok(())))
        .await
        .unwrap();

    let payload = QuestionPayload {
        question: "continue?".into(),
        options: vec!["yes".into(), "no".into()],
        origin: vec![],
        request_id: String::new(),
    };
    let cmd = question_command("activity-1", &payload);

    let response = timeout(session.request_command(cmd)).await.unwrap();

    match response {
        XacppResponse::Generic { name, data } => {
            assert_eq!(name, "question");
            let question_resp: QuestionResponse = serde_json::from_value(data).unwrap();
            match question_resp {
                QuestionResponse::Answer { content } => {
                    assert!(content.contains("continue?"));
                }
                other => panic!("expected Answer, got: {other:?}"),
            }
        }
        other => panic!("expected question response, got: {other:?}"),
    }
}

#[tokio::test]
async fn test_sensitive_info_command_lifecycle() {
    let (transport_a, transport_b) = duplex_pair();
    let peer_a = XacppPeer::new(
        Capabilities {
            commands: Vec::new(),
            produce_events: Vec::new(),
            accept_events: Vec::new(),
            fetch_ext: None,
        },
        transport_a,
        Some(Arc::new(XacppSession::new(None))),
        Arc::new(AcceptNegotiateHandler::new()),
        Arc::new(AutoApproveEstablishHandler),
    );
    // B side uses InteractionEstablishHandler
    let peer_b = XacppPeer::new(
        Capabilities {
            commands: Vec::new(),
            produce_events: Vec::new(),
            accept_events: Vec::new(),
            fetch_ext: None,
        },
        transport_b,
        None,
        Arc::new(AcceptNegotiateHandler::new()),
        Arc::new(InteractionEstablishHandler),
    );
    peer_a.connect().await.unwrap();
    peer_b.connect().await.unwrap();
    peer_a.negotiate().await.unwrap();

    let session = timeout(peer_a.establish(None, Arc::new(InteractionSessionHandler), |_| Ok(())))
        .await
        .unwrap();

    let payload = SensitiveInfoOperationPayload {
        operation: SensitiveInfoOperation::Collect {
            items: vec![
                SensitiveInfoItem {
                    id: None,
                    key: "API_KEY".into(),
                    display_text: "API Key".into(),
                    hint: "enter your key".into(),
                    si_type: SensitiveInfoType::Secret,
                },
                SensitiveInfoItem {
                    id: None,
                    key: "DB_PASSWORD".into(),
                    display_text: "Database Password".into(),
                    hint: "enter password".into(),
                    si_type: SensitiveInfoType::Secret,
                },
            ],
        },
        origin: vec![],
    };
    let cmd = sensitive_info_command("activity-1", &payload);

    let response = timeout(session.request_command(cmd)).await.unwrap();

    match response {
        XacppResponse::Generic { name, data } => {
            assert_eq!(name, "sensitive_info_operation");
            let resp: SensitiveInfoOperationResponse = serde_json::from_value(data).unwrap();
            assert_eq!(resp.results.len(), 2);
            // Both items should be Provided
            for result in &resp.results {
                match result {
                    SensitiveInfoResult::Provided { key, value } => {
                        assert!(!key.is_empty());
                        assert_eq!(value, "mock-value");
                    }
                    other => panic!("expected Provided, got: {other:?}"),
                }
            }
        }
        other => panic!("expected sensitive_info_operation response, got: {other:?}"),
    }
}
