//! Capabilities type for the Negotiate phase.
//!
//! Capabilities are exchanged during the Negotiate phase (before Establish),
//! allowing each side to declare what commands it can handle and what events
//! it may emit.

use serde::{Deserialize, Serialize};

/// Capabilities declared by one side during Negotiate.
///
/// Each side sends one `Capabilities` to the peer:
/// - `commands`: JSON Schemas for commands this side can handle (tool-like).
/// - `events`: JSON Schemas for events this side may emit (contract description).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Capabilities {
    /// JSON Schemas for commands this side can handle.
    /// Each schema is tool-compatible, allowing direct conversion to a tool definition.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub commands: Vec<serde_json::Value>,

    /// JSON Schemas for events this side may emit.
    /// Describes the data structure contract; the receiving side decides how to route.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub events: Vec<serde_json::Value>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_capabilities_serde_roundtrip() {
        let caps = Capabilities {
            commands: vec![
                serde_json::json!({
                    "name": "new_activity",
                    "description": "Create a new activity",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "title": { "type": "string" }
                        }
                    }
                }),
            ],
            events: vec![
                serde_json::json!({
                    "name": "content_delta",
                    "description": "Content delta output",
                    "data": {
                        "type": "object",
                        "properties": {
                            "content": { "type": "string" }
                        }
                    }
                }),
            ],
        };
        let json = serde_json::to_string(&caps).unwrap();
        let deserialized: Capabilities = serde_json::from_str(&json).unwrap();
        assert_eq!(caps, deserialized);
    }

    #[test]
    fn test_capabilities_empty_defaults() {
        let json = r#"{}"#;
        let caps: Capabilities = serde_json::from_str(json).unwrap();
        assert!(caps.commands.is_empty());
        assert!(caps.events.is_empty());
    }
}
