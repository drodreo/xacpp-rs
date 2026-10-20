//! Fetch extension module (`xacpp::ext::fetch`) — convenience assembly over
//! the stateless fetch kernel (SPEC §8 contract firewall).
//!
//! The kernel carries no fetch state and no bytes: it defines the fetch wire
//! commands and the streaming binary primitives (`BinarySink` / `BinaryStream`).
//! This module is the library-provided assembly layer that collapses the
//! two-end glue of "one reliable fetch" into reusable pieces:
//!
//! - a-side: [`FetchPuller`] (fetch round-trip + begin authorization + txid
//!   dispatch) delivering the **bare** [`BinaryStream`] object — the state
//!   machine carrier is the kernel stream itself, never a wrapper type;
//! - b-side: [`FetchService`] (route + begin gate + idempotency tables +
//!   forced serving lifetime) handing the **bare** [`Arc<BinarySink>`] to the
//!   [`FetchProvider`];
//! - canned [`providers`] for bytes/files, and companion free functions
//!   ([`verify`] / [`pause`] / [`resume`] / [`abort`]).
//!
//! Concept red lines (project owner's standing decision): no exported type
//! named FetchClient/FetchServer; the exposed data surface is exactly the
//! kernel stream objects — this module only assembles, it never wraps.
//!
//! Lock discipline: the service tables and the puller's pending table all
//! use `tokio::sync::Mutex` with zero-await critical sections (insert /
//! remove / take are pure memory ops; every `await` happens outside the
//! lock). The sync-signature `on_binary_stream` dispatch callback hands
//! each stream to a spawned task (`dispatch_stream`), so the pending table
//! is only ever locked from async contexts.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use sha2::{Digest, Sha256};
use tokio::sync::{Notify, oneshot};

use crate::reliability::stream::{BinaryMeta, BinarySink, BinaryStream, StreamState};
use crate::session::XacppSession;
use crate::session::handler::XacppSessionHandler;
use crate::wire::commands::XacppCommand;
use crate::wire::error::XacppError;
use crate::wire::events::XacppActivityEvent;
use crate::wire::fetch::{
    FetchReadyMeta, error_codes, format_transaction_id, parse_transaction_id,
};
use crate::wire::message::XacppResponse;

// ===========================================================================
// Shared error type
// ===========================================================================

/// Extension-layer error (not part of the wire error-code registry):
/// wire `fetch_*` codes travel transparently inside [`FetchError::Refused`].
#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    /// The peer answered a fetch-family command with an error response;
    /// `code` is one of the kernel `fetch_*` codes (transparent passthrough).
    #[error("[{code}] {message}")]
    Refused { code: String, message: String },
    /// Connection / session level failure (before or during the exchange).
    #[error("transport error: {0}")]
    Transport(#[from] XacppError),
    /// Payload hash (or size) does not match the announced metadata.
    #[error(
        "integrity mismatch: expected sha256 {expected}, got {actual} ({received} bytes received)"
    )]
    IntegrityMismatch {
        expected: String,
        actual: String,
        received: u64,
    },
    /// Another pull is already registered on the same transaction id.
    /// Normal flows never hit this (ids are responder-generated per
    /// serving); it guards the pathological concurrent-pull-same-uri case.
    #[error("duplicate fetch transaction id")]
    DuplicateTxid,
    /// The transfer ended short of the announced size: the holder declared
    /// the transaction completed while bytes were still missing.
    ///
    /// The payload was never fully delivered, so there is nothing to
    /// compare a hash against — this is a truncated transfer, not a
    /// corrupted one (that is [`FetchError::IntegrityMismatch`]).
    #[error("incomplete transfer: {received} of {expected} bytes received")]
    Incomplete { received: u64, expected: u32 },
}

fn refused(code: &str, message: impl Into<String>) -> FetchError {
    FetchError::Refused {
        code: code.to_string(),
        message: message.into(),
    }
}

/// Maps a `session.fetch()` failure onto the extension error type: kernel
/// `fetch_*` codes arrive as `XacppError::Application` and are unwrapped
/// transparently; everything else is transport-level.
fn map_fetch_err(e: XacppError) -> FetchError {
    match e {
        XacppError::Application { code, message } => FetchError::Refused { code, message },
        other => FetchError::Transport(other),
    }
}

/// Maps a command response to `()` (success = any generic response) or the
/// extension error (error responses keep their wire code).
fn expect_ack(resp: XacppResponse, what: &str) -> Result<(), FetchError> {
    match resp {
        XacppResponse::Generic { .. } => Ok(()),
        XacppResponse::Error { code, message } => Err(FetchError::Refused { code, message }),
        other => Err(refused(
            error_codes::FETCH_REFUSED,
            format!("unexpected {what} response: {other:?}"),
        )),
    }
}

// ===========================================================================
// a-side: FetchPuller (dispatcher — holds no data plane)
// ===========================================================================

