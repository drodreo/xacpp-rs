# AGENTS.md

## Project Overview

xacpp (Agent Control Plane Protocol) — Rust implementation.

This is the Rust counterpart of [xacpp-ts](https://github.com/drodreo/xacpp-ts). The two projects share identical wire protocol (JSONL envelope) and must stay in sync.

## Architecture

Three-layer design, mirroring xacpp-ts:

```
┌──────────────────────────────────────────────────────────────┐
│  Peer (protocol layer)                                       │
│  Typed operations: request_command / request_event           │
│  Session routing: session_id → XacppSessionHandler           │
├──────────────────────────────────────────────────────────────┤
│  Session (session layer)                                     │
│  Holds XacppSessionHandler, sends directly via Transport     │
│  Does not go through Peer                                    │
├──────────────────────────────────────────────────────────────┤
│  Transport (transport layer)                                 │
│  Single semantic: send (request-response), caller may skip   │
│  Internally: envelope assembly/disassembly, id correlation,  │
│  pending matching                                            │
│  Does not know about specific business event types           │
├──────────────────────────────────────────────────────────────┤
│  Underlying pipe (Stdio / TCP / WebSocket)                   │
│  Raw byte stream, frame splitting (JSONL)                    │
└──────────────────────────────────────────────────────────────┘
```

### Layer boundaries

- **Transport → upper**: exposes `send` / `on_request`, hides envelope id, encoding/decoding
- **Peer → upper**: exposes typed `establish` / `request_command` / `request_event` + session routing
- **Session → upper**: holds `XacppSessionHandler`, sends directly via Transport, bypasses Peer

### File structure

| File | Responsibility |
|------|---------------|
| `src/transport/mod.rs` | `XacppTransport` trait + `RequestHandler` type |
| `src/handler.rs` | `XacppSessionHandler` + `EstablishHandler` traits |
| `src/session.rs` | `XacppSession` struct |
| `src/peer.rs` | `XacppPeer` struct + `PeerState` enum |
| `src/message.rs` | `XacppError`, `XacppRequest`, `XacppResponse`, `XacppEnvelope` |
| `src/commands/mod.rs` | `XacppCommand` enum |
| `src/events/` | Event type definitions |
| `src/transport/stdio.rs` | Stdio transport (async stdin/stdout JSONL) |
| `src/transport/socket.rs` | TCP transport (`TcpStream`, spawn-per-request concurrency) |

## Wire Protocol

All wire messages are `XacppEnvelope` with `type` field routing:

```json
Request:  {"type":"request","id":"r1","session_id":null,"payload":{"kind":"command","payload":{"establish":{"credentials":null}}}}
Request:  {"type":"request","id":"r2","session_id":"s1","payload":{"kind":"event","payload":{"type":"think","content":"hi"}}}
Response: {"type":"response","id":"r1","payload":{"kind":"established","sessionId":"s1"}}
Response: {"type":"response","id":"r2","session_id":"s1","payload":{"kind":"action","requestId":"req-1","type":"approve"}}
```

### Naming convention

- Envelope layer: `session_id` (snake_case, `#[serde(rename_all_fields = "camelCase")]` does not apply)
- Response payload: `sessionId`, `requestId` (camelCase, via `rename_all_fields = "camelCase"` on `XacppResponse`)

### XacppCommand wire format

Uses externally tagged serde (`#[serde(rename_all = "snake_case")]`):

- `Establish { credentials: None }` → `{"establish":{"credentials":null}}`
- `NewActivity` → `"new_activity"`

## Build & Test

```bash
cargo build
cargo test
```

## Testing

- `tests/peer_e2e_tests.rs` — Transport + Peer + Session e2e (17 tests)
- `tests/serde_tests.rs` — Serialization round-trip (15 tests)
- `tests/socket_concurrent_tests.rs` — SocketTransport concurrency (3 tests)

Must stay in sync with xacpp-ts test suite.

## Conventions

- All comments in English
- Keep in sync with xacpp-ts: when adding/removing a command, response variant, or envelope field, update both projects
- `XacppCommand` is an externally tagged enum — Establish carries `credentials: Option<String>`
- Transport implementations must handle envelope `session_id` field
- `XacppSession` is `pub(crate)` constructable — only `XacppPeer::establish` creates sessions
