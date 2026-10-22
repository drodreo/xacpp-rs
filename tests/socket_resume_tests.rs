//! Socket (real TCP) recovery tests — SPEC §5 transfer recovery semantics
//! (v1.1.1 revision): disconnect detach, slotless frames, verdict
//! adjudication, declaration retransmission.
//!
//! Real TCP only: `tokio::io::duplex` dies instantly with no kernel
//! buffer, so the egress-flush window (a dead connection's queued bytes
//! keep arriving at the peer's reader) physically cannot occur there —
//! see `xacpp-tcp-resume-coverage-gap.md` for the six-link deadlock chain
//! these tests pin shut.

use std::sync::Arc;
use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::net::TcpListener;

use xacpp::carrier::XacppTransport;
use xacpp::carrier::socket::SocketTransport;
use xacpp::reliability::stream::{BinaryMeta, BinaryStream, StreamHandler};
use xacpp::session::XacppSession;
use xacpp::transfer::TransferConfig;
use xacpp::wire::frame::{FrameDecoder, FrameType};

/// Shrunk timing: 1 KiB frames, retransmission clock disabled (verdicts
/// and disconnects drive everything here), fast verdict timeout, fast
/// tick, generous silent window (the dead-connection silence must not
/// tear the session down mid-test).
fn test_config() -> TransferConfig {
    TransferConfig {
        sender: xacpp::reliability::transaction::SenderConfig {
            frame_size: 1024,
            rto_initial_ms: u64::MAX,
            rto_max_ms: u64::MAX,
            backpressure_cap_bytes: 64 * 1024 * 1024,
        },
        heartbeat_interval_ms: u64::MAX,
        silent_timeout_ms: 30_000,
        ack_batch_frames: 64,
        ack_batch_interval_ms: 50,
        nack_feedback_interval_ms: 50,
        resume_verdict_timeout_ms: 300,
        tick_interval_ms: 20,
    }
}

/// Deterministic payload pattern (not all-zero: exercises real bytes).
fn pattern(len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| ((i * 31 + (i >> 7)) & 0xff) as u8)
        .collect()
}

async fn spawn_listener() -> (TcpListener, String) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    (listener, addr)
}

/// The receiver-side deterministic disconnect scenario (SPEC §5 disconnect
/// discipline): the receiver's mount detaches at the disconnect instant,
/// so frames that keep arriving afterwards (written by the holder after
/// the disconnect, still queued in its egress) find no mount and are
/// dropped. The transfer then closes through the RESUME handshake:
/// slotless on the receiver ⇒ `Unknown` ⇒ the holder-side adjudication
/// (fully sent) ⇒ confirmed. Deterministic — the outcome does not depend
/// on how fast the re-`establish` is processed (`establish_delay`).
async fn run_receiver_disconnect_scenario(establish_delay: Duration) {
    let cfg = test_config();
    let (listener, addr) = spawn_listener().await;

    let session_a = Arc::new(XacppSession::new(Some(cfg)));
    let session_b = Arc::new(XacppSession::new(Some(test_config())));

    // Connection 1: initiator connects, receiver accepts and both wire.
    let ta1: Arc<SocketTransport> = Arc::new(SocketTransport::connect_to(addr.clone()));
    ta1.connect().await.unwrap();
    let (tb1_stream, _) = listener.accept().await.unwrap();
    let tb1: Arc<SocketTransport> = Arc::new(SocketTransport::new(tb1_stream));
    tb1.connect().await.unwrap();

    // Receiver delivery surface: the STREAM face — chunk-level delivery as
    // the contiguous prefix advances, so partial (incomplete-transaction)
    // absorption is observable. The aggregate `on_binary` face only
    // delivers a whole completed transaction.
    let delivered = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    {
        let delivered = Arc::clone(&delivered);
        let handler: StreamHandler = Arc::new(move |stream: BinaryStream| {
            let delivered = Arc::clone(&delivered);
            tokio::spawn(async move {
                while let Some(chunk) = stream.read().await {
                    delivered.fetch_add(chunk.bytes.len(), std::sync::atomic::Ordering::SeqCst);
                }
            });
        });
        session_b.on_binary_stream(handler).await;
    }

    session_b
        .wire(Arc::clone(&tb1) as Arc<dyn XacppTransport>)
        .await
        .unwrap();
    session_a
        .wire(Arc::clone(&ta1) as Arc<dyn XacppTransport>)
        .await
        .unwrap();

    // Batch 1: delivered while the connection is alive (the receiver's
    // mount is up — these bytes are assembled normally).
    let total_len: usize = 2 * 1024 * 1024;
    let first_batch: usize = 256 * 1024;
    let payload = pattern(total_len);
    let sink = session_a
        .open_binary_sink(
            BinaryMeta {
                total_len: total_len as u32,
            },
            None,
        )
        .await
        .unwrap();
    sink.write(&payload[..first_batch]).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        delivered.load(std::sync::atomic::Ordering::SeqCst),
        first_batch,
        "batch 1 must be delivered while the connection is alive"
    );

    // THE DISCONNECT: the receiver detaches its transport — per SPEC §5
    // the transfer mount detaches with it, immediately. The holder tears
    // its own transport down right after: the peer's egress flush and the
    // in-flight ACK stream die on the dead link.
    tb1.disconnect().await.unwrap();
    ta1.disconnect().await.unwrap();

    // Batch 2: written AFTER the disconnect. The frames enter the sender's
    // state machine first (sink.rs: "frames enter the state machine before
    // the egress attempt"); the egress attempt races a dead connection and
    // fails harmlessly — the transaction stays pending and fully written
    // (`finish` will declare it), with its unconfirmed frames retained.
    for chunk in payload[first_batch..].chunks(64 * 1024) {
        sink.write(chunk).await.unwrap();
    }
    // Give any in-flight bytes ample time to travel and be dropped: the
    // receiver's mount is detached, so whatever still arrives on the dead
    // connection is dropped, never routed into session state.
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert_eq!(
        delivered.load(std::sync::atomic::Ordering::SeqCst),
        first_batch,
        "frames arriving after the disconnect must be dropped by the detached mount"
    );

    // Reconnection: same sessions, fresh transport. The receiver wires
    // first (its processing may be arbitrarily slow — `establish_delay`),
    // then the holder: the RESUME declaration goes out at wire-up.
    let ta2: Arc<SocketTransport> = Arc::new(SocketTransport::connect_to(addr.clone()));
    ta2.connect().await.unwrap();
    let (tb2_stream, _) = listener.accept().await.unwrap();
    let tb2: Arc<SocketTransport> = Arc::new(SocketTransport::new(tb2_stream));
    tb2.connect().await.unwrap();
    session_b
        .wire(Arc::clone(&tb2) as Arc<dyn XacppTransport>)
        .await
        .unwrap();
    tokio::time::sleep(establish_delay).await;
    session_a
        .wire(Arc::clone(&ta2) as Arc<dyn XacppTransport>)
        .await
        .unwrap();

    // Deterministic closure: the receiver holds an OPEN INCOMPLETE slot
    // (batch 1) ⇒ verdict `Resumed` ⇒ the gate lifts and the unconfirmed
    // frames (batch 2, which never left the sender's state machine) flow
    // over the NEW connection — the lawful retransmission trigger. The
    // transaction completes on the byte count, the final ACK releases the
    // copy, and `finish` resolves confirmed — in bounded time, whatever
    // the establish speed was.
    let result = tokio::time::timeout(Duration::from_secs(5), sink.finish())
        .await
        .expect("closure within bounded time — no resume deadlock")
        .expect("finish across rewire");
    assert!(result.confirmed, "resumed transfer completes confirmed");
    // The receiver saw exactly the transaction's bytes: batch 1 while the
    // connection was alive, batch 2 through the verdict-driven
    // retransmission — nothing from the dead connection, nothing twice.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        delivered.load(std::sync::atomic::Ordering::SeqCst),
        total_len,
        "the resumed transfer delivers the full payload exactly once"
    );
}

