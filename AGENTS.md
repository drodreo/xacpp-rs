# AGENTS.md

## Project Overview

xacpp (Agent Control Plane Protocol) — Rust implementation.

This is the Rust counterpart of [xacpp-ts](https://github.com/drodreo/xacpp-ts). The two projects share identical wire protocol (binary frames; envelope JSON is carried in `DATA_JSON` frames) and must stay in sync.

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
│  Raw byte stream, binary frame layer (XACP frames, SPEC §3)  │
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
| `src/frame.rs` | Binary wire frame codec (`Frame`, `FrameType`, `FrameDecoder`; SPEC §3/§4) |
| `src/commands/mod.rs` | `XacppCommand` enum |
| `src/events/` | Event type definitions |
| `src/transport/stdio.rs` | Stdio transport (async stdin/stdout, binary frames) |
| `src/transport/socket.rs` | TCP transport (`TcpStream`, spawn-per-request concurrency) |

## Wire Protocol

The wire carries self-describing binary frames (36-byte header + payload + CRC-32, SPEC §3). Each `XacppEnvelope` is serialized to JSON and sent as a single-frame `DATA_JSON` transaction (`seq = 0`, `total_len` = payload length, fresh UUID v4 `transaction_id` per frame). Control frames (ACK/NACK/RESUME/...) are reserved for the reliability layer and are ignored by the current reader. Wire size limits (SPEC §4): `DATA_JSON` payload ≤ 1 MiB.

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

- `tests/peer_e2e_tests.rs` — Transport + Peer + Session e2e
- `tests/serde_tests.rs` — Serialization round-trip
- `tests/socket_concurrent_tests.rs` — SocketTransport concurrency
- `tests/egress_wire_tests.rs` — Raw-TCP wire order/throughput evidence
- `tests/transport_reliability_tests.rs` — Transfer (§5) retransmission / RESUME / backpressure
- `tests/fetch_tests.rs` — Fetch channel e2e (design-anchor matrix)
- `examples/egress_bench.rs` — 40k-event throughput baseline (`cargo run --example egress_bench`, `XACPP_BENCH_N` overrides scale)

Must stay in sync with xacpp-ts test suite.

## Transfer protocol invariants (SPEC §5)

Four invariants govern the transfer layer; every change must preserve them:

1. **Send-side pointer advances on ACK only** — unacknowledged bytes cap the
   in-flight window (backpressure); copies release on ACK, terminal release on
   final ACK or abort.
2. **Receiver slots close on byte-count or control frame** — a transaction
   completes when its bytes add up (`TRANSACTION_COMPLETED` announcement /
   `TRANSACTION_ABORT` end it otherwise); no tombstones — a consumed slot
   leaves no trace.
3. **Everything outside the live lists is answered, never silently absorbed** —
   a data frame for a slotless txid is not reassembled (late retransmission of
   a settled transaction, or an unannounced stream); the holder's closure
   signal is the RESUME verdict, never a data-frame reply.
4. **Closure is adjudicated from local knowledge on both sides** — on an
   `unknown`/`rejected` verdict the holder closes from its own state: all
   frames sent (tail frame plus the announcement) ⇒ confirmed; frames still
   owed ⇒ failed. The receiver never needs to explain why a transaction is
   gone — no terminal trace is required for the loop to close.

### Known misreadings (each caused a real defect)

- **"A slotless data frame opens a fresh slot"** — wrong. The reassembler once
  absorbed late retransmissions by opening resurrection slots; that path
  produced both the RESUME deadlock (a resurrected slot never completes, the
  final ACK never re-emerges) and a duplicate-delivery window. A txid reaches
  the receiver only through the peer's explicit announcement (§8 causality
  chain: metadata observed before any byte); a slotless frame is dropped.
- **"`TRANSACTION_COMPLETED` is the receiver's final ACK"** — wrong direction.
  It is holder → receiver: the sender declaring its data plane finished. The
  final ACK is the receiver's cumulative ACK reaching the last frame.
- **"An `unknown`/`rejected` verdict means the transfer failed"** — wrong as an
  unconditional rule. The verdict only says "no open slot here"; the holder's
  own completeness decides confirmed vs failed (invariant 4).
- **"An expired transaction errors out"** — retired semantics. Copies carry no
  retention deadline and no expiry sweep; error code `0x0002` stays registered
  for pre-1.1 tolerance only.
- **Design instinct check**: if a fix seems to need a new frame type, a new
  verdict state, or memory of past transactions, re-read the invariants — the
  recovery design closes its loops with pieces already in the protocol
  (verdict + local knowledge). Complexity here has always meant a misread, not
  a gap.

## Conventions

- All comments and commit messages in English
- Keep in sync with xacpp-ts: when adding/removing a command, response variant, or envelope field, update both projects
- `XacppCommand` is an externally tagged enum — Establish carries `credentials: Option<String>`
- Transport implementations must handle envelope `session_id` field
- `XacppSession` is `pub(crate)` constructable — only `XacppPeer::establish` creates sessions
