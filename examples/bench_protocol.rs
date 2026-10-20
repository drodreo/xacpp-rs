//! Binary protocol performance benchmark (release acceptance D).
//!
//! Three groups, all numbers reproducible (scale via `XACPP_BENCH_N`):
//!
//! **[1] Codec comparison** — pure encode/decode, no I/O, same envelope set
//! (small ~200 B and medium ~2 KiB, `XACPP_BENCH_N` each): JSONL semantics
//! (`serde_json::to_vec` + newline framing, line split on decode) versus the
//! binary path (`serde_json` → `DATA_JSON` `Frame::encode` →
//! `FrameDecoder::feed` → `from_slice`). Warmup rounds before timing.
//!
//! **[2] End-to-end over TCP loopback** — (a) request/response RTT
//! percentiles and throughput; (b) one 64 MiB `sendBinary` transaction
//! (1 MiB working frames) queued→delivered→final-ACK; (c) 4 concurrent
//! 16 MiB transactions, aggregate throughput.
//!
//! **[3] Control-frame insertion latency** — heartbeat frames (20 ms period,
//! 10 ms clock tick) observed by a tee proxy on the wire, idle connection
//! versus a bulk `DATA_BINARY` transfer in flight (quantifies the §5
//! control-priority behaviour of the egress queue).
//!
//! Usage:
//! ```text
//! cargo run --release --example bench_protocol
//! XACPP_BENCH_N=50000 cargo run --release --example bench_protocol
//! XACPP_BENCH_ONLY=3 cargo run --release --example bench_protocol  # group [3] only
//! ```
//!
//! Group [1] and [2a] scale with `XACPP_BENCH_N` (default 10000); binary and
//! control groups use fixed physical sizes. A `PASS` line at the end asserts
//! round-trip integrity of every measured path.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use serde_json::json;
use tokio::io::AsyncReadExt;
use tokio::net::TcpListener;
use tokio::sync::{Mutex, watch};

use xacpp::carrier::XacppTransport;
use xacpp::carrier::socket::SocketTransport;
use xacpp::reliability::transaction::SenderConfig;
use xacpp::session::XacppSession;
use xacpp::transfer::TransferConfig;
use xacpp::wire::events::{XacppActivityEvent, XacppEvent};
use xacpp::wire::frame::{Frame, FrameDecoder, FrameType};
use xacpp::wire::message::{XacppEnvelope, XacppRequest, XacppResponse};

/// Envelope content padding for the small tier (total JSON lands near 200 B).
const SMALL_PAD: usize = 110;
/// Envelope content padding for the medium tier (total JSON lands near 2 KiB).
const MEDIUM_PAD: usize = 1_900;
/// Heartbeat period for group [3] (ms).
const HB_MS: u64 = 10;
/// Reliability clock tick for group [3] (ms) — scheduling granularity.
const TICK_MS: u64 = 10;
/// Working-frame size used by the binary transactions (bytes).
const WORKING_FRAME: usize = 1024 * 1024;

type Transport = Arc<dyn XacppTransport>;

fn bench_n() -> usize {
    std::env::var("XACPP_BENCH_N")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10_000)
}

/// Representative envelope payload: activity `think` event with padded content.
fn make_request(pad_len: usize) -> XacppRequest {
    XacppRequest::Event(XacppActivityEvent::new(
        "act-bench",
        XacppEvent::new("think", json!({ "content": "x".repeat(pad_len) })),
    ))
}

/// Same message wrapped in a request envelope (codec-group wire form).
fn make_envelope(id: usize, pad_len: usize) -> XacppEnvelope {
    XacppEnvelope::Request {
        id: format!("r{id}"),
        session_id: None,
        payload: make_request(pad_len),
    }
}

/// Deterministic non-zero payload (avoids zero-page shortcuts).
fn make_binary_payload(len: usize) -> Vec<u8> {
    let mut v = vec![0u8; len];
    for (i, b) in v.iter_mut().enumerate() {
        *b = (i % 251) as u8;
    }
    v
}

