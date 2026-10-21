# XACPP Binary Wire Protocol — Specification v1.1

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
- `RESUME` — reconnection declaration (holder → receiver): the pending
  sends the holder still holds.
- `RESUME_RESPONSE` — reconnection verdict (receiver → holder):
  per-entry adjudication; `resumed` is the resume instruction.
- `HEARTBEAT` — connection liveness.
- `ERROR` — protocol-level error.
- `TRANSACTION_PAUSE` (`0x0009`) — holder → receiver: transmission of
  the named transaction is paused.
- `TRANSACTION_ABORT` (`0x000A`) — holder → receiver: the named
  transaction is abandoned and its retained copy released.
- `TRANSACTION_COMPLETED` (`0x000B`) — holder → receiver: the holder
  declares the named transaction's data plane finished — every declared
  byte has been sent.
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

The API-level shape of a binary transaction is a stream object whose
lifecycle contract (session-bound lifetime, terminal states) is pinned in
[docs/streams.md](streams.md); this section pins the wire machinery.

- Senders retain unacknowledged transaction copies and retransmit on timeout.
- Receivers reassemble per transaction and return `ACK` (receipt) / `NACK`
  (gap list) feedback.
- Reconnection resume: on a new connection the holder gates its pending
  sends against retransmission and declares them (`RESUME`) once both
  transfer planes are wired, holding retransmission until the receiver's
  per-entry verdicts arrive (`RESUME_RESPONSE`); a `resumed` verdict lifts
  the gate and the transfer continues from the breakpoint (the holder's
  own ACK bookkeeping is the watermark — no offset rides the verdict).
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

**RESUME / RESUME_RESPONSE** — reconnection recovery handshake. At
wire-up the holder arms the reconnect gate on every pending send
(retransmission stays silent until the verdict). The holder sends the
declaration (`RESUME`) only once BOTH transfer planes are wired — a
declaration dropped by a not-yet-wired peer would deadlock the holder
awaiting a verdict that never arrives. On an establish topology this
means: the responder declares from its `on_establish` wire (the
initiator wired at connect), and the initiator declares when the
handshake completes (the responder wired inside `on_establish`). The
receiver answers one verdict per entry
(`RESUME_RESPONSE`). A `resumed` verdict is the resume instruction — the
sole retransmission trigger across a reconnect (blind retransmission of
retained copies over a fresh connection is retired): the holder lifts the
gate and retransmits every unconfirmed frame. An `unknown`/`rejected`
verdict fails the transfer as a whole: the holder releases the copy and
its completion waiters end unconfirmed (completion remains
final-ACK-only). Distinct frame types — no payload-shape guessing.

```text
RESUME (holder → receiver):
  u8   count                  (0..=255 entries; a holder with nothing
                               pending sends no frame at all)
  per entry: ACK entry shape (transaction_id + holder-side written
             progress — informational only; adjudication is by
             transaction_id alone, and the receiver's own ACK stream is
             the watermark, which is why no offset rides the verdict)
```

```text
RESUME_RESPONSE (receiver → holder):
  u8   count                  (one verdict per declaration entry — every
                               declaration is answered exactly once)
  per entry:
    u8[16] transaction_id
    u8     status             (0x01 resumed — the transaction is still open
                                 inbound; the holder resumes transmission;
                               0x03 unknown — not recognized; fail the
                                 transfer;
                               0x02 rejected — wire-compatibility value,
                                 same effect as unknown)
```

Receiver adjudication: a declared transaction the receiver still holds
open (incomplete) answers `resumed`; anything else answers `unknown` —
with no terminal trace kept, a consumed-and-delivered transaction and a
never-seen one are indistinguishable. Mirror cross-check: an inbound
transaction still open that the declaration does NOT cover can never
complete (the holder released or aborted its copy while disconnected) —
the receiver cancels its stream and drops the slot. Completed-but-
undelivered slots are excluded from the cross-check: they need nothing
more from the holder.

**HEARTBEAT** — unidirectional liveness probe (each side sends its own;
no echo):

```text
u32  monotonic_seq
u64  sender_epoch_ms
```

**TRANSACTION_PAUSE** — the holder has paused transmission of the named
transaction (new frames and retransmission of it both suspended):

```text
u8[16] transaction_id
```

**TRANSACTION_ABORT** — the holder has abandoned the named transaction
and released its retained copy; the receiver discards all received frames
of it:

```text
u8[16] transaction_id
```

**TRANSACTION_COMPLETED** — the holder declares the named transaction's
data plane finished (every declared byte has been sent):

