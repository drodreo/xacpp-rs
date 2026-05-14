//! XACPP — Agent Control Plane Protocol.
//!
//! XACPP is the communication protocol library between an agent and its peers.
//! Its responsibility is defining protocol types and providing standardized implementations
//! for the transport layer and protocol layer.
//!
//! # Three-Layer Architecture
//!
//! ```text
//! ┌──────────────────────────────────────────────────────────────┐
//! │  Peer (Protocol Layer)                                        │
//! │  Typed operations: request_command / request_event            │
//! │  Session routing: session_id → XacppSessionHandler           │
//! ├──────────────────────────────────────────────────────────────┤
//! │  Session (Session Layer)                                      │
//! │  Holds XacppSessionHandler, sends directly to Transport       │
//! │  Does not go through Peer                                     │
//! ├──────────────────────────────────────────────────────────────┤
//! │  Transport (Transport Layer)                                  │
//! │  Single semantic: send (request-response), caller chooses     │
//! │  whether to wait for response                                │
//! │  Internally: envelope assembly/disassembly (id correlation), │
//! │    encoding/decoding, pending matching                       │
//! │  Unaware of specific business event types                     │
//! ├──────────────────────────────────────────────────────────────┤
//! │  Underlying Pipe (Stdio / TCP / WebSocket)                   │
//! │  Pure byte stream I/O, frame splitting (JSONL)              │
//! │  Unaware of requests, responses, or events                    │
//! └──────────────────────────────────────────────────────────────┘
//! ```
//!
//! ## Wire Message Format
//!
//! All wire messages are envelope structures (`XacppEnvelope`),
//! the `type` field routes requests/responses, `payload` carries the payload:
//!
//! ```json
//! Request: {"type":"request","id":"r1","payload":{"kind":"command","payload":{"establish":{"credentials":null}}}}
//! Request: {"type":"request","id":"r2","session_id":"s1","payload":{"kind":"event","payload":{"type":"think","content":"hi"}}}
//! Response: {"type":"response","id":"r1","payload":{"kind":"established","sessionId":"s1","credentials":null}}
//! Response: {"type":"response","id":"r2","session_id":"s1","payload":{"kind":"action","requestId":"req-1","type":"approve"}}
//! ```
//!
//! Transport automatically assigns and matches envelope ids; upper layers only operate on payloads.
//!
//! ## Layer Boundary
//!
//! - **Transport to upper**: exposes `send` / `on_request`,
//!   does not expose envelope id or encoding/decoding details
//! - **Peer to upper**: exposes typed `establish` / `request_command` / `request_event`
//! - **Session to upper**: holds `XacppSessionHandler`, sends/receives directly through Transport

pub mod commands;
pub mod error;
pub mod events;
pub mod handler;
pub mod message;
pub mod peer;
pub mod session;
pub mod transport;
