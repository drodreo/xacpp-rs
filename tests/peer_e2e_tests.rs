//! Transport + Peer + Session end-to-end tests.
//!
//! Core scenarios covered:
//! 1. send: send request, handler callback processes and auto-replies (Acknowledge / business data / Error)
//! 2. routing: session_id routes correctly to corresponding Session handler
//! 3. Establish: handshake flow between initiator and responder
//! 4. Disconnect detection

use std::sync::Arc;
use std::time::Duration;

use tokio::io::BufReader;
use tokio::sync::mpsc;

use xacpp::commands::XacppCommand;
use xacpp::error::XacppError;
use xacpp::events::interaction::{ActionRequestEvent, ActionResponse};
use xacpp::events::payload::AlertLevel;
use xacpp::events::XacppEvent;
use xacpp::handler::{EstablishHandler, XacppSessionHandler};
use xacpp::message::{XacppRequest, XacppResponse};
use xacpp::peer::{PeerState, XacppPeer};
use xacpp::transport::stdio::StdioTransport;
use xacpp::transport::XacppTransport;

// ---- Test Handler Implementations ----

/// Generic Session handler: both Command and Event return Acknowledge.
struct TestSessionHandler;

#[async_trait::async_trait]
impl XacppSessionHandler for TestSessionHandler {
    async fn on_command(&self, _command: XacppCommand) -> Result<XacppResponse, XacppError> {
        Ok(XacppResponse::Acknowledge)
    }

    async fn on_event(&self, _event: XacppEvent) -> Result<XacppResponse, XacppError> {
        Ok(XacppResponse::Acknowledge)
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
    ) -> Result<(String, Arc<dyn XacppSessionHandler>), XacppError> {
        Ok(("auto-sid".into(), Arc::new(TestSessionHandler)))
    }
}

/// Session handler with ID: identifies itself through session_id in response.
struct IdentifiedHandler {
    id: String,
}

#[async_trait::async_trait]
impl XacppSessionHandler for IdentifiedHandler {
    async fn on_command(&self, _command: XacppCommand) -> Result<XacppResponse, XacppError> {
        Ok(XacppResponse::Established {
            session_id: self.id.clone(),
            credentials: None,
        })
    }

