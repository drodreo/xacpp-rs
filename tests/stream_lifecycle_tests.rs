//! Stream lifecycle tests (docs/streams.md: session-bound stream
//! lifetime — "session 不死，流不灭；流只在完成或取消时关闭").
//!
//! Red-first suite for the v3 lifecycle revision: every test pins a
//! behavior the pre-revision code fails (disconnect killed streams,
//! retention sweeps expired live transactions). Retention knobs are
//! shrunk to 300 ms so every pause/disconnect window in these tests sits
//! *beyond the old expiry window* — under the retired semantics these
//! tests fail; under the session-bound lifetime they must pass.
//!
//! Wire face is untouched: DATA_BINARY + ACK/RESUME + the two §5 control
//! frames only.

use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use tokio::io::{AsyncBufRead, AsyncWrite, AsyncWriteExt, BufReader};

use xacpp::carrier::XacppTransport;
use xacpp::carrier::stdio::StdioTransport;
use xacpp::reliability::stream::{BinaryMeta, BinaryStream, StreamHandler, StreamState};
use xacpp::reliability::transaction::SenderConfig;
use xacpp::session::XacppSession;
use xacpp::transfer::TransferConfig;
use xacpp::wire::control::{ErrorPayload, error_codes};
use xacpp::wire::frame::{Frame, FrameType};

// ---- Config ----

/// Lifecycle test config: 1 KiB frames, fast clock, generous silent
/// timeout. The historical retention knobs are gone (copy lifetime =
/// transaction lifetime): every pause/disconnect window in these tests is
/// just a window — no expiry can kill a live transaction under it.
fn lifecycle_config() -> TransferConfig {
    TransferConfig {
        sender: SenderConfig {
            frame_size: 1000,
            rto_initial_ms: 200,
            rto_max_ms: 800,
            backpressure_cap_bytes: 64 * 1024 * 1024,
        },
        heartbeat_interval_ms: 30_000,
        silent_timeout_ms: 30_000,
        ack_batch_frames: 64,
        ack_batch_interval_ms: 50,
        nack_feedback_interval_ms: 50,
        resume_verdict_timeout_ms: 300,
        tick_interval_ms: 20,
    }
}

/// Deterministic payload pattern (same shape as the reliability suite).
fn pattern(len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| ((i * 31 + (i >> 7)) & 0xff) as u8)
        .collect()
}

// ---- Memory duplex plumbing (mirrors transport_reliability_tests) ----

const DUPLEX_BUF: usize = 64 * 1024;
type BoxedWriter = Pin<Box<dyn AsyncWrite + Send>>;
type BoxedReader = Pin<Box<dyn AsyncBufRead + Send>>;

fn duplex_pair() -> (StdioTransport, StdioTransport) {
    let (a, b) = tokio::io::duplex(DUPLEX_BUF);
    let (a_read, a_write) = tokio::io::split(a);
    let (b_read, b_write) = tokio::io::split(b);
    (
        StdioTransport::new(
            Box::pin(a_write) as BoxedWriter,
            Box::pin(BufReader::new(a_read)) as BoxedReader,
        ),
        StdioTransport::new(
            Box::pin(b_write) as BoxedWriter,
            Box::pin(BufReader::new(b_read)) as BoxedReader,
        ),
    )
}

type WiredPair = (
    Arc<XacppSession>,
    Arc<XacppSession>,
    Arc<StdioTransport>,
    Arc<StdioTransport>,
);

/// Two sessions wired over one memory duplex (manual wire — the lifecycle
/// tests re-wire across connections, which the peer-driven harness cannot
/// express).
async fn wired_pair() -> WiredPair {
    let session_a = Arc::new(XacppSession::new(Some(lifecycle_config())));
    let session_b = Arc::new(XacppSession::new(Some(lifecycle_config())));
    let (ta, tb) = duplex_pair();
    let ta: Arc<StdioTransport> = Arc::new(ta);
    let tb: Arc<StdioTransport> = Arc::new(tb);
    ta.connect().await.unwrap();
    tb.connect().await.unwrap();
    session_a
        .wire(Arc::clone(&ta) as Arc<dyn XacppTransport>)
        .await
        .unwrap();
    session_b
        .wire(Arc::clone(&tb) as Arc<dyn XacppTransport>)
        .await
        .unwrap();
    (session_a, session_b, ta, tb)
}

/// Tear down one connection (both ends), like the reliability suite's
/// `resume_across_connections`.
async fn teardown(ta: &Arc<StdioTransport>, tb: &Arc<StdioTransport>) {
    ta.disconnect().await.unwrap();
    tb.disconnect().await.unwrap();
    sleep_clock_detach().await;
}

