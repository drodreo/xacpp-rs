//! XACPP protocol messages — Envelope layer design.
//!
//! All messages transmitted on the wire are unified as `XacppEnvelope`, divided into two layers:
//!
//! - **Envelope layer**: `type` (routing) + `id` (correlation) + `payload` (business content)
//! - **Payload layer**: `XacppRequest` / `XacppResponse`
//!
//! Transport layer is responsible for envelope assembly/disassembly and id correlation; upper layers only operate on payloads.
//!
//! ## JSON Format Examples
//!
//! ```json
//! Request (Command): {"id":"r1","type":"request","payload":{"kind":"command","payload":"authenticate"}}
//! Response (Pairing):  {"id":"r1","type":"response","payload":{"kind":"pairing","code":"123456"}}
//! ```

use serde::{Deserialize, Serialize};

use crate::commands::XacppCommand;
use crate::events::XacppEvent;
use crate::events::{ActionResponse, QuestionResponse, SensitiveInfoOperationResponse};

// ---- Payload Types ----

/// Request payload.
///
/// Received by Transport's `send` method.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", content = "payload", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)] // Protocol serde type, Box has no deserialization benefit
pub enum XacppRequest {
    /// Protocol command.
    Command(XacppCommand),
    /// Protocol event.
    Event(XacppEvent),
}

/// Response payload.
///
/// Returned by Transport's `send` method.
/// After handler returns this type, Transport automatically packs it into an envelope with the same id and sends back.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", rename_all_fields = "camelCase")]
pub enum XacppResponse {
    /// Handshake successful: session identifier and credentials issued.
    Established {
        session_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        credentials: Option<String>,
    },
    /// Handshake rejected.
    EstablishReject {
        reason: String,
    },
    /// Tool invocation authorization response.
    Action {
        request_id: String,
        #[serde(flatten)]
        response: ActionResponse,
    },
    /// User question response.
    Question {
        request_id: String,
        #[serde(flatten)]
        response: QuestionResponse,
    },
    /// Sensitive information operation response.
    SensitiveInfoOperation {
        request_id: String,
        #[serde(flatten)]
        response: SensitiveInfoOperationResponse,
    },
    /// Generic acknowledge: request processed successfully, no data returned.
    Acknowledge,
    /// Processing failed.
    Error {
        code: String,
        message: String,
    },
}

// ---- Envelope Types ----

/// Wire message.
///
/// Envelope layer handles routing: `type` field distinguishes requests and responses,
/// `id` is used for correlation, `payload` carries business content.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum XacppEnvelope {
    Request {
        id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        payload: XacppRequest,
    },
    Response {
        id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        payload: XacppResponse,
    },
}