/// Shared dispatch state. `on_binary_stream` is a single registration slot on
/// the kernel session, so the dispatch state must outlive borrow scopes —
/// hence one `Arc` shared between the public handle and the registered
/// callback (co-owned with the session: the session holds the handler, the
/// handler holds this Arc, this Arc holds the session — both are released
/// when the consumer drops their side).
struct PullerShared {
    session: Arc<XacppSession>,
    /// txid → waiting pull. tokio Mutex (the dispatch path spawns an async
    /// delivery task — see `dispatch_stream`; the lock is never held across
    /// an await).
    pending: tokio::sync::Mutex<HashMap<[u8; 16], PendingEntry>>,
}

/// One waiting pull. `dead` is set by the pull future's drop guard (a
/// lock-free flag flip — `Drop` never coordinates); live paths sweep dead
/// entries when they meet them.
struct PendingEntry {
    sender: oneshot::Sender<BinaryStream>,
    dead: Arc<AtomicBool>,
}

/// RAII guard over one pending registration: dropping the pull future
/// (timeout / cancellation at the caller) flips the dead flag, so a later
/// stream for that txid is dropped instead of being delivered into a dead
/// receiver. The entry itself is swept lazily by the register/dispatch
/// paths — `Drop` only ever flips an atomic.
struct PendingGuard {
    dead: Arc<AtomicBool>,
    disarmed: bool,
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        if !self.disarmed {
            self.dead.store(true, Ordering::SeqCst);
        }
    }
}

fn dispatch_stream(shared: &Arc<PullerShared>, stream: BinaryStream) {
    // The handler is a sync callback; the pending table is a tokio lock.
    // Ownership of the stream moves into a runtime task — delivery is one
    // scheduler tick later, which is invisible to the awaiting pull.
    let shared = Arc::clone(shared);
    tokio::spawn(async move {
        let txid = stream.txid();
        let delivery = shared.pending.lock().await.remove(&txid);
        match delivery {
            Some(entry) => {
                if entry.dead.load(Ordering::SeqCst) {
                    // The puller gave up (guard fired): drop the stream,
                    // never deliver into a dead receiver.
                    log::debug!(
                        "fetch puller: stream {} met a dead pending entry — dropped",
                        format_transaction_id(txid)
                    );
                    return;
                }
                if entry.sender.send(stream).is_err() {
                    // Unreachable in normal flow (the guard marks the entry
                    // before its receiver can be dropped); defensive passthrough.
                    log::warn!(
                        "fetch puller: pending receiver gone for txid {}",
                        format_transaction_id(txid)
                    );
                }
            }
            None => {
                // No waiting pull: 1.1 has no push, so an inbound stream
                // outside the fetch convention is a peer violation or a late
                // replay of a terminal transaction — dropped, never delivered.
                log::debug!(
                    "fetch puller: stream {} has no waiting pull — dropped",
                    format_transaction_id(txid)
                );
            }
        }
    });
}

/// a-side assembler: owns the session's single `on_binary_stream` slot and
/// routes arriving streams to waiting [`FetchPuller::pull`] calls by txid;
/// streams no pull is waiting on are dropped (1.1 has no push).
/// Data is always delivered as the bare kernel [`BinaryStream`].
pub struct FetchPuller {
    shared: Arc<PullerShared>,
}

impl FetchPuller {
    /// Creates the puller and takes over the session's stream registration.
    pub async fn new(session: Arc<XacppSession>) -> Self {
        let shared = Arc::new(PullerShared {
            session,
            pending: tokio::sync::Mutex::new(HashMap::new()),
        });
        let for_handler = Arc::clone(&shared);
        shared
            .session
            .on_binary_stream(Arc::new(move |stream: BinaryStream| {
                dispatch_stream(&for_handler, stream);
            }))
            .await;
        Self { shared }
    }

