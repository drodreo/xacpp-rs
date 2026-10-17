//! XACPP interoperability peer (xacpp-rs) — the contract counterpart for
//! xacpp-ts cross-library tests.
//!
//! Behavior (stability first, deliberately simple):
//!
//! - CLI: `interop_peer --port N`; binds `127.0.0.1:N` (TCP).
//! - Logs go to **stderr only** — stdout stays clean for harness use.
//! - After the listener is bound, prints `READY port=N` on stderr; the
//!   harness waits for this line before connecting.
//! - Envelope layer echo service:
//!   - `Negotiate` → `Negotiated` echoing the initiator's capabilities;
//!   - `Establish` / `EstablishConfirm` → `Established` (`interop` /
//!     `interop-creds`);
//!   - any other command → `Generic` response echoing `name` with the
//!     command arguments as `data`.
//! - Events: every received event prints `EVENT name=<name> n=<count>` on
//!   stderr (count starts at 1) and is acknowledged.
//! - Binary: every fully reassembled `DATA_BINARY` transaction is echoed
//!   back with the same payload via `send_binary`.
//! - Transfer: an `XacppSession` is wired (default config:
//!   heartbeats every 30 s, silent timeout 90 s), so ACK/NACK/RESUME and
//!   bidirectional heartbeats work.
//!
//! Multiple sequential connections are accepted; each connection is served
//! until the peer disconnects or the link is declared dead.

use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use xacpp::commands::XacppCommand;
use xacpp::error::XacppError;
use xacpp::message::{XacppRequest, XacppResponse};
use xacpp::session::XacppSession;
use xacpp::transport::TransferConfig;
use xacpp::transport::XacppTransport;
use xacpp::transport::socket::SocketTransport;

fn main() {
    let port = parse_port();
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime")
        .block_on(run(port));
}

/// Parses `--port N` (required); anything else exits with code 2.
fn parse_port() -> u16 {
    let args: Vec<String> = std::env::args().collect();
    let mut port = None;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--port" => {
                port = args.get(i + 1).and_then(|v| v.parse().ok());
                i += 2;
            }
            other => {
                eprintln!("interop_peer: unknown argument {other}");
                std::process::exit(2);
            }
        }
    }
    port.unwrap_or_else(|| {
        eprintln!("usage: interop_peer --port <N>");
        std::process::exit(2);
    })
}

async fn run(port: u16) {
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
        tokio::spawn(serve(stream));
    }
}

/// Serves one connection until the peer disconnects or the link dies.
async fn serve(stream: tokio::net::TcpStream) {
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
    session.on_binary(Arc::new(move |_txid, payload| {
        let session = Arc::clone(&echo_session);
        tokio::spawn(async move {
            match session.send_binary(payload, None).await {
                Ok(_) => eprintln!("interop_peer: binary echoed"),
                Err(e) => eprintln!("interop_peer: binary echo failed: {e}"),
            }
        });
    }));

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

    // Park until the connection is known dead (peer disconnect, write
    // failure, silent timeout), then let this task end.
    let mut closed = transport.closed();
    while !*closed.borrow_and_update() {
        if closed.changed().await.is_err() {
            break;
        }
    }
    eprintln!("interop_peer: connection closed");
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
        session_id: "interop".into(),
        credentials: "interop-creds".into(),
    }
}
