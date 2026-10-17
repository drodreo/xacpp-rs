# XACPP Binary Wire Protocol — Specification v1.0

## 1. Overview

XACPP (Agent Control Plane Protocol) v1.0 defines a self-describing binary
frame wire format with transaction-level reliability (unconfirmed
retransmission, reconnection resume) and an optional end-to-end session
security layer designed for untrusted relay topologies.

Message semantics — negotiation, commands, events, notify — are unchanged:
the message semantics layer continues to operate on typed payloads. The
upgrade replaces the wire encoding and adds delivery reliability.

Goals:

- Native binary payload transport: multimodal content moves as raw bytes,
  without text-encoding inflation.
- Delivery reliability: unconfirmed transactions are retransmitted; interrupted
  transfers resume across reconnections.
- Frame interleaving: latency-sensitive control traffic is never head-of-line
  blocked behind bulk data.
- Optional, consumer-opted-in end-to-end security; safe for future relay
  topologies where the relay is untrusted.

## 2. Layered Architecture

```text
Carrier (TCP direct / TCP via relay [future] / stdio)
 └─ Outer transport protection (optional; passive-eavesdrop defense only,
    carries no identity)
     └─ Session security layer (XACPP-owned, end-to-end, optional)
         └─ Binary frame layer
             └─ Message semantics layer (negotiate / commands / events / notify)
```

The frame layer is orthogonal to both the carrier and the security layer:
future carriers — including relays — require no frame format changes.

## 3. Frame Format

Fixed header (36 bytes), variable payload, fixed trailer (4 bytes):

| Offset | Field          | Size | Semantics |
|--------|----------------|------|-----------|
| 0      | magic          | 4B   | ASCII `XACP` (0x58 0x41 0x43 0x50) |
| 4      | version_major  | 1B   | Breaking frame-format changes; mismatch ⇒ connection refused |
| 5      | version_minor  | 1B   | Backward-compatible extensions; unknown frame types are ignored |
| 6      | frame_type     | 2B   | Frame role (control / data classes, §4) |
| 8      | transaction_id | 16B  | UUID; globally unique; survives reconnection (resume anchor) |
| 24     | total_len      | 4B   | Total transaction bytes (u32; per-transaction cap 4 GiB — larger content is split into multiple transactions by the message layer) |
| 28     | seq            | 4B   | Frame sequence number within the transaction, starting at 0 |
| 32     | frame_len      | 4B   | Payload byte count of this frame |
| 36     | payload        | var  | — |
| end    | crc32          | 4B   | Covers offset 0 through end of payload |

Rules:

- Big-endian byte order throughout. Fixed overhead: 40 bytes per frame.
- Reassembly criteria: contiguous `seq` AND `sum(frame_len) == total_len`.
- Receivers process streams incrementally; buffers are never pre-allocated
  from declared lengths.
- Receivers enforce a configurable decode buffer cap (RECOMMENDED default
  64 MiB): any frame whose total size (header + frame_len + trailer) exceeds
  the cap is a protocol error, regardless of frame type. This bounds memory
  exposure to unknown frame types and hostile declarations; per-type limits
  (§4) remain the tighter bound for known types.
- Frames of different transactions may interleave on the wire
  (`transaction_id` is carried per-frame); bulk-data frames yield to control
  frames in the sender schedule.

## 4. Frame Types

Control class:

- `NEGOTIATE` — capability exchange (commands/events) and protocol feature
  negotiation.
- `ACK` — receipt confirmation.
- `NACK` — missing-frame report (gap list).
- `RESUME` — reconnection progress handshake (receiver → holder).
- `RESUME_RESPONSE` — reconnection progress answer (holder → receiver).
- `HEARTBEAT` — connection liveness.
- `ERROR` — protocol-level error.
- `SECURITY_UPGRADE` — reserved for runtime session-security upgrade.

Data class:

- `DATA_JSON` — message-semantics layer JSON payload.
- `DATA_BINARY` — raw bytes (multimodal content).

Per-type payload size limits (protocol-level validation; an oversized frame is
a protocol error):