    /// Performs one fetch: metadata round-trip → pending registration →
    /// begin authorization → suspend until the stream arrives (no timeout:
    /// during a disconnect the pending entry survives and the first frame
    /// after re-wiring delivers — "session lives, stream lives"). Cancellation
    /// is the caller dropping this future (the pending entry is removed by
    /// the drop guard).
    ///
    /// Returns the **bare** kernel stream object plus the announced metadata.
    pub async fn pull(&self, uri: &str) -> Result<(BinaryStream, FetchReadyMeta), FetchError> {
        // 1. Metadata (wire `fetch_*` error codes pass through as Refused).
        let meta = self
            .shared
            .session
            .fetch(uri)
            .await
            .map_err(map_fetch_err)?;
        // 2. Register pending (duplicate txid = pathological concurrent pull
        //    on the same serving — fail the latecomer, leave the first intact).
        //    Registration also sweeps entries whose puller gave up (the drop
        //    guard's dead flag) — pull rates are low, an O(n) sweep is fine.
        let (tx, rx) = oneshot::channel();
        let dead = Arc::new(AtomicBool::new(false));
        {
            let mut pending = self.shared.pending.lock().await;
            pending.retain(|_, entry| !entry.dead.load(Ordering::SeqCst));
            if pending.contains_key(&meta.txid) {
                return Err(FetchError::DuplicateTxid);
            }
            pending.insert(
                meta.txid,
                PendingEntry {
                    sender: tx,
                    dead: Arc::clone(&dead),
                },
            );
        }
        let mut guard = PendingGuard {
            dead,
            disarmed: false,
        };
        // 3. Begin authorization (the responder's gate: no frames before this).
        let resp = self
            .shared
            .session
            .request_command(XacppCommand::FetchBegin {
                txid: format_transaction_id(meta.txid),
            })
            .await;
        expect_ack(resp?, "fetch_begin")?;
        // 4. Suspend until the stream object arrives (kernel absorbs
        //    disconnects; delivery happens on the first frame after re-wire).
        let stream = rx
            .await
            .map_err(|_| FetchError::Transport(XacppError::Closed))?;
        // 5. Delivered — disarm the guard (normal completion).
        guard.disarmed = true;
        Ok((stream, meta))
    }

    /// Convenience: `pull` + read-to-end + rolling sha256 verification.
    /// Returns the collected bytes and the announced metadata.
    pub async fn pull_to_vec(&self, uri: &str) -> Result<(Vec<u8>, FetchReadyMeta), FetchError> {
        let (stream, meta) = self.pull(uri).await?;
        let mut hasher = Sha256::new();
        let mut received: u64 = 0;
        let mut collected = Vec::with_capacity(meta.size_bytes as usize);
        while let Some(chunk) = stream.read().await {
            hasher.update(&chunk.bytes);
            received += chunk.bytes.len() as u64;
            collected.extend_from_slice(&chunk.bytes);
        }
        // The stream ended. If it ended short of the announced size, the
        // bytes were never all delivered: the hash comparison has nothing
        // to compare and must not report a mismatch — the transfer was
        // truncated, not corrupted.
        if let Some(detail) = stream.incomplete_detail() {
            return Err(FetchError::Incomplete {
                received: detail.received,
                expected: detail.expected,
            });
        }
        // Any other non-complete terminal state (e.g. Cancelled after the
        // holder's short-write ABORT) likewise means the bytes were never
        // all delivered: report the truncation, never an integrity verdict.
        if stream.state() != StreamState::Complete {
            return Err(FetchError::Incomplete {
                received,
                expected: meta.size_bytes as u32,
            });
        }
        let actual = format!("{:x}", hasher.finalize());
        verify(&meta, &actual).map_err(|e| match e {
            FetchError::IntegrityMismatch {
                expected, actual, ..
            } => FetchError::IntegrityMismatch {
                expected,
                actual,
                received,
            },
            other => other,
        })?;
        Ok((collected, meta))
    }
}

// ===========================================================================
// Companion free functions (verification + companion commands)
// ===========================================================================

/// Compares a stream consumer's self-computed sha256 hex against the
/// announced metadata. Integrity is an application-layer promise (§8.5):
/// the extension only provides the comparison piece. On mismatch the
/// `received` byte count is reported as 0 — the streaming consumer tracks
/// its own byte count (`pull_to_vec` fills the real value internally).
pub fn verify(meta: &FetchReadyMeta, actual_sha256_hex: &str) -> Result<(), FetchError> {
    if meta.sha256 == actual_sha256_hex {
        Ok(())
    } else {
        Err(FetchError::IntegrityMismatch {
            expected: meta.sha256.clone(),
            actual: actual_sha256_hex.to_string(),
            received: 0,
        })
    }
}

/// Companion flow control: asks the holder to pause the named transaction.
pub async fn pause(session: &XacppSession, txid: [u8; 16]) -> Result<(), FetchError> {
    let resp = session
        .request_command(XacppCommand::FetchPause {
            txid: format_transaction_id(txid),
        })
        .await?;
    expect_ack(resp, "fetch_pause")
}

/// Companion flow control: asks the holder to resume the named transaction.
pub async fn resume(session: &XacppSession, txid: [u8; 16]) -> Result<(), FetchError> {
    let resp = session
        .request_command(XacppCommand::FetchResume {
            txid: format_transaction_id(txid),
        })
        .await?;
    expect_ack(resp, "fetch_resume")
}

/// Companion command: stop and release the named fetch transaction on the
/// holder. Idempotent — a failed command (unknown/completed transaction,
/// dead connection) is logged, never surfaced.
pub async fn abort(session: &XacppSession, txid: [u8; 16]) {
    let outcome = match session
        .request_command(XacppCommand::FetchAbort {
            txid: format_transaction_id(txid),
        })
        .await
    {
        Ok(resp) => expect_ack(resp, "fetch_abort"),
        Err(e) => Err(FetchError::Transport(e)),
    };
    if let Err(e) = outcome {
        log::debug!("fetch::abort: idempotent, command outcome ignored: {e}");
    }
}

