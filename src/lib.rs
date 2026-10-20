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
//! │  Lifecycle orchestration: connect / negotiate / establish /    │
//! │  disconnect / state; session routing: sid → SessionHandler     │
//! ├──────────────────────────────────────────────────────────────┤
//! │  Session (Session Layer)                                      │
//! │  Owns the transfer state (SPEC §5); request_command /         │
//! │  request_event / send_event / send_binary to Transport        │
//! ├──────────────────────────────────────────────────────────────┤
//! │  Transport (Transport Layer)                                  │
//! │  Single semantic: send (request-response), caller chooses     │
//! │  whether to wait for response                                │
//! │  Internally: envelope assembly/disassembly (id correlation), │
//! │    encoding/decoding, pending matching                       │
//! │  Unaware of specific business event types                     │
//! ├──────────────────────────────────────────────────────────────┤
//! │  Underlying Pipe (Stdio / TCP / WebSocket)                   │
//! │  Pure byte stream I/O, binary frame codec (SPEC §3/§4)       │
//! │  Unaware of requests, responses, or events                    │
//! └──────────────────────────────────────────────────────────────┘
//! ```
//!
//! ## Wire Message Format
//!
//! Wire traffic is binary frames (SPEC §3/§4); envelopes (`XacppEnvelope`)
//! travel as the payload of `DATA_JSON` frames — the `type` field routes
//! requests/responses, `payload` carries the operation:
//!
//! ```json
//! Request: {"type":"request","id":"r1","payload":{"kind":"command","payload":{"establish":{}}}}
//! Request: {"type":"request","id":"r2","session_id":"s1","payload":{"kind":"event","payload":{"activity":{"id":"act-1"},"event":{"name":"think","data":null}}}}
//! Response: {"type":"response","id":"r1","payload":{"kind":"established","sessionId":"s1","credentials":"issued-creds"}}
//! Response: {"type":"response","id":"r2","payload":{"kind":"generic","name":"action","data":{"type":"approve"}}}
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

pub mod carrier;
pub mod ext {
    pub mod fetch;
}
pub mod reliability;
pub mod session;
pub mod transfer;
pub mod wire;
