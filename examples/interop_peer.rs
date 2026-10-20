//! XACPP interoperability peer (xacpp-rs) — the contract counterpart for
//! xacpp-ts cross-library tests.
//!
//! Behavior (stability first, deliberately simple):
//!
//! - CLI: `interop_peer [--fetch-serve <dir> | --fetch-pull <uri>] --port N`;
//!   binds `127.0.0.1:N` (TCP).
//! - Logs go to **stderr only** — stdout stays clean for harness use.
//! - After the listener is bound, prints `READY port=N` on stderr; the
//!   harness waits for this line before connecting.
//! - Default (no fetch flag) — envelope echo service driven directly on the
//!   transport (legacy shape, kept byte-compatible so the non-fetch interop
//!   suite keeps passing against this binary):
//!   - `Negotiate` → `Negotiated` echoing the initiator's capabilities;
//!   - `Establish` / `EstablishConfirm` → `Established` (`interop` /
//!     `interop-creds`);
//!   - any other command → `Generic` response echoing `name` with the
//!     command arguments as `data`;
//!   - every received event prints `EVENT name=<name> n=<count>` on stderr
//!     (count starts at 1) and is acknowledged;
//!   - every fully reassembled `DATA_BINARY` transaction is echoed back
//!     with the same payload via `send_binary`;
//!   - an `XacppSession` is wired (default config: heartbeats every 30 s,
//!     silent timeout 90 s), so ACK/NACK/RESUME and bidirectional
//!     heartbeats work.
//! - `--fetch-serve <dir>` — full `XacppPeer` responder stack: answers
//!   `fetch(uri)` with the bytes of `<dir>/<uri>` (mime
//!   `application/octet-stream`), identity `interop`/`interop-creds`.
//! - `--fetch-pull <uri>` — full `XacppPeer` responder stack: once the
//!   handshake completes, pulls `<uri>` from the peer and prints
//!   `FETCHED uri=<uri> sha256=<hex> bytes=<n> mime=<mime>` on stderr.
//!   Retries briefly to bridge the harness's `onFetch` registration window.
//!
//! Multiple sequential connections are accepted; each connection is served
//! until the peer disconnects or the link is declared dead.

use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;

use xacpp::carrier::XacppTransport;
use xacpp::carrier::socket::SocketTransport;
use xacpp::reliability::stream::BinaryMeta;
use xacpp::session::XacppSession;
use xacpp::session::handler::{
    EstablishDecision, EstablishHandler, NegotiateHandler, XacppSessionHandler,
};
use xacpp::session::peer::XacppPeer;
use xacpp::transfer::TransferConfig;
use xacpp::wire::capability::{Capabilities, EffectiveCapabilities};
use xacpp::wire::commands::XacppCommand;
use xacpp::wire::error::XacppError;
use xacpp::wire::events::XacppActivityEvent;
use xacpp::wire::fetch::{error_codes, format_transaction_id};
use xacpp::wire::message::{XacppRequest, XacppResponse};

/// Fixed responder identity (matches the legacy echo-mode `Established`
/// answer, so the ts initiator backfills `interop` either way).
const SESSION_ID: &str = "interop";
const SESSION_CREDENTIALS: &str = "interop-creds";

/// Serving mode selected by CLI flags.
#[derive(Clone, Debug)]
enum Mode {
    /// Legacy envelope echo (no fetch flag).
    Echo,
    /// Serve `<dir>/<uri>` for fetch requests.
    FetchServe(PathBuf),
    /// After the handshake, pull `<uri>` from the peer and log it.
    FetchPull(String),
}

fn main() {
    let (mode, port) = parse_args();
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime")
        .block_on(run(mode, port));
}

