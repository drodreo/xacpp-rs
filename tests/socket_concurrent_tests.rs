//! SocketTransport concurrent tests.
//!
//! Validates the spawn-per-request model of SocketTransport:
//! 1. Concurrent requests are processed independently, without crosstalk
//! 2. Concurrent writes of responses without data corruption
//! 3. Abort inflight tasks on disconnect, no deadlock

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::json;
use tokio::net::TcpListener;
use tokio::time::timeout;

use xacpp::commands::XacppCommand;
use xacpp::error::XacppError;
use xacpp::message::{XacppRequest, XacppResponse};
use xacpp::transport::XacppTransport;
use xacpp::transport::socket::SocketTransport;

/// Creates a pair of SocketTransport connected via TCP (client + server).
///
/// Server-side handler is specified by parameter, client handler returns acknowledge.
async fn socket_pair(
    server_handler: Arc<
        dyn Fn(
                Option<String>,
                XacppRequest,
            ) -> std::pin::Pin<
                Box<dyn std::future::Future<Output = Result<XacppResponse, XacppError>> + Send>,
            > + Send
            + Sync,
    >,
) -> (Arc<dyn XacppTransport>, Arc<dyn XacppTransport>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let addr_str = addr.to_string();

    // Server: accept and create SocketTransport
    let server_handle = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let server: Arc<dyn XacppTransport> = Arc::new(SocketTransport::new(stream));
        server.on_request(server_handler).unwrap();
        server.connect().await.unwrap();
        server
    });

    // Client: connect_to
    let client: Arc<dyn XacppTransport> = Arc::new(SocketTransport::connect_to(addr_str));
    client
        .on_request(Arc::new(|_session_id, _payload| {
            Box::pin(async { Ok(XacppResponse::acknowledge()) }) as _
        }))
        .unwrap();
    client.connect().await.unwrap();

    let server = server_handle.await.unwrap();
    (client, server)
}

/// Timeout wrapper (5s).
async fn timeout_5s<F, T>(future: F) -> T
where
    F: std::future::Future<Output = T>,
{
    tokio::time::timeout(Duration::from_secs(5), future)
        .await
        .expect("timeout")
}

// ---- Test 1: Concurrent requests independent processing ----

#[tokio::test]
async fn test_concurrent_requests_independent_processing() {
    // Server handler: sleep 10ms then echo the command name back in a Generic response
    let server_handler = Arc::new(|_session_id, payload| {
        Box::pin(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            let name = match payload {
                XacppRequest::Command(XacppCommand::Generic { name, .. }) => name,
                XacppRequest::Command(XacppCommand::Establish { .. }) => "establish".to_string(),
                XacppRequest::Command(XacppCommand::Negotiate { .. }) => "negotiate".to_string(),
                _ => "other".to_string(),
            };
            Ok(XacppResponse::generic("echo", json!({ "command": name })))
        }) as _
    });

    let (client, _server) = socket_pair(server_handler).await;

    let commands = [
        XacppCommand::generic("new_activity", json!({})),
        XacppCommand::generic(
            "invoke_activity",
            json!({ "activity": "act-1", "messages": [] }),
        ),
        XacppCommand::generic("compact_activity", json!({ "activity": "act-1" })),
        XacppCommand::generic("cancel_activity", json!({ "activity": "act-1" })),
        XacppCommand::Establish { credentials: None },
        XacppCommand::generic("last_activity", json!({})),
        XacppCommand::generic("list_activity", json!({ "pageNum": 1, "pageSize": 10 })),
        XacppCommand::generic("switch_activity", json!({ "activity": "act-1" })),
    ];

    // Concurrently send all requests, measure time
    let start = Instant::now();
    let mut handles = Vec::new();
    for cmd in commands {
        let c = Arc::clone(&client);
        handles.push(tokio::spawn(async move {
            timeout_5s(c.send(None, XacppRequest::Command(cmd)))
                .await
                .unwrap()
        }));
    }

    let mut responses = Vec::with_capacity(handles.len());
    for h in handles {
        responses.push(h.await.unwrap());
    }
    let elapsed = start.elapsed();

    // Collect command name from Generic responses
    let mut names: Vec<String> = responses
        .iter()
        .map(|r| match r {
            XacppResponse::Generic { name: _, data } => {
                data["command"].as_str().unwrap().to_string()
            }
            other => panic!("expected Generic echo, got: {other:?}"),
        })
        .collect();
    names.sort();

    // All 8 responses received, no crosstalk
    assert_eq!(
        names,
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
        "all responses must match their commands"
    );

    // Concurrent time < 50ms (serial would need 8×10ms=80ms)
    assert!(
        elapsed < Duration::from_millis(50),
        "concurrent processing should be faster than serial, took {:?}",
        elapsed
    );
}