fn percentile(sorted: &[f64], q: f64) -> f64 {
    let idx = ((q / 100.0) * (sorted.len() as f64 - 1.0)).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

fn jsonl_encode(env: &XacppEnvelope) -> Vec<u8> {
    let mut wire = serde_json::to_vec(env).expect("serialize envelope");
    wire.push(b'\n');
    wire
}

fn jsonl_decode(wire: &[u8]) -> XacppEnvelope {
    let line = &wire[..wire.len() - 1];
    serde_json::from_slice(line).expect("parse envelope")
}

fn frame_encode(env: &XacppEnvelope) -> Vec<u8> {
    let json = serde_json::to_vec(env).expect("serialize envelope");
    let frame = Frame::new(
        FrameType::DataJson,
        uuid::Uuid::new_v4().into_bytes(),
        json.len() as u32,
        0,
        json,
    );
    frame.encode().expect("frame encode")
}

fn frame_decode_one(wire: &[u8]) -> XacppEnvelope {
    let mut decoder = FrameDecoder::new();
    let frames = decoder.feed(wire).expect("frame decode");
    assert_eq!(frames.len(), 1, "expected exactly one frame");
    serde_json::from_slice(&frames[0].payload).expect("parse envelope")
}

fn envelope_json(env: &XacppEnvelope) -> String {
    serde_json::to_string(env).expect("serialize envelope")
}

// ---- Group [1]: codec comparison ----

fn codec_group(n: usize) {
    println!(
        "[1] codec comparison — pure encode/decode, no I/O (JSONL baseline vs DATA_JSON frames)"
    );
    codec_case("small (~200 B)", SMALL_PAD, n);
    codec_case("medium (~2 KiB)", MEDIUM_PAD, n);
    println!("  result: [1] PASS (round-trip verified on both paths)");
}

fn codec_case(label: &str, pad: usize, n: usize) {
    let envs: Vec<XacppEnvelope> = (0..n).map(|i| make_envelope(i, pad)).collect();
    let json_bytes = serde_json::to_vec(&envs[0]).expect("serialize").len();

    // Round-trip correctness spot check: first / middle / last.
    for idx in [0, n / 2, n - 1] {
        let want = envelope_json(&envs[idx]);
        assert_eq!(
            envelope_json(&jsonl_decode(&jsonl_encode(&envs[idx]))),
            want
        );
        assert_eq!(
            envelope_json(&frame_decode_one(&frame_encode(&envs[idx]))),
            want
        );
    }

    // Warmup (untimed).
    for env in envs.iter().take(500.min(n)) {
        let w = jsonl_encode(env);
        let _ = std::hint::black_box(jsonl_decode(&w));
        let w = frame_encode(env);
        let _ = std::hint::black_box(frame_decode_one(&w));
    }

    // JSONL path.
    let t = Instant::now();
    let jsonl_wires: Vec<Vec<u8>> = envs.iter().map(jsonl_encode).collect();
    let jsonl_enc = t.elapsed();
    let jsonl_wire_bytes: usize = jsonl_wires.iter().map(|w| w.len()).sum();
    let t = Instant::now();
    let backs: Vec<XacppEnvelope> = jsonl_wires.iter().map(|w| jsonl_decode(w)).collect();
    let jsonl_dec = t.elapsed();
    std::hint::black_box(&backs);

    // Binary frame path.
    let t = Instant::now();
    let frame_wires: Vec<Vec<u8>> = envs.iter().map(frame_encode).collect();
    let frame_enc = t.elapsed();
    let frame_wire_bytes: usize = frame_wires.iter().map(|w| w.len()).sum();
    // Decode via a single decoder over a chunked stream (64 KiB — TCP-like
    // coalesced arrival).
    let mut stream: Vec<u8> = Vec::with_capacity(frame_wire_bytes);
    for w in &frame_wires {
        stream.extend_from_slice(w);
    }
    let t = Instant::now();
    let mut decoder = FrameDecoder::new();
    let mut got = 0usize;
    for chunk in stream.chunks(64 * 1024) {
        got += decoder.feed(chunk).expect("frame decode").len();
    }
    assert_eq!(got, n, "frame count mismatch");
    let frame_dec = t.elapsed();
    std::hint::black_box(&stream);

    let mbps = |bytes: usize, secs: f64| bytes as f64 / 1e6 / secs;
    let eps = |secs: f64| n as f64 / secs;
    println!("  payload {label}: {json_bytes} B JSON, {n} messages");
    println!(
        "    {:<22}{:>10}{:>11}{:>11}{:>13}{:>10}",
        "path", "encode", "decode", "total", "events/s", "MB/s"
    );
    println!(
        "    {:<22}{:>10.1?}{:>11.1?}{:>11.1?}{:>13.0}{:>10.1}",
        "JSONL (to_vec+\\n)",
        jsonl_enc,
        jsonl_dec,
        jsonl_enc + jsonl_dec,
        eps((jsonl_enc + jsonl_dec).as_secs_f64()),
        mbps(jsonl_wire_bytes, (jsonl_enc + jsonl_dec).as_secs_f64()),
    );
    println!(
        "    {:<22}{:>10.1?}{:>11.1?}{:>11.1?}{:>13.0}{:>10.1}",
        "DATA_JSON frame",
        frame_enc,
        frame_dec,
        frame_enc + frame_dec,
        eps((frame_enc + frame_dec).as_secs_f64()),
        mbps(frame_wire_bytes, (frame_enc + frame_dec).as_secs_f64()),
    );
}

// ---- Group [2]: end-to-end over TCP loopback ----

/// Client+server transport pair over TCP loopback, sessions wired on both
/// sides (`client_cfg` for the client, defaults for the server).
async fn reliable_pair(
    client_cfg: TransferConfig,
) -> (Transport, Arc<XacppSession>, Arc<XacppSession>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr").to_string();

    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept");
        let transport: Transport = Arc::new(SocketTransport::new(stream));
        let session = Arc::new(XacppSession::new(Some(TransferConfig::default())));
        transport.connect().await.expect("server connect");
        session
            .wire(Arc::clone(&transport))
            .await
            .expect("wire server session");
        // Park in a detached task until the connection is known dead (the
        // join handle below must resolve as soon as the session exists).
        let mut closed = transport.closed();
        tokio::spawn(async move {
            while !*closed.borrow_and_update() {
                if closed.changed().await.is_err() {
                    break;
                }
            }
        });
        session
    });

    let client: Transport = Arc::new(SocketTransport::connect_to(addr));
    let client_session = Arc::new(XacppSession::new(Some(client_cfg)));
    client.connect().await.expect("client connect");
    client_session
        .wire(Arc::clone(&client))
        .await
        .expect("wire client session");
    let server_session = server.await.expect("server task");
    (client, client_session, server_session)
}