/// Parses `[--fetch-serve <dir> | --fetch-pull <uri>] --port N` (port
/// required); anything else exits with code 2.
fn parse_args() -> (Mode, u16) {
    let mut mode = Mode::Echo;
    let mut port = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--fetch-serve" => match args.next() {
                Some(dir) => mode = Mode::FetchServe(PathBuf::from(dir)),
                None => {
                    eprintln!("interop_peer: --fetch-serve requires a directory");
                    std::process::exit(2);
                }
            },
            "--fetch-pull" => match args.next() {
                Some(uri) => mode = Mode::FetchPull(uri),
                None => {
                    eprintln!("interop_peer: --fetch-pull requires a uri");
                    std::process::exit(2);
                }
            },
            "--port" => {
                port = args.next().and_then(|v| v.parse().ok());
            }
            other => {
                eprintln!("interop_peer: unknown argument {other}");
                std::process::exit(2);
            }
        }
    }
    let port = port.unwrap_or_else(|| {
        eprintln!("usage: interop_peer [--fetch-serve <dir> | --fetch-pull <uri>] --port <N>");
        std::process::exit(2);
    });
    (mode, port)
}

async fn run(mode: Mode, port: u16) {
    let listener = match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
        Ok(listener) => listener,
        Err(e) => {
            eprintln!("interop_peer: bind 127.0.0.1:{port} failed: {e}");
            std::process::exit(1);
        }
    };
    let bound_port = listener.local_addr().map(|a| a.port()).unwrap_or(port);
    eprintln!("READY port={bound_port}");

    loop {
        let (stream, peer_addr) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(e) => {
                eprintln!("interop_peer: accept failed: {e}");
                continue;
            }
        };
        eprintln!("interop_peer: connection from {peer_addr}");
        match &mode {
            Mode::Echo => tokio::spawn(serve_echo(stream)),
            Mode::FetchServe(_) | Mode::FetchPull(_) => {
                tokio::spawn(serve_fetch(stream, mode.clone()))
            }
        };
    }
}

/// Parks until the connection is known dead (peer disconnect, write
/// failure, silent timeout).
async fn park_until_dead(transport: &Arc<dyn XacppTransport>) {
    let mut closed = transport.closed();
    while !*closed.borrow_and_update() {
        if closed.changed().await.is_err() {
            break;
        }
    }
    eprintln!("interop_peer: connection closed");
}

// ---- Legacy echo mode (transport-driven, pre-fetch shape) ----

/// Serves one echo connection until the peer disconnects or the link dies.
async fn serve_echo(stream: tokio::net::TcpStream) {
    let transport = Arc::new(SocketTransport::new(stream));

    // Session-owned transfer state: ACK/NACK/RESUME + heartbeats with
    // SPEC §5 RECOMMENDED defaults, wired to this connection.
    let session = Arc::new(XacppSession::new(Some(TransferConfig::default())));

    // Envelope echo service.
    let event_seq = Arc::new(AtomicU64::new(0));
    transport
        .on_request(Arc::new(move |_session_id, payload| {
            let event_seq = Arc::clone(&event_seq);
            Box::pin(async move { handle_request(event_seq, payload).await })
                as Pin<
                    Box<dyn std::future::Future<Output = Result<XacppResponse, XacppError>> + Send>,
                >
        }))
        .expect("register request handler before connect");

    // Binary echo: fully reassembled transactions go back with the same
    // payload. The callback must not block, so the echo is spawned.
    let echo_session: Arc<XacppSession> = Arc::clone(&session);
    session
        .on_binary(Arc::new(move |_txid, payload| {
            let session = Arc::clone(&echo_session);
            tokio::spawn(async move {
                match session.send_binary(payload, None).await {
                    Ok(_) => eprintln!("interop_peer: binary echoed"),
                    Err(e) => eprintln!("interop_peer: binary echo failed: {e}"),
                }
            });
        }))
        .await;

    if let Err(e) = transport.connect().await {
        eprintln!("interop_peer: connect failed: {e}");
        return;
    }

    // Wire the transfer session now that the connection is live (clock +
    // RESUME handshake run through the write surface).
    session
        .wire(Arc::clone(&transport) as Arc<dyn XacppTransport>)
        .await
        .expect("wire transfer session");

    park_until_dead(&(Arc::clone(&transport) as Arc<dyn XacppTransport>)).await;
}

