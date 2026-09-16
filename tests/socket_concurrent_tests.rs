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
