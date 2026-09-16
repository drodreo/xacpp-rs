//! XACPP protocol messages — Envelope layer design.
//!
//! All messages transmitted on the wire are unified as `XacppEnvelope`, divided into two layers:
//!
//! - **Envelope layer**: `type` (routing) + `id` (correlation) + `payload` (business content)
//! - **Payload layer**: `XacppRequest` / `XacppResponse`
//!
//! ## Design
//!
//! Protocol responses (Negotiated, Established, EstablishPrepare, EstablishReject) are typed
//! variants handled by the Peer layer during connection setup.
//!
//! Business responses use the `Generic { name, data }` variant — a uniform structure
//! where `name` identifies the response type and `data` carries the response-specific payload.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::capability::Capabilities;
use crate::commands::XacppCommand;
use crate::events::activity_event::XacppActivityEvent;

// ---- Payload Types ----

/// Request payload.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", content = "payload", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)]
pub enum XacppRequest {
    /// Protocol or business command.
    Command(XacppCommand),
    /// Activity event (one-way notification).
    Event(XacppActivityEvent),
}

/// Response payload.
#[derive(Debug, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum XacppResponse {
    // ---- Protocol responses (Peer layer) ----
    /// Capability negotiation response.
    Negotiated { capabilities: Capabilities },

    /// Handshake successful: session identifier and credentials issued.
    Established {
        session_id: String,
        credentials: String,
    },

    /// Challenge issued during first-time establishment.
    EstablishPrepare { challenge: String },

    /// Handshake rejected.
    EstablishReject { reason: String },

    // ---- Business response (SessionHandler layer) ----
    /// Generic business response.
    ///
    /// `name` identifies the response type (e.g. "activity_ready", "acknowledge", "action").
    /// `data` carries the response-specific JSON payload.
    Generic {
        name: String,
        #[serde(default)]
        data: Value,
    },

    /// Processing failed.
    Error { code: String, message: String },
}

impl XacppResponse {
    /// Convenience: creates a generic acknowledge response.
    pub fn acknowledge() -> Self {
        XacppResponse::Generic {
            name: "acknowledge".to_string(),
            data: Value::Null,
        }
    }

    /// Convenience: creates a generic response with name and data.
    pub fn generic(name: impl Into<String>, data: Value) -> Self {
        XacppResponse::Generic {
            name: name.into(),
            data,
        }
    }

    /// Convenience: creates an error response.
    pub fn error(code: impl Into<String>, message: impl Into<String>) -> Self {
        XacppResponse::Error {
            code: code.into(),
            message: message.into(),
        }
    }
}

// ---- Envelope Types ----

/// Wire message.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_envelope_negotiated_response_deserialize_xabot_wire() {
        let json = r#"{"type":"response","id":"r1","payload":{"kind":"negotiated","capabilities":{"commands":[],"produceEvents":[],"acceptEvents":[]}}}"#;
        let env: XacppEnvelope = serde_json::from_str(json).unwrap();
        match env {
            XacppEnvelope::Response { id, payload, .. } => {
                assert_eq!(id, "r1");
                assert!(matches!(payload, XacppResponse::Negotiated { .. }));
            }
            _ => panic!("expected Response"),
        }
    }

    #[test]
    fn test_response_generic_roundtrip_xabot_wire() {
        let wire = r#"{"kind":"generic","name":"activity_ready","data":{"activity":"act-1"}}"#;
        let resp: XacppResponse = serde_json::from_str(wire).unwrap();
        let ser = serde_json::to_string(&resp).unwrap();
        assert!(ser.contains(r#""kind":"generic""#), "ser={}", ser);
        assert!(ser.contains(r#""name":"activity_ready""#), "ser={}", ser);
    }
}