/// One request → one response (echo semantics per the module docs).
async fn handle_request(
    event_seq: Arc<AtomicU64>,
    payload: XacppRequest,
) -> Result<XacppResponse, XacppError> {
    match payload {
        XacppRequest::Command(XacppCommand::Negotiate { capabilities }) => {
            Ok(XacppResponse::Negotiated { capabilities })
        }
        XacppRequest::Command(XacppCommand::Establish { credentials }) => {
            let _ = credentials;
            Ok(established())
        }
        XacppRequest::Command(XacppCommand::EstablishConfirm) => Ok(established()),
        // Fetch is answered by consumer handlers in real peers (SPEC §8);
        // this legacy echo path drives transports directly, so refuse
        // explicitly.
        XacppRequest::Command(
            XacppCommand::Fetch { .. }
            | XacppCommand::FetchBegin { .. }
            | XacppCommand::FetchPause { .. }
            | XacppCommand::FetchResume { .. }
            | XacppCommand::FetchAbort { .. },
        ) => Ok(XacppResponse::error(
            xacpp::wire::fetch::error_codes::FETCH_REFUSED,
            "interop_peer echo mode does not serve fetch",
        )),
        XacppRequest::Command(XacppCommand::Generic {
            name, arguments, ..
        }) => Ok(XacppResponse::Generic {
            name,
            data: arguments,
        }),
        XacppRequest::Event(event) => {
            let n = event_seq.fetch_add(1, Ordering::Relaxed) + 1;
            eprintln!("EVENT name={} n={n}", event.event.name);
            Ok(XacppResponse::acknowledge())
        }
    }
}

fn established() -> XacppResponse {
    XacppResponse::Established {
        session_id: SESSION_ID.into(),
        credentials: SESSION_CREDENTIALS.into(),
    }
}

// ---- Fetch modes (full XacppPeer responder stack) ----

/// Serves one fetch-mode connection: a `XacppPeer` responder whose
/// `on_establish` constructs and wires the responder session (fetch route
/// registration rides on that wire), so Peer-layer fetch dispatch reaches
/// the session's fetch machinery.
async fn serve_fetch(stream: tokio::net::TcpStream, mode: Mode) {
    let transport: Arc<dyn XacppTransport> = Arc::new(SocketTransport::new(stream));

    let capabilities = Capabilities::default();

    // Responder-session slot: the fetch directory holds only a Weak to the
    // session's directory entry, so the session must outlive the link or
    // Peer-layer lookup_route finds nothing. Held here, released after the
    // link is declared dead.
    let session_slot: Arc<tokio::sync::Mutex<Option<Arc<XacppSession>>>> =
        Arc::new(tokio::sync::Mutex::new(None));

    let establish = Arc::new(FetchEstablishHandler {
        mode,
        session_slot: Arc::clone(&session_slot),
    });
    let peer = XacppPeer::new(
        capabilities,
        Arc::clone(&transport),
        None, // responder side: sessions are constructed inside on_establish
        Arc::new(LogNegotiate),
        establish,
    );

    if let Err(e) = peer.connect().await {
        eprintln!("interop_peer: peer connect failed: {e}");
        return;
    }

    park_until_dead(&transport).await;
    // Link dead: drop the responder session (fetch route dies with it).
    session_slot.lock().await.take();
}

/// Negotiate handler: consume the effective capabilities (nothing to do for
/// the echo-grade fetch peer).
struct LogNegotiate;

#[async_trait]
impl NegotiateHandler for LogNegotiate {
    async fn on_negotiate(&self, effective: EffectiveCapabilities) -> Result<(), XacppError> {
        log::debug!(
            "interop_peer: negotiated emit_events: {:?}",
            effective.emit_events
        );
        Ok(())
    }
}

/// Establish handler for the fetch modes: constructs the responder session
/// (`with_identity` before `wire` so the fetch route registers under the
/// fixed sid), arms it per mode, and answers `Established`.
struct FetchEstablishHandler {
    mode: Mode,
    /// Keeps the responder session alive past `on_establish` (see
    /// `serve_fetch`); the fetch directory only holds a Weak.
    session_slot: Arc<tokio::sync::Mutex<Option<Arc<XacppSession>>>>,
}