// ===========================================================================
// b-side: FetchProvider (two-phase) + FetchService
// ===========================================================================

/// Metadata a provider must resolve up front (SPEC §8.2: sha256 MUST be
/// ready when `fetch_ready` is answered — the resolve phase is mandatory).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedResource {
    /// Payload size in bytes (must be non-zero and ≤ u32::MAX — the wire
    /// frame header carries total_len as u32).
    pub size_bytes: u64,
    /// sha256 hex (lowercase) of the bytes that will be served.
    pub sha256: String,
    /// MIME type declared for the resource.
    pub mime: String,
}

/// b-side provider, two-phase:
///
/// - [`resolve`](Self::resolve) runs when a `fetch{uri}` arrives and must
///   produce the announced metadata (errors: `Application{code}` is passed
///   through to the requester, anything else maps to `fetch_not_found`);
/// - [`serve`](Self::serve) runs once `fetch_begin` authorizes transmission:
///   write the payload into the **real** kernel sink (`Arc<BinarySink>` —
///   write/backpressure/pause semantics are native). Return `Ok` and the
///   extension finishes the transaction; return `Err` (or get cancelled)
///   and the extension aborts it. Providers must never call
///   `finish()`/`abort()` themselves — the serving lifetime is forced here.
#[async_trait]
pub trait FetchProvider: Send + Sync {
    async fn resolve(&self, uri: &str) -> Result<ResolvedResource, XacppError>;
    async fn serve(&self, uri: &str, sink: Arc<BinarySink>) -> Result<(), XacppError>;
}

/// Internal control channel (extension-private; providers never see it).
#[derive(Debug)]
enum ServeControl {
    Pause,
    Resume,
    Abort,
}

/// One serving transaction: the responder-side record tying the announced
/// metadata to the real sink. Lives in the idempotency tables until the
/// serving task reaches a terminal state (or an abort removes it early).
struct Serving {
    uri: String,
    txid: [u8; 16],
    meta: FetchReadyMeta,
    sink: Arc<BinarySink>,
    control_tx: tokio::sync::mpsc::UnboundedSender<ServeControl>,
    /// Taken exactly once by the serving task at `fetch_begin` (tokio mutex
    /// for interior mutability inside the `Arc` — the take is a pure memory
    /// op; it happens outside the service-state lock so no await runs in a
    /// critical section).
    control_rx: tokio::sync::Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<ServeControl>>>,
    /// Begin gate (CAS under the table lock — pure memory critical section).
    begun: AtomicBool,
}

/// `by_uri` entry: either a resolving placeholder (concurrent same-uri
/// fetches wait on it — resolve runs exactly once) or the ready serving.
enum UriEntry {
    Resolving(Arc<ResolveSlot>),
    Ready(Arc<Serving>),
}

/// Resolve-phase rendezvous: the resolver fills `result` then notifies;
/// waiters poll `result` and park on `notify` in between (notify permits
/// cover the check-then-park race).
struct ResolveSlot {
    notify: Notify,
    result: tokio::sync::Mutex<Option<Result<Arc<Serving>, XacppError>>>,
}

impl ResolveSlot {
    async fn wait(&self) -> Result<Arc<Serving>, XacppError> {
        loop {
            if let Some(res) = self.result.lock().await.as_ref() {
                return match res {
                    Ok(s) => Ok(Arc::clone(s)),
                    Err(e) => Err(e.clone()),
                };
            }
            self.notify.notified().await;
        }
    }
}

#[derive(Default)]
struct ServiceState {
    by_uri: HashMap<String, UriEntry>,
    by_txid: HashMap<[u8; 16], Arc<Serving>>,
}

/// b-side assembler implementing [`XacppSessionHandler`]: routes fetch-family
/// commands, enforces the begin gate (no frame leaves before `fetch_begin`),
/// keeps the uri/txid idempotency tables, and owns the forced serving
/// lifetime (finish on provider success, abort on provider failure or
/// cancellation). Non-fetch commands/events pass through to `inner`.
pub struct FetchService {
    provider: Arc<dyn FetchProvider>,
    inner: Option<Arc<dyn XacppSessionHandler>>,
    state: Arc<tokio::sync::Mutex<ServiceState>>,
    session: Arc<XacppSession>,
}

/// Removes one serving from both tables (idempotent no-op when absent or
/// already superseded by a newer serving for the same uri).
async fn remove_serving(state: &tokio::sync::Mutex<ServiceState>, txid: [u8; 16]) {
    let mut st = state.lock().await;
    if let Some(s) = st.by_txid.remove(&txid)
        && matches!(st.by_uri.get(&s.uri), Some(UriEntry::Ready(s2)) if s2.txid == txid)
    {
        // Only evict the uri entry if it still points at this transaction
        // (a late removal must not kill a newer serving on the same uri).
        st.by_uri.remove(&s.uri);
    }
}

