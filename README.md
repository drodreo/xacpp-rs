# xacpp

[中文](./README.zh-CN.md)

Agent Control Plane Protocol — Rust implementation.

xacpp defines the communication protocol between an agent and its peers. It provides a layered architecture for request-response messaging with session management over multiple transport backends.

## Architecture

```
┌──────────────────────────────────────────────────────────────┐
│  Peer (lifecycle orchestration, initiator side)              │
│  negotiate → connect → establish → disconnect                │
├──────────────────────────────────────────────────────────────┤
│  Session (communication facade + transfer state)             │
│  request_command / send_event / send_binary;                 │
│  owns TransferSession (ACK/NACK/RESUME/heartbeat),           │
│  wires itself onto the current transport                     │
├──────────────────────────────────────────────────────────────┤
│  Transport (connection entity)                               │
│  Physical IO, binary framing, single-FIFO egress             │
├──────────────────────────────────────────────────────────────┤
│  Stdio / TCP                                                  │
└──────────────────────────────────────────────────────────────┘
```

A session outlives individual connections: on disconnect the session detaches
its transfer mount from the dead transport immediately — frames still flushing
through the dead connection are dropped, never absorbed into session state —
and the transfer state stays in the session. On reconnect the session wires
itself onto the new transport and interrupted transfers resume through the
RESUME handshake (verdict-gated retransmission, SPEC §5).

## Usage

Add to `Cargo.toml`:

```toml
[dependencies]
xacpp = "1.2"
```

### Establish a session (initiator side)

```rust
use std::sync::Arc;
use xacpp::capability::Capabilities;
use xacpp::handler::{EstablishHandler, NegotiateHandler, XacppSessionHandler};
use xacpp::peer::XacppPeer;
use xacpp::session::XacppSession;
use xacpp::transport::socket::SocketTransport;
use xacpp::transport::XacppTransport;

// A session owns its transfer state; it is created independently of any
// connection and wires itself onto transports as they come and go.
let session = Arc::new(XacppSession::new(None)); // None = TransferConfig::default()

let transport: Arc<dyn XacppTransport> =
    Arc::new(SocketTransport::connect_to("127.0.0.1:8080".into()));

// Peer orchestrates the lifecycle and auto-wires the session on connect.
let caps = Capabilities {
    commands: vec![/* JSON Schemas this side handles */],
    produce_events: vec![],
    accept_events: vec![],
    // Declare fetch-extension support (SPEC §8.7); omit (None) to disable fetch.
    fetch_ext: None,
};
let peer = XacppPeer::new(
    caps,
    transport,
    Some(Arc::clone(&session)),
    Arc::new(MyNegotiateHandler),
    Arc::new(MyEstablishHandler),
);
peer.connect().await?;

// Establish a logical session (fills identity/emit-events into the session).
peer.establish(None, Arc::new(my_session_handler), |_| Ok(())).await?;

// Send commands/events/binary through the session facade.
let response = session.request_command(cmd).await?;
session.send_event(event).await?;
let txid = session
    .send_binary(payload, Some(Arc::new(|result| { /* BinarySendResult */ })))
    .await?;
```

### Handle incoming connections (responder side)

```rust
// Responder side has no peer: accept a connection, create a session,
// restore identity if reconnecting, and wire the session onto the transport.
let transport: Arc<dyn XacppTransport> = Arc::new(SocketTransport::new(stream));
let session = Arc::new(
    XacppSession::new(None)
        .with_identity("session-1".into(), credentials),
);
session.wire(Arc::clone(&transport)).await?;
session.on_binary(Arc::new(|txid, payload| { /* consume */ }));

transport
    .on_request(Arc::new(move |_sid, payload| {
        // Return XacppResponse for inbound commands/events
    }))
    .expect("register request handler before connect");
```

### TCP Transport (for network communication)