#[async_trait]
impl EstablishHandler for FetchEstablishHandler {
    async fn on_establish(
        &self,
        transport: Arc<dyn XacppTransport>,
        _credentials: Option<String>,
    ) -> Result<EstablishDecision, XacppError> {
        let session = Arc::new(
            XacppSession::new(Some(TransferConfig::default()))
                .with_identity(SESSION_ID.to_string(), SESSION_CREDENTIALS.to_string())
                .await,
        );

        let serve_dir = match &self.mode {
            Mode::FetchServe(dir) => Some(dir.clone()),
            Mode::FetchPull(uri) => {
                let pull_session = Arc::clone(&session);
                let uri = uri.clone();
                tokio::spawn(async move { pull_and_report(pull_session, uri).await });
                None
            }
            Mode::Echo => unreachable!("fetch establish handler never runs in echo mode"),
        };

        session.wire(transport).await?;

        // Park the session where serve_fetch holds it.
        *self.session_slot.lock().await = Some(Arc::clone(&session));

        Ok(EstablishDecision::Established {
            session_id: SESSION_ID.into(),
            handler: Arc::new(FetchEchoHandler {
                serve_dir,
                session: Arc::downgrade(&session),
            }),
            credentials: SESSION_CREDENTIALS.into(),
        })
    }

    async fn on_establish_confirm(
        &self,
        _transport: Arc<dyn XacppTransport>,
    ) -> Result<(String, Arc<dyn XacppSessionHandler>, String), XacppError> {
        Err(XacppError::Internal(
            "challenge path not used by interop_peer".into(),
        ))
    }
}