```text
u8[16] transaction_id
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
- Senders release transaction copies on final ACK or on abort. A copy
  lives exactly as long as its transaction: no retention deadline, no
  expiry sweep — a paused transaction or a broken connection never
  invalidates a copy on its own, and resumption is available whenever
  the connection returns. The holder keeps no terminal trace: after a
  reconnect, a declared transaction the receiver no longer holds open
  (completed and consumed, or never seen) adjudicates `unknown` and the
  transfer fails as a whole; `rejected` remains a wire value for
  tolerance with the same effect.
- Retransmission: RECOMMENDED initial timeout 2 s, exponential backoff
  ×2 up to 30 s (both configurable). A NACK triggers immediate
  retransmission of the listed frames.
- Backpressure: senders pause DATA frames while unacknowledged
  transaction bytes exceed the cap (§3); control frames are exempt and
  interleaved (performance invariant 4). The pause may surface either as
  an immediate retryable error from the send API or as internal queuing
  with automatic flush once ACKs release copies — both forms are
  conformant (wire behaviour is identical).
- Pause/abort/completion: a holder may pause an in-flight transaction
  (suspending both new frames and retransmissions of it), abort it
  (releasing the retained copy), or finish it. After each action the
  holder MUST emit the matching control frame (`TRANSACTION_PAUSE` /
  `TRANSACTION_ABORT` / `TRANSACTION_COMPLETED`) so the receiver can
  capture the state; a finish whose written bytes fall short of the
  declared total is not a completion at all — the holder emits
  `TRANSACTION_ABORT` instead (a short write is an abnormal end, never
  a silent one). A paused transaction is distinct from a stalled peer —
  liveness detection is connection-level (HEARTBEAT) and unaffected.
  Resumption after pause is implicit: the next data frame of the
  transaction arrives. On abort the receiver discards the
  transaction's partial reassembly.
- Completion announcement semantics: `TRANSACTION_COMPLETED` is sent
  after the transaction's tail data frame. On the receive side, if the
  named transaction is live: bytes complete ⇒ the transaction completes
  (the byte-count path owns the verdict; a still-open stream is closed
  as complete, and a late announcement is ignored); bytes missing ⇒
  the stream ends explicitly as *incomplete* with the byte gap — the
  receiver never waits for bytes that will not come. A transaction with
  no live slot ignores the frame. The frame is NOT mandatory: a
  transaction whose bytes add up completes without it, so peers that
  never send it stay compatible. The incomplete verdict rests on the
  carrier being ordered and lossless (§7: TCP / stdio) — the
  announcement overtaking a data frame, or a data frame dying in
  transit, are both outside that premise.
- Transaction control frame scheduling: the three transaction control
  frames (`TRANSACTION_PAUSE` / `TRANSACTION_ABORT` /
  `TRANSACTION_COMPLETED`) travel in wire order behind the
  transaction's own queued data frames (an abort that overtook the
  first data frame would be ignored by a receiver that never saw the
  stream, leaving the late frame to open a stream that never
  terminates; a completion that overtook the tail would fake a byte
  shortfall). A sender MAY expedite one only when nothing of that
  transaction is queued ahead of it.
- Liveness: peers exchange HEARTBEAT frames; any inbound frame refreshes
  the peer-activity clock. RECOMMENDED heartbeat interval 30 s, silent
  timeout 90 s (both configurable); silence past the timeout declares the
  connection dead (teardown, resume on reconnect).
- On reconnection, the holder declares its pending sends (`RESUME`) on
  wire-up and holds their retransmission until the receiver's verdicts
  arrive (`RESUME_RESPONSE`); `resumed` lifts the gate and retransmits
  the unconfirmed frames, `unknown`/`rejected` fails the transfer.
  Connection-level traffic (HEARTBEAT/ACK/NACK) is not gated, and
  transactions opened on the new connection flow normally.
- Reliability feature negotiation travels in the envelope Negotiate
  command (message-semantics layer); the `NEGOTIATE` frame type stays
  reserved in v1.0.

### Performance invariants (normative):

1. Senders never block waiting for ACKs — asynchronous pipelining;
   retransmission occurs only after timeout.
2. Transactions deliver independently; no cross-transaction head-of-line
   blocking.
3. Streaming small messages are not batched, or only with a very short window.
4. Connection-level control frames take scheduling priority and may
   interleave between data frames of in-flight transactions; the three
   transaction control frames instead travel in wire order behind their
   transaction's data frames (see §5, completion announcement
   semantics).

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

## 8. Resource Fetch (fetch)

Fetch is a symmetric, stateless pull convention: one peer names a resource,
the other answers with metadata and then streams the bytes over the binary
channel. Roles are interchangeable; frame and command semantics are
identical in both directions. The kernel composes two existing primitives —
one command round-trip and one binary transaction — and introduces no new
frame types and no fetch-specific protocol state.

Fetch is business-neutral: the protocol pins only *how* a resource is
pulled. Which resources are fetchable, and when, is the consumer's business
decision and never appears at the protocol layer.

### 8.1 Sequence

1. **a → b: `fetch` command** — names the resource by `uri`. Multiple
   fetches may be in flight concurrently; fetch commands interleave freely
   with other traffic.
2. **b → a: `fetch_ready` response, or an error** — on success, b commits
   to streaming the resource and declares its metadata.
3. **a → b: `fetch_begin` command** — a confirms readiness to receive;
   b MUST NOT emit any frame of the transaction before receiving it.
4. **b → a: binary transaction** — b streams the resource via the binary
   channel (§5); the transaction id correlates the stream to the response.
5. **a: dispatch** — the transaction is delivered through the binary
   receive path carrying its transaction id. Everything afterwards —
   buffering, persistence, integrity verification, progress accounting —
   is an application-layer concern.

The stream ends when the binary transaction completes under §5 semantics.
There is no fetch-specific completion handshake, disconnect semantics, or
retry obligation: interruption and resume are inherited from §5 unchanged.

### 8.2 Wire format

Request (message-semantics command):

```json
{ "fetch": { "uri": "<string>" } }
```

Response — success:

```json
{ "kind": "fetch_ready", "txid": "<32 lowercase hex chars>",
  "sha256": "<64 lowercase hex chars>", "sizeBytes": 0, "mime": "<string>" }