/// Receiver-side disconnect drops every post-disconnect frame and the
/// transfer closes confirmed through RESUME (fast establish).
#[tokio::test]
async fn receiver_disconnect_drops_residual_frames_then_resume_confirms() {
    run_receiver_disconnect_scenario(Duration::from_millis(0)).await;
}

/// Same scenario with a SLOW establish: the receiver delays wiring the new
/// transport well beyond any residual-frame travel time. Pre-fix, this was
/// the racing form (establish slower than the zombie flush ⇒ deadlock);
/// post-fix the outcome is identical — the detach happened at the
/// disconnect instant, so establish speed no longer matters.
#[tokio::test]
async fn slow_establish_same_deterministic_outcome() {
    run_receiver_disconnect_scenario(Duration::from_millis(600)).await;
}

/// The report-shape deadlock scenario (holder disconnects): the holder
/// queues a full transaction and disconnects immediately — its egress
/// flushes the frames across the dying connection, the receiver (mount
/// still attached) absorbs them into a **completed-but-undelivered** slot,
/// and the final ACK dies on the dead link. On reconnect the receiver
/// must answer `Unknown` (an open incomplete slot answers `Resumed`; a
/// completed slot is indistinguishable from never-seen) and the fully-
/// sent holder closes **confirmed** — the pre-fix chain (Resumed ⇒
/// retransmission swallowed by the complete slot ⇒ holder hangs forever)
/// is pinned shut end to end on real TCP.
#[tokio::test]
async fn holder_disconnect_completed_slot_answers_unknown_then_confirms() {
    let cfg = test_config();
    let (listener, addr) = spawn_listener().await;

    let session_a = Arc::new(XacppSession::new(Some(cfg)));
    let session_b = Arc::new(XacppSession::new(Some(test_config())));

    // Connection 1.
    let ta1: Arc<SocketTransport> = Arc::new(SocketTransport::connect_to(addr.clone()));
    ta1.connect().await.unwrap();
    let (tb1_stream, _) = listener.accept().await.unwrap();
    let tb1: Arc<SocketTransport> = Arc::new(SocketTransport::new(tb1_stream));
    tb1.connect().await.unwrap();
    // No on_binary handler: the completed slot stays retained
    // (completed-but-undelivered) instead of being consumed.
    session_b
        .wire(Arc::clone(&tb1) as Arc<dyn XacppTransport>)
        .await
        .unwrap();
    session_a
        .wire(Arc::clone(&ta1) as Arc<dyn XacppTransport>)
        .await
        .unwrap();

    // The whole transaction queued at once; the disconnect follows
    // immediately — the egress flushes it across the dying connection.
    let (results_tx, mut results_rx) = tokio::sync::mpsc::unbounded_channel();
    let payload = pattern(1024 * 1024);
    let txid = session_a
        .send_binary(
            payload,
            Some(Box::new(move |result| {
                let _ = results_tx.send(result);
            })),
        )
        .await
        .unwrap();
    ta1.disconnect().await.unwrap();

    // Let the residual bytes travel and settle in the receiver's
    // completed-but-undelivered slot.
    tokio::time::sleep(Duration::from_millis(700)).await;

    // Reconnection: same sessions, fresh transport; the receiver wires
    // first, the holder's wire-up declares the pending send.
    let ta2: Arc<SocketTransport> = Arc::new(SocketTransport::connect_to(addr.clone()));
    ta2.connect().await.unwrap();
    let (tb2_stream, _) = listener.accept().await.unwrap();
    let tb2: Arc<SocketTransport> = Arc::new(SocketTransport::new(tb2_stream));
    tb2.connect().await.unwrap();
    session_b
        .wire(Arc::clone(&tb2) as Arc<dyn XacppTransport>)
        .await
        .unwrap();
    session_a
        .wire(Arc::clone(&ta2) as Arc<dyn XacppTransport>)
        .await
        .unwrap();

    // Bounded closure: Unknown verdict (completed slot — must NOT be
    // `Resumed`, which would deadlock the holder on the swallowing slot),
    // fully-sent holder ⇒ confirmed.
    let result = tokio::time::timeout(Duration::from_secs(5), results_rx.recv())
        .await
        .expect("closure within bounded time — no resume deadlock")
        .expect("completion callback fired");
    assert_eq!(result.transaction_id, txid);
    assert!(
        result.confirmed,
        "fully-sent transaction closes confirmed by holder-side adjudication"
    );
}