// ---- Test 2: Concurrent writes without data corruption ----

#[tokio::test]
async fn test_concurrent_write_no_data_corruption() {
    // Server handler: returns 1KB text in Generic data
    let large_content = "A".repeat(1024);
    let server_handler = Arc::new(move |_session_id, _payload| {
        let text = large_content.clone();
        Box::pin(async move { Ok(XacppResponse::generic("large", json!({ "content": text }))) })
            as _
    });

    let (client, _server) = socket_pair(server_handler).await;

    // Concurrently send 10 requests
    let mut handles = Vec::new();
    for _ in 0..10 {
        let c = Arc::clone(&client);
        handles.push(tokio::spawn(async move {
            timeout_5s(c.send(
                None,
                XacppRequest::Command(XacppCommand::generic("ping", json!({}))),
            ))
            .await
            .unwrap()
        }));
    }

    let mut responses = Vec::with_capacity(handles.len());
    for h in handles {
        responses.push(h.await.unwrap());
    }

    // Each response content is complete (1KB, no truncation)
    for (i, resp) in responses.iter().enumerate() {
        match resp {
            XacppResponse::Generic { name: _, data } => {
                let content = data["content"].as_str().unwrap();
                assert_eq!(
                    content.len(),
                    1024,
                    "response {i} truncated: {} bytes",
                    content.len()
                );
                assert!(content.chars().all(|c| c == 'A'), "response {i} corrupted");
            }
            other => panic!("response {i}: expected Generic, got: {other:?}"),
        }
    }
}

// ---- Test 3: No deadlock on disconnect ----

#[tokio::test]
async fn test_disconnect_aborts_inflight_no_deadlock() {
    // Server handler: never returns
    let server_handler = Arc::new(|_session_id, _payload| {
        Box::pin(async { std::future::pending::<Result<XacppResponse, XacppError>>().await }) as _
    });

    let (client, _server) = socket_pair(server_handler).await;

    // Send 3 requests (handler will block)
    let mut send_handles = Vec::new();
    for _ in 0..3 {
        let c = Arc::clone(&client);
        send_handles.push(tokio::spawn(async move {
            c.send(
                None,
                XacppRequest::Command(XacppCommand::generic("ping", json!({}))),
            )
            .await
        }));
    }

    // Wait to ensure requests are sent and received by handler
    tokio::time::sleep(Duration::from_millis(50)).await;

    // disconnect should return within 2s
    let result = timeout(Duration::from_secs(2), client.disconnect()).await;
    assert!(result.is_ok(), "disconnect should not deadlock");
    assert!(result.unwrap().is_ok(), "disconnect should succeed");

    // All inflight sends should complete within timeout (no hang)
    for h in send_handles {
        let result = timeout(Duration::from_secs(2), h).await;
        assert!(result.is_ok(), "inflight send should not hang");
    }
}

// ---- Test: Ordered egress (single FIFO exit for all outbound frames) ----

use xacpp::events::{XacppActivityEvent, XacppEvent};

/// faf events sent sequentially arrive at the peer in submission order.
#[tokio::test]
async fn test_faf_events_arrive_in_submission_order() {
    let arrivals = Arc::new(std::sync::Mutex::new(Vec::<u64>::new()));
    let arrivals_handler = Arc::clone(&arrivals);
    let server_handler = Arc::new(move |_session_id, payload| {
        let arrivals = Arc::clone(&arrivals_handler);
        Box::pin(async move {
            if let XacppRequest::Event(event) = payload {
                let seq = event.event.data["seq"].as_u64().unwrap();
                arrivals.lock().unwrap().push(seq);
            }
            Ok(XacppResponse::acknowledge())
        }) as _
    });

    let (client, _server) = socket_pair(server_handler).await;

    const N: u64 = 50;
    for seq in 0..N {
        let event = XacppActivityEvent::new(
            "act-1",
            XacppEvent::new("content_delta", json!({ "seq": seq })),
        );
        client
            .send_faf(None, XacppRequest::Event(event))
            .await
            .unwrap();
    }

    // FIFO drain guarantee: a following round trip implies all prior frames arrived.
    client
        .send(
            None,
            XacppRequest::Command(XacppCommand::generic("sync", json!({}))),
        )
        .await
        .unwrap();

    assert_eq!(
        *arrivals.lock().unwrap(),
        (0..N).collect::<Vec<u64>>(),
        "faf events must arrive in submission order"
    );
}