/// Serving task: runs the provider's `serve` against the real sink while
/// multiplexing the control channel; forces the terminal operation.
/// Terminal state (finish/abort) and table eviction always happen here —
/// the provider cannot end the transaction, and every path (provider Ok,
/// provider Err, panic/cancellation, Abort control, control-channel death)
/// lands in exactly one terminal op followed by eviction.
async fn run_serving(
    state: Arc<tokio::sync::Mutex<ServiceState>>,
    provider: Arc<dyn FetchProvider>,
    uri: String,
    txid: [u8; 16],
    sink: Arc<BinarySink>,
    mut control_rx: tokio::sync::mpsc::UnboundedReceiver<ServeControl>,
) {
    let serve_provider = Arc::clone(&provider);
    let serve_sink = Arc::clone(&sink);
    let serve_uri = uri.clone();
    let mut join = tokio::spawn(async move { serve_provider.serve(&serve_uri, serve_sink).await });
    let mut aborted = false;
    loop {
        tokio::select! {
            ctrl = control_rx.recv() => match ctrl {
                Some(ServeControl::Pause) => {
                    if let Err(e) = sink.pause().await {
                        log::warn!("fetch serving: sink pause failed (best effort): {e}");
                    }
                }
                Some(ServeControl::Resume) => {
                    if let Err(e) = sink.resume().await {
                        log::warn!("fetch serving: sink resume failed (best effort): {e}");
                    }
                }
                // Abort command (or defensive channel death): kill the serve
                // task, wait for it to release its sink reference, then abort.
                Some(ServeControl::Abort) | None => {
                    join.abort();
                    let _ = join.await;
                    aborted = true;
                    break;
                }
            },
            done = &mut join => {
                match done {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => {
                        log::warn!("fetch serving: provider serve failed → abort: {e}");
                        aborted = true;
                    }
                    Err(e) => {
                        log::warn!("fetch serving: serve task cancelled/panicked → abort: {e}");
                        aborted = true;
                    }
                }
                break;
            }
        }
    }
    // Evict from the tables so the service's Arc is released and the sink
    // becomes singly-owned by this task.
    remove_serving(&state, txid).await;
    match Arc::try_unwrap(sink) {
        Ok(sink) => {
            let result = if aborted {
                sink.abort().await
            } else {
                sink.finish().await.map(|r| {
                    log::debug!("fetch serving: finished, confirmed={}", r.confirmed);
                })
            };
            if let Err(e) = result {
                log::warn!("fetch serving: terminal op failed: {e}");
            }
        }
        Err(_) => {
            // Unreachable: the table eviction above released the service's
            // reference and the (already-dead) serve task released its own.
            log::error!("fetch serving: sink still shared at terminal state — terminal op skipped");
        }
    }
}

fn ready_response(meta: &FetchReadyMeta) -> XacppResponse {
    XacppResponse::FetchReady {
        txid: format_transaction_id(meta.txid),
        sha256: meta.sha256.clone(),
        size_bytes: meta.size_bytes,
        mime: meta.mime.clone(),
    }
}

/// Maps a provider/build failure onto the wire response: `Application`
/// errors pass through with their declared code, anything else (including
/// infrastructure failures) maps to `fetch_not_found` per the kernel shell
/// convention.
fn error_response(e: &XacppError) -> XacppResponse {
    match e {
        XacppError::Application { code, message } => XacppResponse::Error {
            code: code.clone(),
            message: message.clone(),
        },
        other => XacppResponse::Error {
            code: error_codes::FETCH_NOT_FOUND.into(),
            message: other.to_string(),
        },
    }
}

fn ack() -> XacppResponse {
    XacppResponse::Generic {
        name: "acknowledge".into(),
        data: serde_json::Value::Null,
    }
}

impl FetchService {
    /// Creates the service. `inner` (optional) receives every non-fetch
    /// command and every event; without it, unknown commands answer
    /// `unknown_command`.
    pub fn new(
        session: Arc<XacppSession>,
        provider: Arc<dyn FetchProvider>,
        inner: Option<Arc<dyn XacppSessionHandler>>,
    ) -> Self {
        Self {
            provider,
            inner,
            state: Arc::new(tokio::sync::Mutex::new(ServiceState::default())),
            session,
        }
    }

