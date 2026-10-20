//! XACPP Event type.
//!
//! Events are one-way notifications: the transport layer auto-acknowledges
//! receipt, but the sender does not block for a meaningful response.
//!
//! All business events use a generic `{ name, data }` structure.
//! The `name` field identifies the event type; `data` carries the
//! event-specific JSON payload.
//!
//! Type definitions for common event payloads are kept in sibling modules
//! (e.g. `payload.rs`, `content.rs`, `upload.rs`) and serve as
//! serialization/deserialization targets for `data`.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// XACPP protocol event (generic).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct XacppEvent {
    /// Event type identifier (e.g. "content_delta", "tool_use", "complete").
    pub name: String,
    /// Event-specific JSON payload.
    #[serde(default)]
    pub data: Value,
}

impl XacppEvent {
    /// Convenience constructor.
    pub fn new(name: impl Into<String>, data: Value) -> Self {
        XacppEvent {
            name: name.into(),
            data,
        }
    }
}
