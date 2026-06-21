//! Capabilities type for the Negotiate phase.
//!
//! Capabilities are exchanged during the Negotiate phase (before Establish),
//! allowing each side to declare what commands it can handle and what events
//! it can produce or accept.

use serde::{Deserialize, Serialize};

/// Capabilities declared by one side during Negotiate.
///
/// Each side sends one `Capabilities` to the peer:
/// - `commands`: JSON Schemas for commands this side can handle (tool-like).
/// - `produce_events`: JSON Schemas for events this side may emit (I will send these).
/// - `accept_events`: JSON Schemas for events this side can receive (I can handle these).
///
/// The effective event capability is the intersection of local `produce_events`
/// and remote `accept_events`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Capabilities {
    /// JSON Schemas for commands this side can handle.
    /// Each schema is tool-compatible, allowing direct conversion to a tool definition.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub commands: Vec<serde_json::Value>,

    /// JSON Schemas for events this side may emit (produce).
    /// Describes the data structure contract; the receiving side decides how to route.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub produce_events: Vec<serde_json::Value>,

    /// JSON Schemas for events this side can receive (accept).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub accept_events: Vec<serde_json::Value>,
}

/// Negotiate 完成后的生效能力（应用层可见的）。
///
/// 由协议层在 negotiate 阶段自动计算。
#[derive(Debug, Clone, Default)]
pub struct EffectiveCapabilities {
    /// 对端能处理的命令 schema 列表（来自 remote.commands）。
    /// 应用层可以用这些 schema 向对端发 command。
    pub remote_commands: Vec<serde_json::Value>,

    /// 我能发给对端的事件名列表（local.produce_events ∩ remote.accept_events）。
    /// sendEvent 时协议层会校验事件名是否在此列表中。
    pub emit_events: Vec<String>,
}

impl EffectiveCapabilities {
    /// 从 local 和 remote capabilities 计算 effective。
    pub fn from_capabilities(local: &Capabilities, remote: &Capabilities) -> Self {
        let remote_commands = remote.commands.clone();
        let local_produce = extract_names(&local.produce_events);
        let remote_accept = extract_names(&remote.accept_events);
        let emit_events = local_produce
            .into_iter()
            .filter(|name| remote_accept.contains(&name))
            .collect();
        Self { remote_commands, emit_events }
    }
}

/// 从 JSON Schema 数组中提取 name 字段。
fn extract_names(schemas: &[serde_json::Value]) -> Vec<String> {
    schemas
        .iter()
        .filter_map(|s| s.get("name").and_then(|n| n.as_str()).map(String::from))
        .collect()
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
            produce_events: vec![
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
            accept_events: vec![
                serde_json::json!({
                    "name": "upload",
                    "description": "Upload request",
                    "data": {
                        "type": "object",
                        "properties": {
                            "url": { "type": "string" }
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
        assert!(caps.produce_events.is_empty());
        assert!(caps.accept_events.is_empty());
    }

    #[test]
    fn test_effective_capabilities_intersection() {
        // Local: can produce content_delta, think, info
        let local = Capabilities {
            commands: vec![],
            produce_events: vec![
                serde_json::json!({ "name": "content_delta" }),
                serde_json::json!({ "name": "think" }),
                serde_json::json!({ "name": "info" }),
            ],
            accept_events: vec![],
        };

        // Remote: can accept content_delta, info, error
        let remote = Capabilities {
            commands: vec![serde_json::json!({ "name": "new_activity" })],
            produce_events: vec![],
            accept_events: vec![
                serde_json::json!({ "name": "content_delta" }),
                serde_json::json!({ "name": "info" }),
                serde_json::json!({ "name": "error" }),
            ],
        };

        let effective = EffectiveCapabilities::from_capabilities(&local, &remote);

        // remote_commands should contain the command schemas from remote
        assert_eq!(
            effective.remote_commands,
            vec![serde_json::json!({ "name": "new_activity" })]
        );

        // emit_events should be intersection: content_delta and info
        assert_eq!(effective.emit_events, vec!["content_delta", "info"]);
    }

    #[test]
    fn test_effective_capabilities_empty_intersection() {
        let local = Capabilities {
            commands: vec![],
            produce_events: vec![serde_json::json!({ "name": "think" })],
            accept_events: vec![],
        };

        let remote = Capabilities {
            commands: vec![],
            produce_events: vec![],
            accept_events: vec![serde_json::json!({ "name": "content_delta" })],
        };

        let effective = EffectiveCapabilities::from_capabilities(&local, &remote);

        assert!(effective.remote_commands.is_empty());
        assert!(effective.emit_events.is_empty());
    }

    #[test]
    fn test_effective_capabilities_default() {
        let effective = EffectiveCapabilities::default();
        assert!(effective.remote_commands.is_empty());
        assert!(effective.emit_events.is_empty());
    }
}
