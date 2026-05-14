# xacpp

Agent Control Plane Protocol — Rust implementation.

xacpp defines the communication protocol between an agent and its peers. It provides a layered architecture for request-response messaging with session management over multiple transport backends.

## Architecture

```
┌──────────────────────────────────────────────────────────────┐
│  Peer (protocol layer)                                       │
│  Typed operations + session routing                          │
├──────────────────────────────────────────────────────────────┤
│  Session (session layer)                                     │
│  Independent session context, sends directly via Transport   │
├──────────────────────────────────────────────────────────────┤
│  Transport (transport layer)                                 │
│  Envelope assembly, id correlation, pending matching         │
├──────────────────────────────────────────────────────────────┤
│  Stdio / TCP / WebSocket                                     │
└──────────────────────────────────────────────────────────────┘
```

## Usage

Add to `Cargo.toml`:

```toml
[dependencies]
xacpp = "0.1"
```

### Establish a session (initiator side)

```rust
use std::sync::Arc;
use xacpp::commands::XacppCommand;
use xacpp::events::XacppEvent;
use xacpp::handler::{EstablishHandler, XacppSessionHandler};
use xacpp::message::{XacppRequest, XacppResponse};
use xacpp::peer::XacppPeer;
use xacpp::transport::stdio::StdioTransport;
use xacpp::transport::XacppTransport;

// Create transport + peer
let transport: Arc<dyn XacppTransport> = Arc::new(StdioTransport::new(/* ... */));

struct MyEstablishHandler;
#[async_trait::async_trait]
impl EstablishHandler for MyEstablishHandler {
    async fn on_establish(
        &self,
        transport: Arc<dyn XacppTransport>,
        credentials: Option<String>,
    ) -> Result<(String, Arc<dyn XacppSessionHandler>), xacpp::error::XacppError> {
        Ok(("session-1".into(), Arc::new(MySessionHandler)))
    }
}

let peer = XacppPeer::new(transport, Arc::new(MyEstablishHandler));
peer.connect().await?;

// Establish a logical session
let session = peer.establish(None, Arc::new(my_session_handler)).await?;

// Send commands/events through the session
let response = session.request_command(XacppCommand::NewActivity).await?;
session.request_event(XacppEvent::Think { content: "Hello!".into() }).await?;
```

### Handle incoming requests (responder side)

```rust
struct MySessionHandler;

#[async_trait::async_trait]
impl XacppSessionHandler for MySessionHandler {
    async fn on_command(&self, command: XacppCommand) -> Result<XacppResponse, XacppError> {
        Ok(XacppResponse::Acknowledge)
    }

    async fn on_event(&self, event: XacppEvent) -> Result<XacppResponse, XacppError> {
        Ok(XacppResponse::Acknowledge)
    }
}
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
| `XacppTransport` | Transport trait (`connect`, `disconnect`, `send`, `on_request`) |
| `XacppPeer` | Protocol endpoint with session routing |
| `XacppSession` | Logical session, sends via Transport directly |
| `XacppSessionHandler` | Handles inbound Command/Event for a session |
| `EstablishHandler` | Handles Establish handshake requests |
| `XacppCommand` | Protocol commands (`Establish`, `NewActivity`, etc.) |
| `XacppEvent` | Protocol events (Think, ActionRequest, Question, etc.) |
| `XacppRequest` | Request payload (Command or Event) |
| `XacppResponse` | Response payload (Established, Acknowledge, Action, etc.) |
| `XacppError` | Error enum with machine-readable codes |
| `PeerState` | Peer state enum (Disconnected, Connected) |

### Transport Implementations

| Type | Description |
|------|-------------|
| `StdioTransport` | Async stdin/stdout JSONL pipe |
| `SocketTransport` | TCP transport (`TcpStream`), spawn-per-request concurrency |

## Wire Protocol

JSONL (one JSON object per line) with envelope structure:

```json
{"type":"request","id":"r1","payload":{"kind":"command","payload":{"establish":{"credentials":null}}}}
{"type":"response","id":"r1","payload":{"kind":"established","sessionId":"s1"}}
```

## License

MIT