/// SPEC §5 (new): a RESUME declaration that stays unanswered past the
/// verdict timeout is retransmitted — the handshake is the sole recovery
/// entry, so its own reliability is mandatory. Observed on the wire: the
/// receiver side is a bare frame reader (no transfer session — every
/// declaration is silently unanswered), and the declaration must be seen
/// at least twice (initial + timeout retransmission).
#[tokio::test]
async fn resume_declaration_retransmits_on_verdict_timeout() {
    let cfg = test_config(); // resume_verdict_timeout_ms = 300
    let (listener, addr) = spawn_listener().await;

    let session_a = Arc::new(XacppSession::new(Some(cfg)));

    // Connection 1: full session pair, empty transaction set (the wire-up
    // declaration is empty — a holder with nothing pending declares
    // nothing).
    let ta1: Arc<SocketTransport> = Arc::new(SocketTransport::connect_to(addr.clone()));
    ta1.connect().await.unwrap();
    let (tb1_stream, _) = listener.accept().await.unwrap();
    let tb1: Arc<SocketTransport> = Arc::new(SocketTransport::new(tb1_stream));
    tb1.connect().await.unwrap();
    session_a
        .wire(Arc::clone(&ta1) as Arc<dyn XacppTransport>)
        .await
        .unwrap();
    drop(tb1);

    // Queue a transaction on the live connection 1, then tear it down:
    // the transaction stays pending (no ACK, retransmission disabled).
    let _txid = session_a.send_binary(pattern(4096), None).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    ta1.disconnect().await.unwrap();

    // Connection 2: the receiver side is a BARE reader — frames are
    // decoded and counted, never answered. The holder wires (wire-up
    // declaration #1 goes out) and stays gated; the verdict timeout must
    // produce declaration #2.
    let ta2: Arc<SocketTransport> = Arc::new(SocketTransport::connect_to(addr.clone()));
    ta2.connect().await.unwrap();
    let (tb2_stream, _) = listener.accept().await.unwrap();
    session_a
        .wire(Arc::clone(&ta2) as Arc<dyn XacppTransport>)
        .await
        .unwrap();

    let mut decoder = FrameDecoder::new();
    let mut buf = [0u8; 8192];
    let mut reader = tb2_stream;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut declarations = 0usize;
    while tokio::time::Instant::now() < deadline {
        let n = tokio::time::timeout(Duration::from_millis(500), reader.read(&mut buf))
            .await
            .ok()
            .transpose()
            .ok()
            .flatten()
            .unwrap_or(0);
        if n == 0 {
            continue;
        }
        for frame in decoder.feed(&buf[..n]).unwrap() {
            if frame.frame_type == FrameType::Resume {
                declarations += 1;
            }
        }
        if declarations >= 2 {
            break;
        }
    }
    assert!(
        declarations >= 2,
        "an unanswered RESUME declaration must be retransmitted past the verdict timeout (saw {declarations})"
    );
}