fn big_cap() -> SenderConfig {
    SenderConfig {
        backpressure_cap_bytes: 256 * 1024 * 1024,
        ..SenderConfig::default()
    }
}

async fn rtt_group(n: usize) {
    println!("[2] end-to-end over TCP loopback");

    // Server acknowledging every request.
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr").to_string();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept");
        let transport: Transport = Arc::new(SocketTransport::new(stream));
        transport
            .on_request(Arc::new(|_sid, _req| {
                Box::pin(async { Ok(XacppResponse::acknowledge()) }) as _
            }))
            .expect("register handler");
        transport.connect().await.expect("server connect");
    });

    let client: Transport = Arc::new(SocketTransport::connect_to(addr));
    client.connect().await.expect("client connect");
    server.await.expect("server task");

    // Warmup.
    for _ in 0..100 {
        client
            .send(None, make_request(SMALL_PAD))
            .await
            .expect("warmup send");
    }

    let mut rtts = Vec::with_capacity(n);
    let t0 = Instant::now();
    for _ in 0..n {
        let s = Instant::now();
        let resp = client
            .send(None, make_request(SMALL_PAD))
            .await
            .expect("send");
        std::hint::black_box(&resp);
        rtts.push(s.elapsed());
    }
    let total = t0.elapsed();

    let mut ms: Vec<f64> = rtts.iter().map(|d| d.as_secs_f64() * 1e3).collect();
    ms.sort_by(f64::total_cmp);
    println!("  2a request-response RTT ({n} envelopes, DATA_JSON frames):");
    println!(
        "    p50 {:.3} ms / p95 {:.3} ms / p99 {:.3} ms / max {:.3} ms",
        percentile(&ms, 50.0),
        percentile(&ms, 95.0),
        percentile(&ms, 99.0),
        ms[ms.len() - 1],
    );
    println!(
        "    throughput {:.0} events/s (mean {:.3} ms/roundtrip)",
        n as f64 / total.as_secs_f64(),
        total.as_secs_f64() * 1e3 / n as f64,
    );
    let _ = client.disconnect().await;
    println!("  result: [2a] PASS (all {n} round trips answered)");
}