/// Give the clock task a beat to observe the closed watch and detach the
/// wiring (its tick is 20 ms; 250 ms is a wide margin).
async fn sleep_clock_detach() {
    tokio::time::sleep(Duration::from_millis(250)).await;
}

/// Re-wire both sessions onto a fresh connection. Receiver side first: its
/// wire emits the RESUME request into the duplex before the holder's
/// reader starts (same ordering discipline as `resume_across_connections`).
async fn rewire(session_a: &Arc<XacppSession>, session_b: &Arc<XacppSession>) {
    let (ta, tb) = duplex_pair();
    let ta: Arc<StdioTransport> = Arc::new(ta);
    let tb: Arc<StdioTransport> = Arc::new(tb);
    tb.connect().await.unwrap();
    session_b
        .wire(Arc::clone(&tb) as Arc<dyn XacppTransport>)
        .await
        .unwrap();
    ta.connect().await.unwrap();
    session_a
        .wire(Arc::clone(&ta) as Arc<dyn XacppTransport>)
        .await
        .unwrap();
}

/// Receiver-side stream capture: spawn a reader over the stream the moment
/// the handler fires; record handler invocation count (object identity:
/// exactly one stream object for the transaction), the bytes it saw, and
/// keep a handle to the *same* stream object for state polling.
struct StreamCapture {
    handler_calls: Arc<AtomicUsize>,
    finished: tokio::sync::mpsc::UnboundedReceiver<(Vec<u8>, usize)>,
    /// Handle to the *same* stream object for state polling. The handler is
    /// a synchronous `Fn` (it cannot await), so the slot is a watch channel:
    /// `send_replace` overwrites synchronously, `borrow` reads it
    /// synchronously — last-write-wins, exactly like the slot it replaces.
    stream_handle: tokio::sync::watch::Receiver<Option<BinaryStream>>,
}

async fn capture_stream(b: &Arc<XacppSession>) -> StreamCapture {
    let handler_calls = Arc::new(AtomicUsize::new(0));
    let (done_tx, done_rx) = tokio::sync::mpsc::unbounded_channel::<(Vec<u8>, usize)>();
    let calls = Arc::clone(&handler_calls);
    let (handle_tx, handle_rx) = tokio::sync::watch::channel(None::<BinaryStream>);
    let handler: StreamHandler = Arc::new(move |stream: BinaryStream| {
        calls.fetch_add(1, Ordering::SeqCst);
        handle_tx.send_replace(Some(stream.clone()));
        let done_tx = done_tx.clone();
        tokio::spawn(async move {
            let mut total = Vec::new();
            while let Some(chunk) = stream.read().await {
                total.extend_from_slice(&chunk.bytes);
            }
            let _ = done_tx.send((total, stream.total_len() as usize));
        });
    });
    b.on_binary_stream(handler).await;
    StreamCapture {
        handler_calls,
        finished: done_rx,
        stream_handle: handle_rx,
    }
}

const FIVE_FRAMES: usize = 5 * 1000;

// ---- L1: disconnect → stream survives → same object completes after
// rewire. Locks the single-stream-object invariant: the kernel-layer
// second-stream window (a tail-only retransmission after rewire once
// re-created a second stream object) is closed by the SPEC §3/§5 slotless
// rule — only `seq == 0` opens a slot, so a late tail frame can never
// re-create a stream. ----