    /// Opens the sink and builds the serving record (no frames sent — the
    /// begin gate keeps the pump out until `fetch_begin`).
    async fn build_serving(
        &self,
        uri: &str,
        resolved: ResolvedResource,
    ) -> Result<Arc<Serving>, XacppError> {
        if resolved.size_bytes == 0 {
            return Err(XacppError::Application {
                code: error_codes::FETCH_REFUSED.into(),
                message: "resource size must be non-zero".into(),
            });
        }
        if resolved.size_bytes > u32::MAX as u64 {
            return Err(XacppError::Application {
                code: error_codes::FETCH_REFUSED.into(),
                message: "resource exceeds protocol size limit (u32::MAX)".into(),
            });
        }
        let sink = self
            .session
            .open_binary_sink(
                BinaryMeta {
                    total_len: resolved.size_bytes as u32,
                },
                None,
            )
            .await?;
        let txid = sink.txid();
        let (control_tx, control_rx) = tokio::sync::mpsc::unbounded_channel();
        let meta = FetchReadyMeta {
            txid,
            sha256: resolved.sha256,
            size_bytes: resolved.size_bytes,
            mime: resolved.mime,
        };
        Ok(Arc::new(Serving {
            uri: uri.to_string(),
            txid,
            meta,
            sink: Arc::new(sink),
            control_tx,
            control_rx: tokio::sync::Mutex::new(Some(control_rx)),
            begun: AtomicBool::new(false),
        }))
    }

    /// `fetch{uri}`: idempotent metadata (ready entries and concurrent
    /// resolving placeholders answer with the same meta; resolve runs
    /// exactly once per uri).
    async fn handle_fetch(&self, uri: String) -> Result<XacppResponse, XacppError> {
        // Fast path: ready entry answers immediately.
        {
            let st = self.state.lock().await;
            match st.by_uri.get(&uri) {
                Some(UriEntry::Ready(s)) => return Ok(ready_response(&s.meta)),
                Some(UriEntry::Resolving(slot)) => {
                    // Slow path: another fetch is already resolving — wait
                    // for its outcome and answer identically (lock released
                    // before the wait).
                    let slot = Arc::clone(slot);
                    drop(st);
                    return match slot.wait().await {
                        Ok(s) => Ok(ready_response(&s.meta)),
                        Err(e) => Ok(error_response(&e)),
                    };
                }
                None => {}
            }
        }
        // Miss: insert the placeholder, then resolve outside the lock so
        // concurrent same-uri fetches serialize behind this slot.
        let slot = Arc::new(ResolveSlot {
            notify: Notify::new(),
            result: tokio::sync::Mutex::new(None),
        });
        {
            let mut st = self.state.lock().await;
            // Re-check under the write lock (two missers can race here).
            match st.by_uri.get(&uri) {
                Some(UriEntry::Ready(s)) => return Ok(ready_response(&s.meta)),
                Some(UriEntry::Resolving(slot)) => {
                    let slot = Arc::clone(slot);
                    drop(st);
                    return match slot.wait().await {
                        Ok(s) => Ok(ready_response(&s.meta)),
                        Err(e) => Ok(error_response(&e)),
                    };
                }
                None => {
                    st.by_uri
                        .insert(uri.clone(), UriEntry::Resolving(Arc::clone(&slot)));
                }
            }
        }
        // Resolve + build (awaits outside the table lock).
        let outcome: Result<Arc<Serving>, XacppError> = match self.provider.resolve(&uri).await {
            Ok(resolved) => self.build_serving(&uri, resolved).await,
            Err(e) => Err(e),
        };
        // Fill the result, swap the placeholder for the ready entry (or evict
        // it on failure), then wake the waiters. Note: a failed resolve is
        // still a *response* (wire error), not an `Err` return.
        let response = {
            let mut st = self.state.lock().await;
            match &outcome {
                Ok(s) => {
                    st.by_uri
                        .insert(uri.clone(), UriEntry::Ready(Arc::clone(s)));
                    st.by_txid.insert(s.txid, Arc::clone(s));
                    Ok(ready_response(&s.meta))
                }
                Err(e) => {
                    // Only evict our own placeholder.
                    if matches!(st.by_uri.get(&uri), Some(UriEntry::Resolving(existing)) if Arc::ptr_eq(existing, &slot))
                    {
                        st.by_uri.remove(&uri);
                    }
                    Ok(error_response(e))
                }
            }
        };
        {
            let mut result = slot.result.lock().await;
            *result = Some(match &outcome {
                Ok(s) => Ok(Arc::clone(s)),
                Err(e) => Err(e.clone()),
            });
        }
        slot.notify.notify_waiters();
        response
    }