```rust
use xacpp::transport::socket::SocketTransport;

// Client
let client = SocketTransport::connect_to("127.0.0.1:8080".into());

// Server (with accepted TcpStream)
let server = SocketTransport::new(accepted_stream);
```

## API

### Core Types

| Type | Description |
|------|-------------|
| `XacppPeer` | Lifecycle orchestration (negotiate/connect/establish/disconnect); wires the session onto the transport automatically (initiator side) |
| `XacppSession` | Communication facade (`request_command`/`send_event`/`send_binary`); owns transfer state, survives reconnections, `wire`/`unwire` onto transports |
| `TransferSession` | Transfer-transaction state machine (ACK/NACK/RESUME/heartbeat/backpressure); owned by `XacppSession` |
| `TransferConfig` | Transfer tuning (heartbeat interval, silent timeout, ACK batching, tick) |
| `XacppTransport` | Connection entity: physical IO, binary framing, single-FIFO egress |
| `XacppSessionHandler` | Handles inbound Command/Event for a session |
| `NegotiateHandler` / `EstablishHandler` | Handshake handlers |
| `XacppCommand` / `XacppEvent` | Protocol commands and events |
| `ActivityRef` | Activity identifier reference shared by command and event envelopes |
| `XacppResponse` | Response payload (Established, Acknowledge, Action, etc.) |
| `XacppError` | Error enum with machine-readable codes |
| `PeerState` | Peer state enum (Disconnected, Connected) |

### Transport Implementations

| Type | Description |
|------|-------------|
| `StdioTransport` | Child-process pipes with binary framing |
| `SocketTransport` | TCP transport (`TcpStream`), spawn-per-request concurrency |

## Wire Protocol

Framing is the XACPP binary frame protocol (docs/SPEC.md §3/§4): every envelope travels as the payload of a `DATA_JSON` frame. Envelope structure:

```json
{"type":"request","id":"r1","payload":{"kind":"command","payload":{"establish":{"credentials":null}}}}
{"type":"request","id":"r2","payload":{"kind":"command","payload":{"generic":{"name":"report_to_user","arguments":{},"activity":{"id":"act-1"}}}}}
{"type":"request","id":"r3","payload":{"kind":"event","payload":{"activity":{"id":"act-1"},"event":{"name":"think","data":{"content":"hi"}}}}}
{"type":"response","id":"r1","payload":{"kind":"established","sessionId":"s1"}}
```

Generic commands carry an optional `activity` field as a structured `{id}` reference;
when absent, the field is omitted from the wire format. It is optional at the protocol
level — validation is the command implementor's decision. Event envelopes identify
their activity with the same structured `{id}` reference.

### Command Declarations

A command schema may carry a `dispatcher` field with values `"bridge"` or `"tool"`,
defaulting to `"bridge"`. `bridge` means the command is integrated via the event
bridge system path (not exposed to the model tool surface); `tool` means the command
is injected into the receiver's model tool surface (the model invokes it directly).
Declaration schemas are pass-through JSON; the protocol library does not enforce types.

## Performance

Measured by `cargo run --release --example bench_protocol` (TCP loopback, release build); scale via `XACPP_BENCH_N`, select a group via `XACPP_BENCH_ONLY`. These numbers are the reference for future optimization work.

| Metric | Baseline 2026-10-14 | Re-verified 2026-10-23 |
|---|---|---|
| Codec ~200 B (frames vs JSONL) | level | frames 492.9 vs 315.1 MB/s |
| Codec ~2 KiB (frames vs JSONL) | +49% | — |
| Request-response RTT | p50 0.070 ms @ ~13.5k events/s | p50 0.071 ms @ ~13.3k events/s |
| 64 MiB `sendBinary` (1 MiB frames) | 463–638 MB/s | 198–209 MB/s |
| 4 × 16 MiB concurrent | 638–732 MB/s | 447 MB/s |
| Heartbeat p50 under bulk | level with idle (10.2 ms) | 10.2 ms = idle |

## License

MIT