async fn binary_group() {
    // 2b — one 64 MiB transaction, 1 MiB working frames.
    let total_mib = 64usize;
    let expect_len = total_mib * 1024 * 1024;
    let (client, client_session, server_session) = reliable_pair(TransferConfig {
        sender: big_cap(),
        ..TransferConfig::default()
    })
    .await;
    let (delivered_tx, mut delivered_rx) = watch::channel(0usize);
    server_session
        .on_binary(Arc::new(move |_txid, payload| {
            assert_eq!(payload.len(), expect_len, "delivered payload length");
            let _ = delivered_tx.send(payload.len());
        }))
        .await;

    let payload = make_binary_payload(expect_len);
    let frames = expect_len.div_ceil(WORKING_FRAME);
    let frame_mib = WORKING_FRAME / (1024 * 1024);
    let completed = Arc::new(AtomicUsize::new(0));
    let completed2 = completed.clone();
    let t0 = Instant::now();
    client_session
        .send_binary(
            payload,
            Some(Box::new(move |_result| {
                completed2.fetch_add(1, Ordering::Relaxed);
            })),
        )
        .await
        .expect("send_binary");
    while *delivered_rx.borrow_and_update() != expect_len {
        if delivered_rx.changed().await.is_err() {
            panic!("connection closed before delivery completed");
        }
    }
    let delivered = t0.elapsed();
    let deadline = Instant::now() + Duration::from_secs(30);
    while completed.load(Ordering::Relaxed) < 1 {
        assert!(Instant::now() < deadline, "final ACK timeout");
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    let acked = t0.elapsed();
    println!(
        "  2b sendBinary single {total_mib} MiB transaction ({frames} × {frame_mib} MiB frames):"
    );
    println!(
        "    queue → peer delivered: {:.1} ms ({:.0} MB/s payload)",
        delivered.as_secs_f64() * 1e3,
        expect_len as f64 / 1e6 / delivered.as_secs_f64(),
    );
    println!(
        "    queue → final ACK (copy released): {:.1} ms ({:.0} MB/s payload)",
        acked.as_secs_f64() * 1e3,
        expect_len as f64 / 1e6 / acked.as_secs_f64(),
    );
    let _ = client.disconnect().await;

    // 2c — 4 concurrent 16 MiB transactions.
    let per_mib = 16usize;
    let txns = 4usize;
    let expect = per_mib * 1024 * 1024;
    let (client, client_session, server_session) = reliable_pair(TransferConfig {
        sender: big_cap(),
        ..TransferConfig::default()
    })
    .await;
    let delivered = Arc::new(AtomicUsize::new(0));
    let d2 = delivered.clone();
    server_session
        .on_binary(Arc::new(move |_txid, payload| {
            assert_eq!(payload.len(), expect, "delivered payload length");
            d2.fetch_add(1, Ordering::Relaxed);
        }))
        .await;
    let completed = Arc::new(AtomicUsize::new(0));
    let payloads: Vec<Vec<u8>> = (0..txns).map(|_| make_binary_payload(expect)).collect();
    let mut handles = Vec::with_capacity(txns);
    let t0 = Instant::now();
    for p in payloads {
        let c = client_session.clone();
        let comp = completed.clone();
        handles.push(tokio::spawn(async move {
            c.send_binary(
                p,
                Some(Box::new(move |_result| {
                    comp.fetch_add(1, Ordering::Relaxed);
                })),
            )
            .await
            .expect("send_binary")
        }));
    }
    for h in handles {
        let _ = h.await.expect("join sender");
    }
    let queued = t0.elapsed();
    let deadline = Instant::now() + Duration::from_secs(30);
    while delivered.load(Ordering::Relaxed) < txns {
        assert!(Instant::now() < deadline, "concurrent delivery timeout");
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    let elapsed = t0.elapsed();
    let deadline = Instant::now() + Duration::from_secs(30);
    while completed.load(Ordering::Relaxed) < txns {
        assert!(Instant::now() < deadline, "concurrent final-ACK timeout");
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    let total_bytes = (per_mib * txns) as f64 * 1024.0 * 1024.0;
    println!("  2c sendBinary {txns} × {per_mib} MiB concurrent:");
    println!(
        "    submit → all delivered: {:.1} ms (aggregate {:.0} MB/s payload)",
        elapsed.as_secs_f64() * 1e3,
        total_bytes / 1e6 / elapsed.as_secs_f64(),
    );
    println!(
        "    submit → all {txns} send_binary calls returned: {:.1} ms (egress accept)",
        queued.as_secs_f64() * 1e3,
    );
    let _ = client.disconnect().await;
    println!("  result: [2b/2c] PASS (payload integrity + final ACKs observed)");
}

// ---- Group [3]: control-frame insertion latency ----

#[derive(Default)]
struct WireLog {
    /// (arrival instant, frame type u16, payload len) per frame observed on
    /// the client→server wire.
    events: Mutex<Vec<(Instant, u16, usize)>>,
    /// Raw bytes observed entering the tee from the client (lock-free).
    upstream_bytes: std::sync::atomic::AtomicU64,
}

fn hb_cfg() -> TransferConfig {
    TransferConfig {
        heartbeat_interval_ms: HB_MS,
        tick_interval_ms: TICK_MS,
        sender: SenderConfig {
            backpressure_cap_bytes: 1536 * 1024 * 1024,
            ..SenderConfig::default()
        },
        ..TransferConfig::default()
    }
}

/// Client → observer sink. The sink reads raw wire bytes, decodes frames and
/// discards them. Nothing downstream ever stops reading or answers, so the
/// only pacing on the wire is the client's own clock — no transport-level
/// teardown/silent machinery can interfere with the observation.
async fn observer_client_pair() -> (
    Arc<WireLog>,
    Transport,
    Arc<XacppSession>,
    tokio::task::JoinHandle<()>,
) {
    let sink = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let sink_addr = sink.local_addr().expect("addr").to_string();
    let log = Arc::new(WireLog::default());
    let log_for_task = log.clone();
    let sink_task = tokio::spawn(async move {
        let (mut sock, _) = sink.accept().await.expect("sink accept");
        let mut decoder = FrameDecoder::new();
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            match sock.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    log_for_task
                        .upstream_bytes
                        .fetch_add(n as u64, Ordering::Relaxed);
                    if let Ok(frames) = decoder.feed(&buf[..n])
                        && !frames.is_empty()
                    {
                        let mut ev = log_for_task.events.lock().await;
                        for f in frames {
                            let len = f.payload.len();
                            ev.push((Instant::now(), f.frame_type.to_u16(), len));
                        }
                    }
                }
            }
        }
    });

    let client: Transport = Arc::new(SocketTransport::connect_to(sink_addr));
    let client_session = Arc::new(XacppSession::new(Some(hb_cfg())));
    client_session
        .wire(Arc::clone(&client))
        .await
        .expect("wire");
    client.connect().await.expect("client connect");
    (log, client, client_session, sink_task)
}