    /// `fetch_begin{txid}`: the gate. Unknown/malformed → `fetch_refused`;
    /// already begun → success (idempotent); first begin flips the gate and
    /// spawns the serving task (provider.serve against the real sink +
    /// control multiplexing + forced terminal op).
    async fn handle_begin(&self, txid_hex: &str) -> Result<XacppResponse, XacppError> {
        let Ok(txid) = parse_transaction_id(txid_hex) else {
            return Ok(XacppResponse::error(
                error_codes::FETCH_REFUSED,
                format!("malformed fetch transaction id: {txid_hex}"),
            ));
        };
        // Take the gate under the state lock, then take the spawn materials
        // with the lock released: the `begun` CAS is the single-take gate
        // (a second begin sees `begun == true` and returns the idempotent
        // ack), so the control channel can be taken outside the critical
        // section — no await runs while the state lock is held. After the
        // gate flip the serving can only leave the tables via this
        // transaction's own terminal path (or an abort, which hands the
        // terminal op to the serving task through the same control
        // channel) — no window between the gate and the spawn.
        let serving = {
            let st = self.state.lock().await;
            let Some(s) = st.by_txid.get(&txid).cloned() else {
                return Ok(XacppResponse::error(
                    error_codes::FETCH_REFUSED,
                    "unknown fetch transaction",
                ));
            };
            if s.begun
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_err()
            {
                return Ok(ack()); // already begun — idempotent success
            }
            s
        };
        let uri = serving.uri.clone();
        let sink = Arc::clone(&serving.sink);
        let Some(rx) = serving.control_rx.lock().await.take() else {
            // Unreachable: the `begun` CAS above grants exactly one taker.
            // Degrade to a refusal instead of panicking.
            log::error!("fetch serving: control channel already taken for txid {txid_hex}");
            return Ok(XacppResponse::error(
                error_codes::FETCH_REFUSED,
                format!("fetch serving: control channel already taken for {txid_hex}"),
            ));
        };
        // Spawn outside the lock.
        tokio::spawn(run_serving(
            Arc::clone(&self.state),
            Arc::clone(&self.provider),
            uri,
            txid,
            sink,
            rx,
        ));
        Ok(ack())
    }

    /// `fetch_pause`/`fetch_resume{txid}`: forward to the serving task's
    /// control channel (the sink itself is owned by the serving task).
    async fn handle_flow(
        &self,
        txid_hex: &str,
        control: ServeControl,
        what: &str,
    ) -> Result<XacppResponse, XacppError> {
        let Ok(txid) = parse_transaction_id(txid_hex) else {
            return Ok(XacppResponse::error(
                error_codes::FETCH_REFUSED,
                format!("malformed fetch transaction id: {txid_hex}"),
            ));
        };
        let st = self.state.lock().await;
        match st.by_txid.get(&txid) {
            Some(s) => {
                let _ = s.control_tx.send(control);
                Ok(ack())
            }
            None => Ok(XacppResponse::error(
                error_codes::FETCH_REFUSED,
                format!("{what}: unknown fetch transaction"),
            )),
        }
    }

    /// `fetch_abort{txid}`: idempotent — unknown/completed transactions
    /// answer success. Known-not-begun: evict and abort the sink directly
    /// (no serving task exists). Known-begun: evict + signal the serving
    /// task, which kills the provider task and aborts the sink.
    async fn handle_abort(&self, txid_hex: &str) -> Result<XacppResponse, XacppError> {
        let Ok(txid) = parse_transaction_id(txid_hex) else {
            // Malformed ids address nothing — idempotent success.
            return Ok(ack());
        };
        let serving = {
            let mut st = self.state.lock().await;
            let s = st.by_txid.remove(&txid);
            if let Some(s) = &s
                && matches!(st.by_uri.get(&s.uri), Some(UriEntry::Ready(s2)) if s2.txid == txid)
            {
                st.by_uri.remove(&s.uri);
            }
            s
        };
        let Some(s) = serving else {
            return Ok(ack()); // unknown/completed — idempotent
        };
        if !s.begun.swap(true, Ordering::SeqCst) {
            // Not begun: no serving task owns the sink — abort it here.
            let sink = Arc::clone(&s.sink);
            drop(s);
            match Arc::try_unwrap(sink) {
                Ok(sink) => {
                    if let Err(e) = sink.abort().await {
                        log::warn!("fetch abort: sink abort failed: {e}");
                    }
                }
                Err(_) => {
                    log::error!("fetch abort: not-begun sink still shared — abort skipped");
                }
            }
        } else {
            // Begun: the serving task owns the terminal op.
            let _ = s.control_tx.send(ServeControl::Abort);
        }
        Ok(ack())
    }
}

#[async_trait]
impl XacppSessionHandler for FetchService {
    async fn on_command(&self, command: XacppCommand) -> Result<XacppResponse, XacppError> {
        match command {
            XacppCommand::Fetch { uri } => self.handle_fetch(uri).await,
            XacppCommand::FetchBegin { txid } => self.handle_begin(&txid).await,
            XacppCommand::FetchPause { txid } => {
                self.handle_flow(&txid, ServeControl::Pause, "fetch_pause")
                    .await
            }
            XacppCommand::FetchResume { txid } => {
                self.handle_flow(&txid, ServeControl::Resume, "fetch_resume")
                    .await
            }
            XacppCommand::FetchAbort { txid } => self.handle_abort(&txid).await,
            other => match &self.inner {
                Some(inner) => inner.on_command(other).await,
                None => Err(XacppError::Application {
                    code: "unknown_command".into(),
                    message: "no inner handler registered".into(),
                }),
            },
        }
    }

