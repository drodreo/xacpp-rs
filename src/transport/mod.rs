//! XACPP Transport Abstraction.
//!
//! ## Scope of Responsibilities
//!
//! Transport unifies underlying communication channels (stdio / TCP / WebSocket) into `send` semantics:
//!
//! - **send**: Send request payload, wait for response payload. Caller can spawn to background if not interested in response.
//! - **accept**: Listen for peer requests, distribute via registered `on_request` callback,
//!   callback receives session_id + payload, return value is automatically sent back as response.
//!
//! Transport internally handles:
//!
//! - **Envelope assembly/disassembly**: Auto-assign request id, pack into envelope for sending, unpack envelope to return payload
//! - **Request-response correlation**: Match incoming Response to pending send via id
//! - **Encoding/decoding**: Serialize / deserialize (JSONL)
//! - **Connection management**: Establish / disconnect underlying communication channel
//!
//! ## Layer Boundary
//!
//! - **Transport upward**: Expose `send` / `on_request`,
//!   do not expose raw byte send/receive, envelope id, or encoding/decoding details
//! - **Peer upward**: Expose typed `request_command` / `request_event`
//!   and session routing mechanism
//!
//! ## accept Semantics
//!
//! Transport listens for peer input, delivers (session_id, payload) to registered callback.
//! Handler returns `Ok(response payload)` or `Err(XacppError)`, Transport always constructs response envelope to send back.
//!
//! ## Error Semantics Convention
//!
//! **Connection-type `Err` = connection unavailable**. All fault tolerance logic is encapsulated within Transport implementation.
//! Upper layer only needs one rule: connection-type `Err` from method means connection abnormal.

pub mod socket;
pub mod stdio;

use async_trait::async_trait;

use crate::error::XacppError;
use crate::message::{XacppRequest, XacppResponse};

// Re-export: RequestHandler is defined in handler module, transport submodules reference via super::
pub use crate::handler::RequestHandler;

/// XACPP Transport Layer Abstraction.
///
/// Specific implementations encapsulate all underlying fault tolerance logic (retry, backoff, reconnect, etc.),
/// exposing only two results to upper layer: `Ok` = normal, `Err` = connection abnormal.
#[async_trait]
pub trait XacppTransport: Send + Sync {
    /// Establish underlying communication channel and start accept loop.
    async fn connect(&self) -> Result<(), XacppError>;

    /// Disconnect underlying communication channel.
    async fn disconnect(&self) -> Result<(), XacppError>;

    /// Send request payload and wait for response.
    ///
    /// Transport auto-assigns id, packs envelope, serializes and sends, registers pending, waits for response, unpacks envelope to return payload.
    /// Caller can spawn to background or ignore return value if not interested in response.
    async fn send(
        &self,
        session_id: Option<&str>,
        payload: XacppRequest,
    ) -> Result<XacppResponse, XacppError>;

    /// Register Request callback (unified handling of Command and Event).
    ///
    /// Must be called before `connect`, otherwise returns `Err(XacppError::AlreadyConnected)`.
    /// When handler returns `Ok`, Transport auto-packs into envelope with same id and sends back;
    /// when handler returns `Err`, Transport auto-constructs Error response and sends back.
    fn on_request(&self, handler: RequestHandler) -> Result<(), XacppError>;
}