    async fn on_event(&self, _event: XacppEvent) -> Result<XacppResponse, XacppError> {
        Ok(XacppResponse::Established {
            session_id: self.id.clone(),
            credentials: None,
        })
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
    ) -> Result<(String, Arc<dyn XacppSessionHandler>), XacppError> {
        let n = self.counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let sid = format!("handler-{n}");
        Ok((sid.clone(), Arc::new(IdentifiedHandler { id: sid })))
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

/// Creates a pair of connected Peers (B side auto-approves Establish).
async fn connected_peers() -> (XacppPeer, XacppPeer) {
    let (transport_a, transport_b) = duplex_pair();
    let peer_a = XacppPeer::new(transport_a, Arc::new(AutoApproveEstablishHandler));
    let peer_b = XacppPeer::new(transport_b, Arc::new(AutoApproveEstablishHandler));
    peer_a.connect().await.unwrap();
    peer_b.connect().await.unwrap();
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

    transport_b.on_request(Arc::new(|session_id, payload| {
        Box::pin(async move {
            match (session_id, payload) {
                (None, XacppRequest::Command(XacppCommand::Establish { .. })) => {
                    Ok(XacppResponse::Established {
                        session_id: "sid-1".into(),
                        credentials: None,
                    })
                }
                _ => Ok(XacppResponse::Acknowledge),
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

    let (notify_tx, mut notify_rx) = mpsc::channel::<XacppEvent>(10);
    transport_b.on_request(Arc::new(move |_session_id, payload| {
        let tx = notify_tx.clone();
        Box::pin(async move {
            if let XacppRequest::Event(evt) = payload {
                let _ = tx.send(evt).await;
            }
            Ok(XacppResponse::Acknowledge)
        })
    }))
    .unwrap();

    transport_a.connect().await.unwrap();
    transport_b.connect().await.unwrap();

    let response = timeout(transport_a.send(
        Some("s1"),
        XacppRequest::Event(XacppEvent::Think {
            content: "hello".into(),
        }),
    ))
    .await
    .unwrap();

    assert!(matches!(response, XacppResponse::Acknowledge));

    let evt = timeout(notify_rx.recv()).await.unwrap();
    match evt {
        XacppEvent::Think { content } => assert_eq!(content, "hello"),
        other => panic!("expected Think, got: {other:?}"),
    }
}

#[tokio::test]
async fn test_transport_send_interactive_event() {
    let (transport_a, transport_b) = duplex_pair();

    transport_b.on_request(Arc::new(|session_id, payload| {
        Box::pin(async move {
            match (session_id, payload) {
                (Some(_), XacppRequest::Event(XacppEvent::ActionRequest(e))) => {
                    Ok(XacppResponse::Action {
                        request_id: e.request_id.clone(),
                        response: ActionResponse::Approve,
                    })
                }
                _ => Ok(XacppResponse::Acknowledge),
            }
        })
    }))
    .unwrap();

    transport_a.connect().await.unwrap();
    transport_b.connect().await.unwrap();

    let response = timeout(transport_a.send(
        Some("s1"),
        XacppRequest::Event(XacppEvent::ActionRequest(ActionRequestEvent {
            request_id: "req-1".into(),
            tool_name: "bash".into(),
            arguments: "{}".into(),
            action_id: "act-1".into(),
            description: "test".into(),
            alert: AlertLevel::Info,
            responder: None,
        })),
    ))
    .await
    .unwrap();

    match response {
        XacppResponse::Action {
            request_id,
            response,
        } => {
            assert_eq!(request_id, "req-1");
            assert!(matches!(response, ActionResponse::Approve));
        }
        other => panic!("expected Action response, got: {other:?}"),
    }
}

#[tokio::test]
async fn test_transport_handler_error() {
    let (transport_a, transport_b) = duplex_pair();

    transport_b.on_request(Arc::new(|_session_id, _payload| {
        Box::pin(async {
            Err(XacppError::Internal("something went wrong".into()))
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

    transport_a.on_request(Arc::new(|_session_id, _payload| {
        Box::pin(async move {
            Ok(XacppResponse::Established {
                session_id: "from-a".into(),
                credentials: None,
            })
        })
    }))
    .unwrap();

    transport_b.on_request(Arc::new(|_session_id, _payload| {
        Box::pin(async move {
            Ok(XacppResponse::Established {
                session_id: "from-b".into(),
                credentials: None,
            })
        })
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
        XacppResponse::Established { session_id, .. } => assert_eq!(session_id, "from-b"),
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
        XacppResponse::Established { session_id, .. } => assert_eq!(session_id, "from-a"),
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
    let (peer_a, _peer_b) = connected_peers().await;

    let handler: Arc<dyn XacppSessionHandler> = Arc::new(TestSessionHandler);
    let session = timeout(peer_a.establish(None, handler)).await.unwrap();

    assert!(!session.session_id().is_empty());
    assert!(session.credentials().is_none());
}

#[tokio::test]
async fn test_session_request_command() {
    let (peer_a, _peer_b) = connected_peers().await;

    let handler: Arc<dyn XacppSessionHandler> = Arc::new(TestSessionHandler);
    let session = timeout(peer_a.establish(None, handler)).await.unwrap();

    let response = timeout(session.request_command(XacppCommand::NewActivity))
        .await
        .unwrap();

    assert!(matches!(response, XacppResponse::Acknowledge));
}

#[tokio::test]
async fn test_session_request_event() {
    let (peer_a, _peer_b) = connected_peers().await;

    let handler: Arc<dyn XacppSessionHandler> = Arc::new(TestSessionHandler);
    let session = timeout(peer_a.establish(None, handler)).await.unwrap();

    let response = timeout(session.request_event(XacppEvent::Think {
        content: "hi".into(),
    }))
    .await
    .unwrap();

    assert!(matches!(response, XacppResponse::Acknowledge));
}

// ---- Disconnect Scenarios Tests ----

#[tokio::test]
async fn test_disconnect_then_send_returns_error() {
    let (transport_a, transport_b) = duplex_pair();

    transport_b.on_request(Arc::new(|_session_id, _payload| {
        Box::pin(async { Ok(XacppResponse::Acknowledge) })
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
    assert!(matches!(response, XacppResponse::Acknowledge));

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
        Box::pin(async { Ok(XacppResponse::Acknowledge) })
    }));
    assert!(result.is_err(), "on_request after connect should fail");
    assert!(matches!(
        result.unwrap_err(),
        XacppError::AlreadyConnected
    ));
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

    transport_b.on_request(Arc::new(|_session_id, payload| {
        Box::pin(async move {
            // Return different session_id based on command type, to verify matching correctness
            let sid = if let XacppRequest::Command(cmd) = payload {
                match cmd {
                    XacppCommand::Establish { .. } => "establish",
                    XacppCommand::NewActivity => "new",
                    XacppCommand::InvokeActivity => "invoke",
                    XacppCommand::CompactActivity => "compact",
                    XacppCommand::CancelActivity => "cancel",
                }
            } else {
                "event"
            };
            Ok(XacppResponse::Established {
                session_id: sid.into(),
                credentials: None,
            })
        })
    }))
    .unwrap();

    transport_a.connect().await.unwrap();
    transport_b.connect().await.unwrap();

    let commands = [
        XacppCommand::Establish { credentials: None },
        XacppCommand::NewActivity,
        XacppCommand::InvokeActivity,
        XacppCommand::CompactActivity,
        XacppCommand::CancelActivity,
    ];

    // Concurrently send 5 requests
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

    let sids: Vec<&str> = responses
        .iter()
        .map(|r| match r {
            XacppResponse::Established { session_id, .. } => session_id.as_str(),
            other => panic!("expected Established response, got: {other:?}"),
        })
        .collect();

    // All 5 different sids received, no duplicates, no missing
    let mut sorted = sids.clone();
    sorted.sort();
    assert_eq!(
        sorted,
        ["cancel", "compact", "establish", "invoke", "new"],
        "all 5 responses must be present and matched correctly"
    );
}

#[tokio::test]
async fn test_inflight_request_cancelled_on_disconnect() {
    // A sends request, B's handler intentionally does not respond (stall), then B disconnects.
    // A's pending send should receive Closed error.
    let (transport_a, transport_b) = duplex_pair();

    transport_b.on_request(Arc::new(move |_session_id, _payload| {
        Box::pin(std::future::pending::<Result<XacppResponse, XacppError>>())
    }))
    .unwrap();

    transport_a.connect().await.unwrap();
    transport_b.connect().await.unwrap();

    // A sends request (handler will block without responding)
    let t = Arc::clone(&transport_a);
    let send_handle = tokio::spawn(async move {
        timeout(t.send(None, XacppRequest::Command(XacppCommand::Establish { credentials: None })))
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
    let peer_a = XacppPeer::new(transport_a, Arc::new(SequencedEstablishHandler::new()));
    let peer_b = XacppPeer::new(transport_b, Arc::new(SequencedEstablishHandler::new()));
    peer_a.connect().await.unwrap();
    peer_b.connect().await.unwrap();

    // A initiator: establish two sessions, handler returns Acknowledge for commands (doesn't matter)
    let handler_a: Arc<dyn XacppSessionHandler> = Arc::new(TestSessionHandler);
    let session_1 = timeout(peer_a.establish(None, Arc::clone(&handler_a)))
        .await
        .unwrap();
    let session_2 = timeout(peer_a.establish(None, Arc::clone(&handler_a)))
        .await
        .unwrap();

    let sid_1 = session_1.session_id().to_owned();
    let sid_2 = session_2.session_id().to_owned();
    assert_ne!(sid_1, sid_2, "two sessions must have different IDs");

    // Send command via session_1 → B side routes to handler-1 → response session_id = "handler-1"
    let resp_1 = timeout(session_1.request_command(XacppCommand::NewActivity))
        .await
        .unwrap();
    match resp_1 {
        XacppResponse::Established { session_id, .. } => {
            assert_eq!(session_id, "handler-1", "session_1 must route to handler-1");
        }
        other => panic!("expected Established (handler identity), got: {other:?}"),
    }

    // Send command via session_2 → B side routes to handler-2 → response session_id = "handler-2"
    let resp_2 = timeout(session_2.request_command(XacppCommand::NewActivity))
        .await
        .unwrap();
    match resp_2 {
        XacppResponse::Established { session_id, .. } => {
            assert_eq!(session_id, "handler-2", "session_2 must route to handler-2");
        }
        other => panic!("expected Established (handler identity), got: {other:?}"),
    }

    // Cross-validation: session_1 sends again, still routes to handler-1
    let resp_1_again = timeout(session_1.request_command(XacppCommand::CancelActivity))
        .await
        .unwrap();
    match resp_1_again {
        XacppResponse::Established { session_id, .. } => {
            assert_eq!(session_id, "handler-1", "session_1 must still route to handler-1");
        }
        other => panic!("expected Established (handler identity), got: {other:?}"),
    }
}