    async fn on_event(&self, event: XacppActivityEvent) -> Result<XacppResponse, XacppError> {
        match &self.inner {
            Some(inner) => inner.on_event(event).await,
            None => Ok(ack()),
        }
    }

    /// Loading a `FetchService` on the session IS the fetch responder
    /// registration (SPEC §8.2 gate: fetch-family commands then reach
    /// `on_command` instead of being answered `fetch_unavailable`).
    fn is_fetch_responder(&self) -> bool {
        true
    }
}

// ===========================================================================
// Canned providers
// ===========================================================================

/// Chunk size used by the canned providers (64 KiB — large enough to amortize
/// write overhead, small enough for responsive flow control).
const CANNED_CHUNK: usize = 64 * 1024;

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// In-memory resource: resolve computes sha256+len directly; serve writes the
/// bytes through the real sink in chunks.
pub fn bytes(data: Vec<u8>, mime: &str) -> Arc<dyn FetchProvider> {
    Arc::new(BytesProvider {
        data,
        mime: mime.to_string(),
    })
}

struct BytesProvider {
    data: Vec<u8>,
    mime: String,
}

#[async_trait]
impl FetchProvider for BytesProvider {
    async fn resolve(&self, _uri: &str) -> Result<ResolvedResource, XacppError> {
        Ok(ResolvedResource {
            size_bytes: self.data.len() as u64,
            sha256: sha256_hex(&self.data),
            mime: self.mime.clone(),
        })
    }

    async fn serve(&self, _uri: &str, sink: Arc<BinarySink>) -> Result<(), XacppError> {
        for chunk in self.data.chunks(CANNED_CHUNK) {
            sink.write(chunk).await?;
        }
        Ok(())
    }
}

/// File resource: resolve pre-scans the file streaming (never loads it whole)
/// for sha256+size; serve re-reads and writes in 64 KiB chunks.
///
/// `tokio`'s `fs` feature is not part of this crate's dependency set, so the
/// blocking reads run on `spawn_blocking` (std::fs handles are `Send`).
pub fn file(path: PathBuf, mime: &str) -> Arc<dyn FetchProvider> {
    Arc::new(FileProvider {
        path,
        mime: mime.to_string(),
    })
}

struct FileProvider {
    path: PathBuf,
    mime: String,
}

/// One blocking read pass: returns the filled prefix of `buf`.
/// `file` moves in and out so the caller can keep reading.
fn read_chunk_blocking(
    mut file: std::fs::File,
    mut buf: Vec<u8>,
) -> std::io::Result<(std::fs::File, Vec<u8>, usize)> {
    let n = file.read(&mut buf)?;
    buf.truncate(n);
    Ok((file, buf, n))
}

/// Streaming pre-scan: sha256 + byte count in one pass (spawn_blocking).
async fn prescan_file(path: &Path) -> Result<(String, u64), XacppError> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let mut file = std::fs::File::open(&path)
            .map_err(|e| XacppError::Internal(format!("fetch file open failed: {e}")))?;
        let mut hasher = Sha256::new();
        let mut size: u64 = 0;
        let mut buf = vec![0u8; CANNED_CHUNK];
        loop {
            let n = file
                .read(&mut buf)
                .map_err(|e| XacppError::Internal(format!("fetch file read failed: {e}")))?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            size += n as u64;
        }
        Ok((format!("{:x}", hasher.finalize()), size))
    })
    .await
    .map_err(|e| XacppError::Internal(format!("fetch prescan join failed: {e}")))?
}

#[async_trait]
impl FetchProvider for FileProvider {
    async fn resolve(&self, _uri: &str) -> Result<ResolvedResource, XacppError> {
        let (sha256, size_bytes) = prescan_file(&self.path).await?;
        Ok(ResolvedResource {
            size_bytes,
            sha256,
            mime: self.mime.clone(),
        })
    }

    async fn serve(&self, _uri: &str, sink: Arc<BinarySink>) -> Result<(), XacppError> {
        let path = self.path.clone();
        let mut file = tokio::task::spawn_blocking(move || std::fs::File::open(&path))
            .await
            .map_err(|e| XacppError::Internal(format!("fetch serve join failed: {e}")))?
            .map_err(|e| XacppError::Internal(format!("fetch file open failed: {e}")))?;
        loop {
            // Blocking read off the async path; the sink write itself is async
            // (backpressure/pause park inside the kernel).
            let (f, buf, n) = tokio::task::spawn_blocking(move || {
                read_chunk_blocking(file, vec![0u8; CANNED_CHUNK])
            })
            .await
            .map_err(|e| XacppError::Internal(format!("fetch serve join failed: {e}")))?
            .map_err(|e| XacppError::Internal(format!("fetch file read failed: {e}")))?;
            file = f;
            if n == 0 {
                break;
            }
            sink.write(&buf).await?;
        }
        Ok(())
    }
}