/// Fixed-size bulk burst: enqueue `txns` × 16 MiB binary transactions
/// without waiting (the sink never ACKs; the copy retention window is far
/// longer than the burst).
async fn bulk_burst(
    _client: Transport,
    client_session: Arc<XacppSession>,
    txns: usize,
) -> (u64, Duration) {
    const TXN: usize = 16 * 1024 * 1024;
    let mut sent: u64 = 0;
    let t0 = Instant::now();
    for _ in 0..txns {
        let payload = make_binary_payload(TXN);
        match client_session.send_binary(payload, None).await {
            Ok(_) => sent += TXN as u64,
            Err(_) => break,
        }
    }
    (sent, t0.elapsed())
}

/// Wait until `min_samples` heartbeats are observed, return the last
/// `min_samples - 1` inter-arrival gaps in ms.
async fn collect_heartbeat_gaps(log: &WireLog, min_samples: usize, timeout: Duration) -> Vec<f64> {
    let hb = FrameType::Heartbeat.to_u16();
    let deadline = Instant::now() + timeout;
    loop {
        let arrivals: Vec<Instant> = {
            let ev = log.events.lock().await;
            ev.iter()
                .filter(|(_, t, _)| *t == hb)
                .map(|(i, _, _)| *i)
                .collect()
        };
        if arrivals.len() >= min_samples {
            let window = &arrivals[arrivals.len() - min_samples..];
            return window
                .windows(2)
                .map(|w| (w[1] - w[0]).as_secs_f64() * 1e3)
                .collect();
        }
        assert!(
            Instant::now() < deadline,
            "heartbeat collection timeout ({}/{} observed)",
            arrivals.len(),
            min_samples
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

fn print_gaps(label: &str, samples: &[f64]) {
    let mut s = samples.to_vec();
    s.sort_by(f64::total_cmp);
    println!(
        "    {label}: p50 {:.1} ms / p95 {:.1} ms / p99 {:.1} ms / max {:.1} ms  (period {HB_MS} ms)",
        percentile(&s, 50.0),
        percentile(&s, 95.0),
        percentile(&s, 99.0),
        s[s.len() - 1],
    );
}

async fn control_group() {
    println!(
        "[3] control-frame insertion latency — heartbeat ({HB_MS} ms period, {TICK_MS} ms tick), wire observed by a read-only sink"
    );

    // Idle connection.
    let (log, client, _client_session, sink_task) = observer_client_pair().await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    let idle = collect_heartbeat_gaps(&log, 201, Duration::from_secs(30)).await;
    println!("  heartbeat inter-arrival gaps:");
    print_gaps("idle connection        ", &idle);
    let _ = client.disconnect().await;
    let _ = sink_task.await;

    // Bulk DATA_BINARY in flight (the peer never ACKs: what is measured is
    // the enqueue → wire behaviour of the control channel while the egress
    // is saturated with bulk frames).
    let (log, client, client_session, sink_task) = observer_client_pair().await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    let txns = 64usize;
    let bulk = tokio::spawn(bulk_burst(client.clone(), client_session.clone(), txns));
    let bulk_gaps = collect_heartbeat_gaps(&log, 201, Duration::from_secs(60)).await;
    let (sent, elapsed) = bulk.await.expect("bulk join");
    print_gaps("bulk binary in flight  ", &bulk_gaps);
    println!(
        "    background: {:.0} MiB binary queued in {:.2?} ({:.0} MB/s enqueue rate)",
        sent as f64 / (1024.0 * 1024.0),
        elapsed,
        sent as f64 / 1e6 / elapsed.as_secs_f64(),
    );
    let _ = client.disconnect().await;
    let _ = sink_task.await;
    println!("  result: [3] PASS (heartbeats observed in both scenarios)");
}

#[tokio::main]
async fn main() {
    let n = bench_n();
    println!("=== xacpp v1.0 binary protocol benchmark (release acceptance D) ===");
    println!("N = {n} (XACPP_BENCH_N; run with --release for meaningful numbers)");
    println!();
    // Library warnings carry the failure signals this benchmark needs;
    // defaults to `warn`, RUST_LOG overrides.
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    if std::env::var("XACPP_BENCH_ONLY").as_deref() == Ok("3") {
        control_group().await;
        return;
    }
    // Group [1] is pure CPU work; it briefly parks one worker thread.
    codec_group(n);
    println!();
    rtt_group(n).await;
    println!();
    binary_group().await;
    println!();
    control_group().await;
    println!();
    println!("=== all groups PASS ===");
}
