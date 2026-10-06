//! Egress throughput / ordering benchmark over a real TCP socket pair.
//!
//! Mirrors the wire-level form of `tests/egress_wire_tests.rs`: the receiving
//! side is a raw TCP peer (no xacpp transport), so what is measured is the
//! true on-the-wire behaviour of the ordered egress queue + fire-and-forget
//! send. The default scale of 40k events matches the typical stream length
//! previously captured in high-latency scenarios.
//!
//! Usage:
//! ```text
//! cargo run --example egress_bench
//! XACPP_BENCH_N=100000 cargo run --example egress_bench
//! ```
//!
//! Verdict: PASS requires the peer to receive exactly `N` event seqs in
//! strictly increasing order starting at 0 — no loss, no reorder.

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::TcpListener;

use xacpp::events::{XacppActivityEvent, XacppEvent};
use xacpp::message::{XacppRequest, XacppResponse};
use xacpp::transport::XacppTransport;
use xacpp::transport::socket::SocketTransport;

/// Typical stream length captured in high-latency scenarios (batch-1 baseline).
const DEFAULT_N: u64 = 40_000;
/// Generous wall-clock cap for the whole drain phase.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(120);

fn bench_n() -> u64 {
    match std::env::var("XACPP_BENCH_N") {
        Ok(v) => v
            .trim()
            .parse()
            .expect("XACPP_BENCH_N must be a positive integer"),
        Err(_) => DEFAULT_N,
    }
}

/// Extract the event sequence number from a raw JSONL frame.
fn frame_seq(line: &str) -> Option<u64> {
    let v: Value = serde_json::from_str(line).ok()?;
    if v["type"] != "request" {
        return None;
    }
    v.pointer("/payload/payload/event/data/seq")?.as_u64()
}

struct Collected {
    seqs: Vec<u64>,
    bytes: u64,
}

/// Raw TCP peer collecting event seqs until `expected` frames arrive.
async fn raw_seq_peer(expected: usize) -> (String, tokio::task::JoinHandle<Collected>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let handle = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut lines = BufReader::new(stream).lines();
        let mut seqs = Vec::with_capacity(expected);
        let mut bytes = 0u64;
        while seqs.len() < expected {
            match lines.next_line().await {
                Ok(Some(line)) => {
                    bytes += line.len() as u64 + 1; // + '\n'
                    if let Some(seq) = frame_seq(&line) {
                        seqs.push(seq);
                    }
                }
                _ => break,
            }
        }
        Collected { seqs, bytes }
    });
    (addr, handle)
}

/// Client transport connected to a raw peer (the peer never sends requests,
/// so the handler is never invoked; it acknowledges anything just in case).
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

#[tokio::main]
async fn main() {
    let (n, scale_source) = match std::env::var("XACPP_BENCH_N") {
        Ok(_) => (bench_n(), "env:XACPP_BENCH_N"),
        Err(_) => (DEFAULT_N, "default"),
    };

    let (addr, collector) = raw_seq_peer(n as usize).await;
    let client = raw_client(addr).await;

    println!(
        "# xacpp egress bench | events={n} ({scale_source}) | transport=tcp socket-pair (raw peer) | path=send_faf -> ordered egress"
    );

    let started = Instant::now();
    for seq in 0..n {
        client.send_faf(None, seq_event(seq)).await.unwrap();
    }
    let issue = started.elapsed();

    let collected = tokio::time::timeout(DRAIN_TIMEOUT, collector)
        .await
        .expect("wire drain timed out")
        .unwrap();
    let drain = started.elapsed();

    let issue_rate = n as f64 / issue.as_secs_f64();
    let throughput = n as f64 / drain.as_secs_f64();
    let avg_frame = if collected.seqs.is_empty() {
        0
    } else {
        collected.bytes / collected.seqs.len() as u64
    };

    println!("issue : {n} events enqueued in {issue:.3?} ({issue_rate:.0} events/s, enqueue-only)");
    println!(
        "drain : peer received last frame at {drain:.3?} since issue start ({throughput:.0} events/s end-to-end)"
    );
    println!(
        "wire  : {} frames on the wire, {} bytes total (avg {avg_frame} bytes/frame)",
        collected.seqs.len(),
        collected.bytes
    );

    // Correctness: exactly N seqs, strictly increasing from 0 — no loss, no
    // reorder (wire order must equal submission order).
    let received = collected.seqs.len() as u64;
    let in_order = collected.seqs == (0..n).collect::<Vec<u64>>();
    let pass = received == n && in_order;

    println!("verify: received {received}/{n}, strictly-increasing-from-0={in_order}");
    println!("result: {}", if pass { "PASS" } else { "FAIL" });
    if !pass {
        std::process::exit(1);
    }
}