| Frame class | Max payload size | Rationale |
|---|---|---|
| Control frames | 16 KiB | Control payloads are metadata-sized; headroom covers negotiation lists, batched receipts, and future security handshakes |
| `DATA_JSON` | 1 MiB | Message-layer JSON with 10× headroom over current largest messages |
| `DATA_BINARY` | 16 MiB | Bulk transfer ceiling; implementations SHOULD use ~1 MiB working frames to keep control-frame insertion latency bounded on slow links |

Control-frame payload design rule: control frames carry lightweight control
semantics only — receipts, bitmaps, gap lists, references, fingerprints,
digests. Bulk content (for example certificates) is transported as data
transactions; control frames reference it.

## 5. Reliability

- Senders retain unacknowledged transaction copies and retransmit on timeout.
- Receivers reassemble per transaction and return `ACK` (receipt) / `NACK`
  (gap list) feedback.
- Reconnection resume: a new connection carries `transaction_id` plus received
  progress (`RESUME` handshake); the transfer continues from the breakpoint.
- `HEARTBEAT` maintains liveness detection.
- Application-layer backpressure: unacknowledged transaction bytes are capped
  (RECOMMENDED default 64 MiB, configurable). Senders pause sending pending
  ACK when the cap is reached; the cap also bounds retransmission storms.

### Control frame payload layouts

All integers big-endian. Control payloads are bounded by the 16 KiB
control-frame cap (§4).

**ACK** — transaction progress receipt (cumulative):

```text
u8   count                    (1..=255 entries)
per entry:
  u8[16] transaction_id
  u32    highest_contiguous_seq  (all frames 0..=seq are received)
  u32    received_frames         (total frames received; detects sparse residue)
```

A transaction whose received frames do not include a contiguous prefix
from frame 0 exports no ACK entry — a cumulative
`highest_contiguous_seq` cannot express "no prefix". Such transactions
are surfaced through NACK gap lists instead.

**NACK** — missing frame request:

```text
u8   count                    (1..=255 entries)
per entry:
  u8[16] transaction_id
  u16    missing_count
  u32[]  missing_seqs             (missing_count entries)
```

**RESUME / RESUME_RESPONSE** — reconnection progress handshake. The
receiver of an incomplete transaction declares progress (`RESUME`); the
holder of the copy answers per entry (`RESUME_RESPONSE`) and retransmits
the missing frames. Distinct frame types — no payload-shape guessing.

```text
RESUME (receiver → holder):
  u8   count                  (0..=255 entries; 0 = nothing to resume)
  per entry: ACK entry shape (transaction_id, highest_contiguous_seq, received_frames)
```

```text
RESUME_RESPONSE (holder → receiver):
  u8   count                  (0..=255 entries)
  per entry:
    u8[16] transaction_id
    u8     status             (0x01 resumed — retransmission follows;
                               0x02 rejected — copy no longer held, receiver discards;
                               0x03 unknown)
```

A transaction whose received frames have no contiguous prefix from
frame 0 exports no RESUME entry either (same u32 expressiveness limit
as ACK); the holder's retransmission timeout covers it.

**HEARTBEAT** — unidirectional liveness probe (each side sends its own;
no echo):

```text
u32  monotonic_seq
u64  sender_epoch_ms
```

**ERROR** — protocol-level error:

```text
u16  code
u8[] message                  (UTF-8, may be empty)
```

Error code registry: `0x0001` protocol_version_unsupported,
`0x0002` transaction_expired, `0x0003` backpressure_stall. The registry
is append-only. For `0x0002`, the message field starts with the affected
transaction id as 32 lowercase hex chars (the remainder is free-form);
receivers parse it structurally.

### Reliability semantics

- ACK entries are cumulative; a transaction is complete when
  `highest_contiguous_seq` covers its final frame and reassembly
  criteria (§3) hold. The receiver sends incremental ACKs periodically
  (RECOMMENDED every 64 frames or 500 ms, whichever first) and a final
  ACK on completion.
- The final ACK attests reassembly completion, not consumer-side
  delivery: completed payloads are retained on the receiver until the
  application handler consumes them (or the session ends); a handler
  registered after completion still receives the payload exactly once.