```

Failure uses the generic error response with a registered code:

| code | semantics |
|---|---|
| `fetch_not_found` | uri unknown or resource gone |
| `fetch_refused` | uri outside the servable scope, or pause/resume not implemented |
| `fetch_unavailable` | fetch unsupported on this end (no fetch responder loaded) |

- `uri` is opaque to the protocol; its structure is an application-layer
  convention between the two ends.
- `txid` is generated by the responder (b) and MUST be unique among
  in-flight transactions on the connection; it IS the binary transaction
  id of step 4.
- The responder computes `sha256`/`sizeBytes` from the bytes it sends
  (self-computed at response time; never application-declared).

### 8.3 Companion commands

The fetch command set as a whole is protocol contract; *who* implements
these commands and *how* is the extension layer's business (§8.6). All
three are message-semantics commands addressed to the transaction holder:

```json
{ "fetch_begin":  { "txid": "<32 lowercase hex chars>" } }
{ "fetch_pause":  { "txid": "<32 lowercase hex chars>" } }
{ "fetch_resume": { "txid": "<32 lowercase hex chars>" } }
{ "fetch_abort":  { "txid": "<32 lowercase hex chars>" } }
```

- `fetch_begin` — stream-start authorization: the requester confirms
  readiness to receive the named transaction; the holder MUST NOT emit
  any frame of it before receiving this command. Idempotent: a repeated
  `fetch_begin` for an in-flight transaction answers success; an unknown
  or terminal txid answers `fetch_refused`.
- `fetch_pause` / `fetch_resume` — flow-control requests: the holder
  suspends / continues transmission of the named transaction.
- `fetch_abort` — stop and release: the holder ceases transmission of
  the named transaction and releases its resources. Idempotent: aborting
  a completed or unknown txid answers success.
- Generic `ok` / error responses apply; a holder that does not implement
  pause/resume answers `fetch_refused`.

Holder-side state notifications travel on the transport control plane,
not as commands: `TRANSACTION_PAUSE` / `TRANSACTION_ABORT` /
`TRANSACTION_COMPLETED` (§5) let the receiver capture the holder's
pause/abort/completion actions per transaction.

### 8.4 Ordering

The holder MUST NOT start the binary transaction before receiving
`fetch_begin`, and the requester issues `fetch_begin` only after
observing `fetch_ready`. This causality chain — not any channel-ordering
assumption — guarantees the requester observes the metadata before any
byte of that transaction. An
inbound binary transaction whose id matches no announced fetch is not a
fetch delivery; how it is handled is the consumer's business — the fetch
kernel imposes no buffering obligation.

### 8.5 Out of scope (contract firewall)

The kernel promises a binary stream, not a file delivery.
Request/stream correlation beyond txid carriage, buffering, disk writes,
sha256 verification, completion aggregation, and pause/resume/cancel
bookkeeping are application-layer concerns. A library may ship optional
extension modules for these conveniences; extensions MUST NOT change,
weaken, or add to any obligation in this section. Where an extension
does verify integrity: a transfer that ends short of the announced bytes
is *truncated* (§5, incomplete terminal), not corrupted — the consumer
must see that truncation explicitly (never a hang, never misreported as
a digest mismatch), and the retry-or-otherwise decision belongs to the
application.

## 9. Versioning

- Library 1.x releases carry protocol major 1; the frame header version and
  the library version advance together.
- `version_major` mismatch ⇒ connection refused.
- `version_minor`: a lower implementation tolerates a higher peer by ignoring
  unknown frame types and unrecognized fields.

## 10. Terminology

- **round** — unique identifier of one engine run (turn-level identity).
  Event payload field; unrelated to connection or security.
- **pair** — unique identifier of one loop iteration within a round. Event
  payload field; unrelated to connection or security.
- **transaction** — transport-layer framing/reassembly unit (frame header
  `transaction_id`); a different layer from round/pair.
- **trust establishment** — security-layer trust-anchor exchange. Deliberately
  distinct from `pair`, which is reserved by the event vocabulary.
