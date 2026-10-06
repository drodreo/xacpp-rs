//! Wire-level egress tests: a raw TCP peer (no transport on the receiving
//! side) observes frames, asserting true on-the-wire order and throughput.
//!
//! These complement the transport-level tests: SocketTransport's server side
//! spawns one handler task per request, so handler-observed order is subject
//! to task scheduling; wire order is the actual invariant the egress
//! guarantees.

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::TcpListener;

use xacpp::events::{XacppActivityEvent, XacppEvent};
use xacpp::message::{XacppRequest, XacppResponse};
use xacpp::transport::XacppTransport;
use xacpp::transport::socket::SocketTransport;

/// Extract the event sequence number from a raw JSONL frame.
fn frame_seq(line: &str) -> Option<u64> {
    let v: Value = serde_json::from_str(line).ok()?;
    if v["type"] != "request" {
        return None;
    }
    v.pointer("/payload/payload/event/data/seq")?.as_u64()
}

/// Start a raw TCP peer collecting event seqs until `expected` frames arrive
/// (or the connection closes). Returns (addr, collector handle).
async fn raw_seq_peer(expected: usize) -> (String, tokio::task::JoinHandle<Vec<u64>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let handle = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut lines = BufReader::new(stream).lines();
        let mut seqs = Vec::new();
        while seqs.len() < expected {
            match lines.next_line().await {
                Ok(Some(line)) => {
                    if let Some(seq) = frame_seq(&line) {
                        seqs.push(seq);
                    }
                }
                _ => break,
            }
        }
        seqs
    });
    (addr, handle)
}

/// Client transport connected to a raw peer (handler acknowledges anything;
/// the peer never sends requests, so it is never invoked).
async fn raw_client(addr: String) -> Arc<dyn XacppTransport> {
    let client: Arc<dyn XacppTransport> = Arc::new(SocketTransport::connect_to(addr));
    client
        .on_request(Arc::new(|_session_id, _payload| {
            Box::pin(async { Ok(XacppResponse::acknowledge()) }) as _
        }))
        .unwrap();
    client.connect().await.unwrap();
    client
}

fn seq_event(seq: u64) -> XacppRequest {
    XacppRequest::Event(XacppActivityEvent::new(
        "act-1",
        XacppEvent::new("content_delta", json!({ "seq": seq })),
    ))
}

/// Wire order: 1000 faf events arrive exactly in submission order.
#[tokio::test]
async fn test_wire_order_of_faf_stream() {
    const N: u64 = 1000;
    let (addr, collector) = raw_seq_peer(N as usize).await;
    let client = raw_client(addr).await;

    for seq in 0..N {
        client.send_faf(None, seq_event(seq)).await.unwrap();
    }

    let seqs = tokio::time::timeout(Duration::from_secs(10), collector)
        .await
        .expect("wire drain timed out")
        .unwrap();
    assert_eq!(
        seqs,
        (0..N).collect::<Vec<_>>(),
        "wire order must equal submission order"
    );
}

/// Throughput: issuing 1000 faf events is enqueue-only (nowhere near the
/// per-event round trip a wait-per-event design would cost), and the wire
/// drain keeps pace.
#[tokio::test]
async fn test_faf_issue_throughput() {
    const N: u64 = 1000;
    let (addr, collector) = raw_seq_peer(N as usize).await;
    let client = raw_client(addr).await;

    let started = Instant::now();
    for seq in 0..N {
        client.send_faf(None, seq_event(seq)).await.unwrap();
    }
    let issue_cost = started.elapsed();

    let seqs = tokio::time::timeout(Duration::from_secs(10), collector)
        .await
        .expect("wire drain timed out")
        .unwrap();
    let drain_cost = started.elapsed();

    assert_eq!(seqs.len() as u64, N);
    // Generous bounds: the point is orders of magnitude, not micro-benchmarks.
    assert!(
        issue_cost < Duration::from_millis(500),
        "faf issue path must be enqueue-only (took {issue_cost:?} for {N} events)"
    );
    assert!(
        drain_cost < Duration::from_secs(5),
        "wire drain must keep pace (took {drain_cost:?} for {N} events)"
    );
    println!("faf issue: {N} events in {issue_cost:?}; wire drained in {drain_cost:?}");
}

/// Multi-sender stability: concurrent producers lose no frames and corrupt
/// none — every submitted seq arrives exactly once (cross-sender order is
/// intentionally unspecified).
#[tokio::test]
async fn test_concurrent_producers_no_loss() {
    const PRODUCERS: u64 = 8;
    const PER: u64 = 125;
    const N: u64 = PRODUCERS * PER;
    let (addr, collector) = raw_seq_peer(N as usize).await;
    let client = raw_client(addr).await;

    let mut tasks = Vec::new();
    for p in 0..PRODUCERS {
        let client = Arc::clone(&client);
        tasks.push(tokio::spawn(async move {
            for i in 0..PER {
                let seq = p * PER + i;
                client.send_faf(None, seq_event(seq)).await.unwrap();
            }
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }

    let mut seqs = tokio::time::timeout(Duration::from_secs(10), collector)
        .await
        .expect("wire drain timed out")
        .unwrap();
    seqs.sort_unstable();
    assert_eq!(
        seqs,
        (0..N).collect::<Vec<_>>(),
        "every frame must arrive exactly once"
    );
}
