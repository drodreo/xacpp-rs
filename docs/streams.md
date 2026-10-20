# Streaming Transfer & Stream Lifecycle

Binary streams are the API-level shape of a binary transaction (SPEC §5).
This document pins the lifecycle contract of the stream objects (`BinarySink`
on the sender, `BinaryStream` on the receiver); the wire machinery itself
(frames, ACK/NACK, recovery handshake) lives in SPEC §3–§5.

## Session-bound lifetime

A stream lives with its **logical session**, not with any single connection
("session 不死，流不灭"):

- A disconnect **stalls** a live stream: chunks stop flowing, the object and
  its buffered bytes stay alive, and the state clears back when the session
  is re-wired. The same stream object resumes — a transaction never spawns
  a second stream object.
- The sender side is symmetric: sink writes across a disconnect **park**
  (they neither fail nor emit) and drain after the recovery handshake on
  the re-wired connection.

## Terminal states

A stream closes only on **completion** or **cancellation** — never on a
disconnect:

- **Complete** — every declared byte arrived (SPEC §3 criteria). All bytes
  delivered in order.
- **Cancelled** — the transaction can never complete: holder
  `TRANSACTION_ABORT`, the recovery cross-check (a RESUME declaration that
  did not cover this still-open transaction), a peer ERROR
  `transaction_expired`, or session death. Partial bytes are discarded.
- **Incomplete** — the holder announced completion (`TRANSACTION_COMPLETED`)
  but the received bytes fall short of the declared total. The stream ends
  explicitly with the byte gap readable (`received`/`expected`) instead of
  waiting for bytes that will never come.

## Interaction with the recovery handshake

The recovery discipline (SPEC §5) is what keeps this contract across a
reconnect:

- The holder's declaration gates its pending sends; the receiver's verdict
  re-arms transmission of the recognized ones — the same stream objects
  continue.
- An open inbound transaction the declaration does not cover is cancelled
  by the cross-check (its holder is gone), which is how a stream whose
  holder vanished still reaches a terminal state instead of hanging.

## Copy discipline

Sender copies live exactly as long as their transaction (no retention
deadline, no expiry sweep — SPEC §5). Retention is per-frame: frames at or
below the confirmed ACK frontier release their payload eagerly, so memory
cost is bounded by the unconfirmed window, not the transaction size.