/// Pulls `uri` from the peer: the kernel shell returns the `fetch_ready`
/// metadata; the bytes are claimed via the stream surface (matched by
/// txid) and verified against the announced sha256 before reporting in the
/// harness's `FETCHED uri=<uri> sha256=<hex> bytes=<n> mime=<mime>` format.
///
/// The ts harness registers its responder lazily; a short settle delay
/// plus retries bridges that registration window without making the peer
/// racy.
async fn pull_and_report(session: Arc<XacppSession>, uri: String) {
    struct Claim {
        meta: tokio::sync::Mutex<Option<([u8; 16], u32)>>,
        chunks: tokio::sync::Mutex<Vec<Vec<u8>>>,
        terminal: tokio::sync::Mutex<Option<xacpp::reliability::stream::StreamState>>,
    }
    let claim = Arc::new(Claim {
        meta: tokio::sync::Mutex::new(None),
        chunks: tokio::sync::Mutex::new(Vec::new()),
        terminal: tokio::sync::Mutex::new(None),
    });
    {
        let claim = Arc::clone(&claim);
        session
            .on_binary_stream(Arc::new(
                move |stream: xacpp::reliability::stream::BinaryStream| {
                    let claim = Arc::clone(&claim);
                    tokio::spawn(async move {
                        *claim.meta.lock().await = Some((stream.txid(), stream.total_len()));
                        while let Some(chunk) = stream.read().await {
                            claim.chunks.lock().await.push(chunk.bytes);
                        }
                        *claim.terminal.lock().await = Some(stream.state());
                    });
                },
            ))
            .await;
    }

    const ATTEMPTS: u32 = 5;
    for attempt in 1..=ATTEMPTS {
        tokio::time::sleep(Duration::from_millis(500)).await;
        match session.fetch(&uri).await {
            Ok(meta) => {
                // Wait for the stream matching the announced txid.
                let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
                loop {
                    let txid_matches = claim
                        .meta
                        .lock()
                        .await
                        .map(|(txid, _)| txid == meta.txid)
                        .unwrap_or(false);
                    let done = txid_matches
                        && matches!(
                            *claim.terminal.lock().await,
                            Some(xacpp::reliability::stream::StreamState::Complete)
                        );
                    if done {
                        break;
                    }
                    if tokio::time::Instant::now() >= deadline {
                        eprintln!("interop_peer: stream for the fetch never completed");
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                let payload = claim.chunks.lock().await.concat();
                let sha256 = sha256_hex(&payload);
                if payload.len() as u64 != meta.size_bytes || sha256 != meta.sha256 {
                    eprintln!(
                        "interop_peer: stream mismatch for {uri}: {}/{} bytes, sha {sha256} vs {}",
                        payload.len(),
                        meta.size_bytes,
                        meta.sha256
                    );
                    return;
                }
                eprintln!(
                    "FETCHED uri={uri} sha256={sha256} bytes={} mime={}",
                    meta.size_bytes, meta.mime
                );
                return;
            }
            Err(e) => {
                eprintln!("interop_peer: fetch attempt {attempt}/{ATTEMPTS} failed: {e}");
            }
        }
    }
    eprintln!("interop_peer: fetch of {uri} failed after {ATTEMPTS} attempts");
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest;
    format!("{:x}", sha2::Sha256::digest(bytes))
}

/// Session handler behind the fetch modes' `Established` answer: generic
/// echo plus event ack — and, in serve mode, the fetch responder itself
/// (SPEC §8: `fetch` commands reach the consumer's `SessionHandler`).
struct FetchEchoHandler {
    /// `Some(dir)` in `--fetch-serve` mode: serve `<dir>/<uri>`.
    serve_dir: Option<PathBuf>,
    /// Weak back-reference used to open the streaming sink.
    session: std::sync::Weak<XacppSession>,
}

#[async_trait]
impl XacppSessionHandler for FetchEchoHandler {
    /// This handler answers the fetch family itself (hand-rolled responder,
    /// no FetchService wrapper) — declare it, or the peer-level gate
    /// answers `fetch_unavailable` before `on_command` ever runs.
    fn is_fetch_responder(&self) -> bool {
        true
    }

    async fn on_command(&self, command: XacppCommand) -> Result<XacppResponse, XacppError> {
        match command {
            XacppCommand::Generic {
                name, arguments, ..
            } => Ok(XacppResponse::Generic {
                name,
                data: arguments,
            }),
            XacppCommand::Fetch { uri } => self.serve_fetch(uri).await,
            other => Err(XacppError::Internal(format!(
                "interop_peer fetch session does not handle {other:?}"
            ))),
        }
    }

    async fn on_event(&self, _event: XacppActivityEvent) -> Result<XacppResponse, XacppError> {
        Ok(XacppResponse::acknowledge())
    }
}

impl FetchEchoHandler {
    /// Answers `fetch{uri}` with `fetch_ready` and streams `<dir>/<uri>`
    /// through a sink opened with the announced txid (SPEC §8.4: the
    /// response completes before the binary transaction — the stream task
    /// yields so the response frame queues on the egress first).
    async fn serve_fetch(&self, uri: String) -> Result<XacppResponse, XacppError> {
        let Some(dir) = &self.serve_dir else {
            return Ok(XacppResponse::error(
                error_codes::FETCH_REFUSED,
                "not a fetch-serving peer",
            ));
        };
        let Some(session) = self.session.upgrade() else {
            return Ok(XacppResponse::error(
                error_codes::FETCH_REFUSED,
                "responder session is gone",
            ));
        };
        let bytes = match std::fs::read(dir.join(&uri)) {
            Ok(bytes) => bytes,
            Err(e) => {
                return Ok(XacppResponse::error(
                    error_codes::FETCH_NOT_FOUND,
                    format!("{uri}: {e}"),
                ));
            }
        };
        let txid = uuid::Uuid::new_v4().into_bytes();
        let sink = session
            .open_binary_sink(
                BinaryMeta {
                    total_len: bytes.len() as u32,
                },
                Some(txid),
            )
            .await
            .map_err(|e| XacppError::Internal(e.to_string()))?;
        let response = XacppResponse::FetchReady {
            txid: format_transaction_id(txid),
            sha256: sha256_hex(&bytes),
            size_bytes: bytes.len() as u64,
            mime: "application/octet-stream".into(),
        };
        tokio::spawn(async move {
            for _ in 0..10 {
                tokio::task::yield_now().await;
            }
            let mut offset = 0usize;
            while offset < bytes.len() {
                let end = (offset + 64 * 1024).min(bytes.len());
                if sink.write(&bytes[offset..end]).await.is_err() {
                    return;
                }
                offset = end;
            }
            let _ = sink.finish().await;
        });
        Ok(response)
    }
}
