//! XACPP Transport Abstraction.
//!
//! ## Scope of Responsibilities
//!
//! Transport unifies underlying communication channels (stdio / TCP / WebSocket) into `send` semantics:
//!
//! - **send**: Send request payload, wait for response payload. Caller can spawn to background if not interested in response.
//! - **send_faf**: Send request payload fire-and-forget — returns as soon as the frame is queued, never waits for the response.
//! - **accept**: Listen for peer requests, distribute via registered `on_request` callback,
//!   callback receives session_id + payload, return value is automatically sent back as response.
//!
//! Transport internally handles:
//!
//! - **Ordered egress**: all outbound frames (requests, events, responses) go through a single
//!   FIFO egress queue — one drain task per connection owns the write half and writes frames
//!   in submission order (see `egress` module)
//! - **Envelope assembly/disassembly**: Auto-assign request id, pack into envelope for sending, unpack envelope to return payload
//! - **Request-response correlation**: Match incoming Response to pending send via id
//! - **Encoding/decoding**: Serialize envelopes to JSON and carry them as
//!   `DATA_JSON` binary frames (see [`encode_data_json_frame`] and the
//!   `frame` module; SPEC §3/§4)
//! - **Connection management**: Establish / disconnect underlying communication channel
//! - **Wiring surface**: transports expose the narrow [`TransferWiring`]
//!   surface (write control/data frames, register inbound hooks) to a wired
//!   [`TransferSession`] (see `XacppSession::wire`) — the session
//!   self-drives its clock, RESUME handshakes, and inbound routing
//!
//! ## Layer Boundary
//!
//! - **Transport upward**: Expose `send` / `send_faf` / `on_request`,
//!   do not expose raw byte send/receive, envelope id, or encoding/decoding details
//! - **Peer upward**: Expose typed `request_command` / `request_event` / `send_event`
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

pub(crate) mod egress;
pub mod socket;
pub mod stdio;
pub mod transfer;

pub use transfer::{
    BinaryHandler, BinarySendResult, CompletionCallback, TransferActivityHandlers, TransferConfig,
    TransferSession, TransferWiring,
};

use async_trait::async_trait;

use crate::error::XacppError;
use crate::message::{XacppRequest, XacppResponse};

// Re-export: RequestHandler is defined in handler module, transport submodules reference via super::
pub use crate::handler::RequestHandler;

/// Encode a serialized envelope payload as a single-frame `DATA_JSON`
/// transaction on the binary wire (SPEC §3/§4).
///
/// One envelope = one transaction = one frame: `seq = 0`,
/// `total_len = payload length`, and `transaction_id` is a fresh UUID v4
/// per frame (uniqueness keeps frames self-describing even though the
/// resume anchor is only consulted for multi-frame `DATA_BINARY`
/// transactions).
///
/// Payloads exceeding the `DATA_JSON` per-frame limit (1 MiB, §4) fail with
/// [`XacppError::Internal`]: per SPEC §3, larger content must be split into
/// multiple transactions by the message layer, which is out of scope until
/// the transfer (multi-frame reassembly) batch.
pub(crate) fn encode_data_json_frame(payload: &[u8]) -> Result<Vec<u8>, XacppError> {
    let frame = crate::frame::Frame::new(
        crate::frame::FrameType::DataJson,
        uuid::Uuid::new_v4().into_bytes(),
        // Cast is lossless: encode() below rejects anything over the 1 MiB
        // `DATA_JSON` limit, far below u32::MAX.
        payload.len() as u32,
        0,
        payload.to_vec(),
    );
    frame
        .encode()
        .map_err(|e| XacppError::Internal(e.to_string()))
}

/// XACPP Transport Layer Abstraction.
///
/// Specific implementations encapsulate all underlying fault tolerance logic (retry, backoff, reconnect, etc.),
/// exposing only two results to upper layer: `Ok` = normal, `Err` = connection abnormal.
///
/// The transfer-session attachment surface is the separate narrow
/// [`TransferWiring`] trait (write control/data frames + register inbound
/// hooks); the supertrait bound expresses the TS
/// `XacppTransport & TransferWiring` intersection: every transport also
/// carries the wiring surface. Wiring lifecycle (clock, RESUME, inbound
/// routing) lives on the session side — see `XacppSession::wire`.
#[async_trait]
pub trait XacppTransport: TransferWiring {
    /// Establish underlying communication channel and start accept loop.
    async fn connect(&self) -> Result<(), XacppError>;

    /// Disconnect underlying communication channel.
    ///
    /// Detaches any wired transfer session automatically (the session's
    /// transaction state survives — it lives on the session side).
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

    /// Send request payload fire-and-forget: returns as soon as the frame is
    /// queued for writing, never waits for the response.
    ///
    /// Ordering guarantee: the frame is written after all previously queued
    /// outbound frames and before all subsequently queued ones (single FIFO
    /// egress per connection). `Err` = connection already known dead.
    ///
    /// Default implementation falls back to a full round trip, dropping the
    /// response (correct but does not unlock the caller).
    async fn send_faf(
        &self,
        session_id: Option<&str>,
        payload: XacppRequest,
    ) -> Result<(), XacppError> {
        self.send(session_id, payload).await.map(|_| ())
    }

    /// Register Request callback (unified handling of Command and Event).
    ///
    /// Must be called before `connect`, otherwise returns `Err(XacppError::AlreadyConnected)`.
    /// When handler returns `Ok`, Transport auto-packs into envelope with same id and sends back;
    /// when handler returns `Err`, Transport auto-constructs Error response and sends back.
    fn on_request(&self, handler: RequestHandler) -> Result<(), XacppError>;

    /// Subscribe to connection-close notification.
    ///
    /// Yields once the connection is known dead (peer disconnect observed by
    /// the reader, write failure in the egress drain, or local `disconnect`).
    /// Check `*rx.borrow()` after subscribing: the connection may already be
    /// closed before the subscription.
    fn closed(&self) -> tokio::sync::watch::Receiver<bool>;
}