/// A command enqueued between two faf events does not overtake them:
/// the single egress preserves the relative order of events and commands.
#[tokio::test]
async fn test_command_does_not_overtake_queued_events() {
    let arrivals = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let arrivals_handler = Arc::clone(&arrivals);
    let server_handler = Arc::new(move |_session_id, payload| {
        let arrivals = Arc::clone(&arrivals_handler);
        Box::pin(async move {
            let label = match payload {
                XacppRequest::Event(event) => {
                    format!("ev{}", event.event.data["seq"].as_u64().unwrap())
                }
                XacppRequest::Command(XacppCommand::Generic { name, .. }) => format!("cmd:{name}"),
                _ => "other".to_string(),
            };
            arrivals.lock().unwrap().push(label);
            Ok(XacppResponse::acknowledge())
        }) as _
    });

    let (client, _server) = socket_pair(server_handler).await;

    let ev = |seq: u64| {
        XacppActivityEvent::new(
            "act-1",
            XacppEvent::new("content_delta", json!({ "seq": seq })),
        )
    };
    client
        .send_faf(None, XacppRequest::Event(ev(0)))
        .await
        .unwrap();
    client
        .send(
            None,
            XacppRequest::Command(XacppCommand::generic("probe", json!({}))),
        )
        .await
        .unwrap();
    client
        .send_faf(None, XacppRequest::Event(ev(1)))
        .await
        .unwrap();
    // Final sync: drains ev1 before assertion.
    client
        .send(
            None,
            XacppRequest::Command(XacppCommand::generic("sync", json!({}))),
        )
        .await
        .unwrap();

    assert_eq!(
        *arrivals.lock().unwrap(),
        vec!["ev0", "cmd:probe", "ev1", "cmd:sync"],
        "commands must not overtake previously queued events"
    );
}

/// `send_faf` returns without waiting for the peer's ack, and does not stall
/// the pipe for subsequent round trips.
#[tokio::test]
async fn test_send_faf_returns_without_ack() {
    // Server never responds to events (handler pends forever); commands ack normally.
    let server_handler = Arc::new(|_session_id, payload| {
        Box::pin(async move {
            match payload {
                XacppRequest::Event(_) => {
                    std::future::pending::<Result<XacppResponse, XacppError>>().await
                }
                _ => Ok(XacppResponse::acknowledge()),
            }
        }) as _
    });

    let (client, _server) = socket_pair(server_handler).await;

    // faf returns immediately even though the peer never acks the event.
    let event = XacppActivityEvent::new(
        "act-1",
        XacppEvent::new("content_delta", json!({ "seq": 0 })),
    );
    timeout_5s(client.send_faf(None, XacppRequest::Event(event)))
        .await
        .unwrap();

    // The pipe is not stalled: a following command still round-trips.
    timeout_5s(client.send(
        None,
        XacppRequest::Command(XacppCommand::generic("probe", json!({}))),
    ))
    .await
    .unwrap();
}

// ---- Test: connection-close notification ----

/// `closed()` fires when the peer disconnects, even with zero traffic:
/// readers observe EOF and broadcast the close to all watchers.
#[tokio::test]
async fn test_closed_fires_on_peer_disconnect() {
    let server_handler =
        Arc::new(|_session_id, _payload| Box::pin(async { Ok(XacppResponse::acknowledge()) }) as _);
    let (client, server) = socket_pair(server_handler).await;

    let mut closed_rx = client.closed();
    assert!(!*closed_rx.borrow(), "connection starts open");

    server.disconnect().await.unwrap();

    tokio::time::timeout(Duration::from_secs(5), closed_rx.changed())
        .await
        .expect("closed notification timed out")
        .unwrap();
    assert!(
        *closed_rx.borrow(),
        "closed must be marked after peer disconnect"
    );
}

/// Late subscriber: subscribing after the close still observes the state via
/// `borrow()` (documented contract — `changed()` alone would miss it).
#[tokio::test]
async fn test_closed_late_subscriber_reads_state() {
    let server_handler =
        Arc::new(|_session_id, _payload| Box::pin(async { Ok(XacppResponse::acknowledge()) }) as _);
    let (client, server) = socket_pair(server_handler).await;

    server.disconnect().await.unwrap();

    // Wait for the close to propagate (first watcher drives it), then
    // subscribe late and read the state directly.
    let mut first = client.closed();
    tokio::time::timeout(Duration::from_secs(5), first.changed())
        .await
        .expect("closed notification timed out")
        .unwrap();

    let late = client.closed();
    assert!(
        *late.borrow(),
        "late subscriber must read closed state via borrow()"
    );
}