#[tokio::test]
async fn l1_disconnect_stream_survives_same_object_completes_after_rewire() {
    let (a, b, ta, tb) = wired_pair().await;
    let mut capture = capture_stream(&b).await;

    let payload = pattern(FIVE_FRAMES);
    let sink = a
        .open_binary_sink(
            BinaryMeta {
                total_len: payload.len() as u32,
            },
            None,
        )
        .await
        .unwrap();
    sink.write(&payload[..2000]).await.unwrap();

    // Let the first frames arrive and the stream open on B.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        capture.handler_calls.load(Ordering::SeqCst),
        1,
        "stream handler must have fired exactly once (stream object exists)"
    );

    // Kill the connection and sit beyond the old 300 ms retention window.
    teardown(&ta, &tb).await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    // The stream object is visible and *stalled* — not dead: state() reads
    // Stalled while the transaction lives with the session (pre-revision
    // the stream was finished with Closed right at the disconnect).
    {
        let guard = capture.stream_handle.borrow();
        let stream = (*guard)
            .as_ref()
            .expect("stream object survives the disconnect");
        assert_eq!(
            stream.state(),
            StreamState::Stalled,
            "disconnect must stall, not kill"
        );
    }

    // Reconnect: SAME sessions, fresh transport. The holder declares its
    // pending send (RESUME); the receiver adjudicates it still open
    // (Resumed); the holder's gate lifts and its retained copy
    // retransmits; the sink keeps writing the tail.
    rewire(&a, &b).await;
    // Stall clears on wire-up (paused intent would be preserved too).
    assert_eq!(
        (*capture.stream_handle.borrow()).as_ref().unwrap().state(),
        StreamState::Active,
        "re-wire must clear the stall on the same object"
    );
    for chunk in payload[2000..].chunks(1000) {
        sink.write(chunk).await.unwrap();
    }
    let result = sink.finish().await.expect("finish across rewire");
    assert!(result.confirmed, "final ACK confirms after rewire");

    let (total, declared) = tokio::time::timeout(Duration::from_secs(10), capture.finished.recv())
        .await
        .expect("stream must complete within timeout")
        .expect("reader finished");
    assert_eq!(
        capture.handler_calls.load(Ordering::SeqCst),
        1,
        "exactly one stream object for the whole transaction (no re-created stream)"
    );
    assert_eq!(declared, FIVE_FRAMES);
    assert_eq!(total, payload, "same stream object delivered every byte");
}

// ---- L2: sink.write parks across disconnect, continues after rewire ----