- Senders release transaction copies on final ACK, or on RESUME
  rejection by the peer. RECOMMENDED copy retention after connection
  loss: 120 s (configurable) — expired copies answer future RESUME
  requests with `rejected`.
- Retransmission: RECOMMENDED initial timeout 2 s, exponential backoff
  ×2 up to 30 s (both configurable). A NACK triggers immediate
  retransmission of the listed frames.
- Backpressure: senders pause DATA frames while unacknowledged
  transaction bytes exceed the cap (§3); control frames are exempt and
  interleaved (performance invariant 4). The pause may surface either as
  an immediate retryable error from the send API or as internal queuing
  with automatic flush once ACKs release copies — both forms are
  conformant (wire behaviour is identical).
- Liveness: peers exchange HEARTBEAT frames; any inbound frame refreshes
  the peer-activity clock. RECOMMENDED heartbeat interval 30 s, silent
  timeout 90 s (both configurable); silence past the timeout declares the
  connection dead (teardown, resume on reconnect).
- On reconnection, a receiver sends RESUME requests for each incomplete
  transaction before accepting new data flows on that logical channel;
  the holder answers per entry, then retransmits.
- Reliability feature negotiation travels in the envelope Negotiate
  command (message-semantics layer); the `NEGOTIATE` frame type stays
  reserved in v1.0.

### Performance invariants (normative):

1. Senders never block waiting for ACKs — asynchronous pipelining;
   retransmission occurs only after timeout.
2. Transactions deliver independently; no cross-transaction head-of-line
   blocking.
3. Streaming small messages are not batched, or only with a very short window.
4. Control frames take scheduling priority and may interleave between data
   frames of in-flight transactions.

## 6. Session Security (Optional)

- Security is opt-in: consumers explicitly enable it; the default wire mode
  is plaintext (intranet-first).
- Design stance: relays are untrusted. End-to-end encryption ensures any relay
  — and any intermediate node — sees ciphertext only. Privacy is a first-order
  requirement, independent of who operates the relay.
- Identity: trust establishment exchanges long-term identities and pins them
  mutually. Trust anchors are modeled as peer identities, decoupled from
  connection topology: direct and relayed connections consume the same
  identity store.
- Keys: per-session ephemeral key exchange (forward secrecy); long-term
  identities are used for authentication only, never directly for encryption.
- Integration point: the security handshake integrates into the Establish
  stage of the existing connect → negotiate → establish flow (which already
  carries credentials and a challenge/response path), or via the
  `SECURITY_UPGRADE` control frame.
- First-time trust establishment is performed in trusted environments
  (localhost / intranet); the flow reserves room for out-of-band fingerprint
  confirmation.
- Downgrade defense (applies once negotiation-based upgrade exists): pinned
  records carry a "security required" flag; pinned peers are never downgraded
  to plaintext.
- Deployment constraint: any deployment crossing public networks — including
  relay topologies — MUST enable session security.

Implementation options for the session layer (TLS-over-arbitrary-streams
double-layer TLS, or a Noise-protocol-framework session) are selected by the
implementing library; both satisfy the requirements above.

## 7. Carrier Matrix

| Carrier | Session security | Frame CRC role |
|---|---|---|
| TCP direct | Optional (plaintext default) | Primary integrity defense in plaintext mode |
| TCP via relay (future) | Required | Defense in depth |
| stdio | Not applicable (local pipe; no threat model) | Primary integrity defense |

## 8. Versioning

- Library 1.x releases carry protocol major 1; the frame header version and
  the library version advance together.
- `version_major` mismatch ⇒ connection refused.
- `version_minor`: a lower implementation tolerates a higher peer by ignoring
  unknown frame types and unrecognized fields.

## 9. Terminology

- **round** — unique identifier of one engine run (turn-level identity).
  Event payload field; unrelated to connection or security.
- **pair** — unique identifier of one loop iteration within a round. Event
  payload field; unrelated to connection or security.
- **transaction** — transport-layer framing/reassembly unit (frame header
  `transaction_id`); a different layer from round/pair.
- **trust establishment** — security-layer trust-anchor exchange. Deliberately
  distinct from `pair`, which is reserved by the event vocabulary.