#[tokio::test]
async fn l2_sink_write_parks_across_disconnect_and_continues() {
    let (a, b, ta, tb) = wired_pair().await;

    let (received_tx, mut received_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    {
        let received_tx = received_tx.clone();
        b.on_binary(Arc::new(move |_txid, payload| {
            let _ = received_tx.send(payload);
        }))
        .await;
    }

    let payload = pattern(FIVE_FRAMES);
    let sink = a
        .open_binary_sink(
            BinaryMeta {
                total_len: payload.len() as u32,
            },
            None,
        )
        .await
        .unwrap();
    sink.write(&payload[..1000]).await.unwrap();

    // Give the first frame a delivery window so the receiver holds an open
    // slot before the outage: a zero-progress pending send is invalidated
    // on reconnect (the receiver answers Unknown — with no tombstones it
    // cannot distinguish "never seen" from "long completed"). (on_binary
    // is the whole-buffer callback, so progress below completion is not
    // observable through it.)
    tokio::time::sleep(Duration::from_millis(300)).await;

    teardown(&ta, &tb).await;

    // Write the rest *while disconnected*: the writes must park (not fail),
    // then drain once the session is re-wired.
    let rest = payload[1000..].to_vec();
    let parked = tokio::spawn(async move {
        for chunk in rest.chunks(1000) {
            sink.write(chunk).await?;
        }
        sink.finish().await
    });

    // Sit beyond the old retention window before reconnecting.
    tokio::time::sleep(Duration::from_millis(400)).await;
    rewire(&a, &b).await;

    let result = tokio::time::timeout(Duration::from_secs(10), parked)
        .await
        .expect("parked writes must finish after rewire")
        .expect("write loop must not fail")
        .expect("finish must not fail");
    assert!(result.confirmed, "final ACK confirms after rewire");

    let mut got: Vec<u8> = Vec::new();
    while let Ok(chunk) = received_rx.try_recv() {
        got.extend_from_slice(&chunk);
    }
    assert_eq!(got, payload, "receiver got every byte despite the outage");
}

// ---- L3: sink.finish waits across disconnect for the final ACK ----

#[tokio::test]
async fn l3_sink_finish_waits_across_disconnect() {
    let (a, b, ta, tb) = wired_pair().await;

    let (received_tx, mut received_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    {
        let received_tx = received_tx.clone();
        b.on_binary(Arc::new(move |_txid, payload| {
            let _ = received_tx.send(payload);
        }))
        .await;
    }

    let payload = pattern(FIVE_FRAMES);
    let sink = a
        .open_binary_sink(
            BinaryMeta {
                total_len: payload.len() as u32,
            },
            None,
        )
        .await
        .unwrap();
    sink.write(&payload[..1000]).await.unwrap();

    // Give the first frame a delivery window so the receiver holds an open
    // slot before the outage (see l2): had every byte landed pre-teardown
    // the transaction would already be complete at the receiver, and the
    // reconnect verdict would be Unknown.
    tokio::time::sleep(Duration::from_millis(300)).await;

    teardown(&ta, &tb).await;

    // Write the rest *while disconnected* (parked), then finish(): finish
    // must wait (not fail) for the final ACK that only arrives after the
    // rewire.
    let rest = payload[1000..].to_vec();
    let finish = tokio::spawn(async move {
        for chunk in rest.chunks(1000) {
            sink.write(chunk).await?;
        }
        sink.finish().await
    });

    // Sit beyond the old retention window before reconnecting.
    tokio::time::sleep(Duration::from_millis(400)).await;
    rewire(&a, &b).await;

    let result = tokio::time::timeout(Duration::from_secs(10), finish)
        .await
        .expect("finish must resolve after rewire")
        .expect("finish task joined")
        .expect("finish must not fail");
    assert!(
        result.confirmed,
        "finish waits across the outage and confirms via final ACK"
    );

    let mut got: Vec<u8> = Vec::new();
    while let Ok(chunk) = received_rx.try_recv() {
        got.extend_from_slice(&chunk);
    }
    assert_eq!(got, payload);
}

// ---- L4: a pause longer than the old retention window stays resumable ----

#[tokio::test]
async fn l4_long_pause_beyond_old_retention_window_completes() {
    let (a, b, _ta, _tb) = wired_pair().await;

    let (received_tx, mut received_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    {
        let received_tx = received_tx.clone();
        b.on_binary(Arc::new(move |_txid, payload| {
            let _ = received_tx.send(payload);
        }))
        .await;
    }

    let payload = pattern(FIVE_FRAMES);
    let sink = a
        .open_binary_sink(
            BinaryMeta {
                total_len: payload.len() as u32,
            },
            None,
        )
        .await
        .unwrap();
    sink.write(&payload[..1000]).await.unwrap();

    // Holder pauses; the pause outlives the old 300 ms retention window.
    sink.pause().await.unwrap();
    tokio::time::sleep(Duration::from_millis(600)).await;

    // Resume and finish: the transaction must still be alive (pre-revision
    // the retention sweep tombstoned it and the remaining writes failed).
    sink.resume().await.unwrap();
    for chunk in payload[1000..].chunks(1000) {
        sink.write(chunk).await.unwrap();
    }
    let result = sink.finish().await.expect("finish after long pause");
    assert!(result.confirmed, "long-paused transaction still confirms");

    let mut got: Vec<u8> = Vec::new();
    while let Ok(chunk) = received_rx.try_recv() {
        got.extend_from_slice(&chunk);
    }
    assert_eq!(got, payload, "receiver reassembles across the long pause");
}

// ---- L5: an open stream the holder did NOT declare is cancelled on the
// RESUME declaration (cross-check, recovery discipline) ----
//
// Single-ended injection: the receiver has a partial stream open, then the
// holder's RESUME declaration arrives without that txid (the holder aborted
// or released its copy while the link was down). The open-but-undeclared
// stream can never complete now — streams close only on cancellation or
// completion, so the cross-check must cancel it (pre-revision it hung
// forever).

#[tokio::test]
async fn l5_undeclared_open_stream_is_cancelled_on_resume_declaration() {
    let (session, mut peer_write) = single_ended_session().await;
    let mut capture = capture_stream(&session).await;

    let txid = [8u8; 16];
    let frame_len = 4usize;
    let total_len = (3 * frame_len) as u32;
    feed_frame(&mut peer_write, txid, total_len, 0, &[0u8; 4]).await;
    feed_frame(&mut peer_write, txid, total_len, 1, &[1u8; 4]).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(capture.handler_calls.load(Ordering::SeqCst), 1);

    // The holder's declaration covers an unrelated transaction only — the
    // open stream's txid is NOT declared.
    let declare = xacpp::wire::control::ResumeRequestPayload {
        entries: vec![xacpp::wire::control::AckEntry {
            transaction_id: [6u8; 16],
            highest_contiguous_seq: 0,
            received_frames: 1,
        }],
    };
    let frame = Frame::new(
        FrameType::Resume,
        [0u8; 16],
        0,
        0,
        declare.encode().unwrap(),
    );
    let wire = frame.encode().unwrap();
    peer_write.write_all(&wire).await.unwrap();
    peer_write.flush().await.unwrap();

    let (total, _declared) = tokio::time::timeout(Duration::from_secs(5), capture.finished.recv())
        .await
        .expect("stream must terminate when the holder's declaration omits it (pre-revision it hung forever)")
        .expect("reader finished");
    assert_eq!(
        (*capture.stream_handle.borrow()).as_ref().unwrap().state(),
        StreamState::Cancelled,
        "an open-but-undeclared stream is cancelled by the cross-check"
    );
    assert!(
        total.len() < total_len as usize,
        "cancelled stream delivers only its partial prefix"
    );
}

// ---- L6: peer ERROR transaction_expired terminates the receiver stream ----

#[tokio::test]
async fn l6_transaction_expired_error_terminates_receiver_stream() {
    // Single-ended session with an inject writer (no peer): feed two
    // frames of a three-frame transaction to open a stream, then inject
    // the holder's ERROR transaction_expired.
    let (session, mut peer_write) = single_ended_session().await;
    let mut capture = capture_stream(&session).await;

    let txid = [9u8; 16];
    let frame_len = 4usize;
    let total_len = (3 * frame_len) as u32;
    feed_frame(&mut peer_write, txid, total_len, 0, &[0u8; 4]).await;
    feed_frame(&mut peer_write, txid, total_len, 1, &[1u8; 4]).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(capture.handler_calls.load(Ordering::SeqCst), 1);

    let payload = ErrorPayload {
        code: error_codes::TRANSACTION_EXPIRED,
        message: format!(
            "{} holder copy released",
            uuid::Uuid::from_bytes(txid).simple()
        ),
    };
    let frame = Frame::new(FrameType::Error, [0u8; 16], 0, 0, payload.encode().unwrap());
    let wire = frame.encode().unwrap();
    peer_write.write_all(&wire).await.unwrap();
    peer_write.flush().await.unwrap();

    let _ = tokio::time::timeout(Duration::from_secs(5), capture.finished.recv())
        .await
        .expect("stream must terminate on transaction_expired (pre-revision it hung forever)")
        .expect("reader finished");
    assert_eq!(
        (*capture.stream_handle.borrow()).as_ref().unwrap().state(),
        StreamState::Cancelled,
        "peer ERROR transaction_expired cancels the stream"
    );
}

// ---- L7: session death cancels non-terminal streams ----
//
// The stream lives with the session (docs/streams.md): when the last
// session handle drops, its non-terminal streams reach the `Cancelled`
// terminal state — never `Closed` (that state no longer exists).

#[tokio::test]
async fn l7_session_drop_cancels_non_terminal_streams() {
    let (session, mut peer_write) = single_ended_session().await;
    let mut capture = capture_stream(&session).await;

    let txid = [7u8; 16];
    let frame_len = 4usize;
    let total_len = (3 * frame_len) as u32;
    feed_frame(&mut peer_write, txid, total_len, 0, &[0u8; 4]).await;
    feed_frame(&mut peer_write, txid, total_len, 1, &[1u8; 4]).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(capture.handler_calls.load(Ordering::SeqCst), 1);

    // Detach first (clock aborted, wiring dropped — streams stall), then
    // drop the session: the state machine's Drop impl cancels every
    // non-terminal stream.
    session.unwire().await;
    drop(session);
    tokio::time::sleep(Duration::from_millis(200)).await;

    assert_eq!(
        (*capture.stream_handle.borrow()).as_ref().unwrap().state(),
        StreamState::Cancelled,
        "session death must cancel the surviving stream"
    );
    // The reader observed the terminal close (channel taken).
    let _ = tokio::time::timeout(Duration::from_secs(5), capture.finished.recv())
        .await
        .expect("reader must see the stream close on session drop")
        .expect("reader finished");
}

// ---- single-ended plumbing (mirrors transport_reliability_tests) ----

type InjectWriter = Pin<Box<dyn AsyncWrite + Send>>;

async fn single_ended_session() -> (Arc<XacppSession>, InjectWriter) {
    let session = Arc::new(XacppSession::new(Some(lifecycle_config())));
    let (a, b) = tokio::io::duplex(DUPLEX_BUF);
    let (_a_read, a_write) = tokio::io::split(a);
    let (_b_read, b_write) = tokio::io::split(b);
    let transport: Arc<StdioTransport> = Arc::new(StdioTransport::new(
        Box::pin(a_write) as BoxedWriter,
        Box::pin(BufReader::new(_a_read)) as BoxedReader,
    ));
    transport.connect().await.unwrap();
    session
        .wire(Arc::clone(&transport) as Arc<dyn XacppTransport>)
        .await
        .unwrap();
    (Arc::clone(&session), Box::pin(b_write) as InjectWriter)
}

async fn feed_frame(
    w: &mut (impl AsyncWrite + Unpin),
    txid: [u8; 16],
    total_len: u32,
    seq: u32,
    bytes: &[u8],
) {
    let frame = Frame::new(FrameType::DataBinary, txid, total_len, seq, bytes.to_vec());
    let wire = frame.encode().unwrap();
    w.write_all(&wire).await.unwrap();
    w.flush().await.unwrap();
}
