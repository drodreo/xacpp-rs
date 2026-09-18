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
        Self {
            remote_commands,
            emit_events,
        }
    }
}

/// 从 JSON Schema 数组中提取 name 字段。
fn extract_names(schemas: &[serde_json::Value]) -> Vec<String> {
    schemas
        .iter()
        .filter_map(|s| s.get("name").and_then(|n| n.as_str()).map(String::from))
        .collect()
}

// ---- Command declaration schema (typed view of `Capabilities::commands`) ----

/// Typed representation of one command declaration schema (an element of
/// `Capabilities::commands` / `EffectiveCapabilities::remote_commands`).
///
/// Purely additive: the raw `serde_json::Value` carrier is unchanged. Use
/// [`CommandDeclaration::from_value`] / [`CommandDeclaration::to_value`] to
/// convert between the raw and typed forms.
///
/// Forward compatibility:
/// - Unknown fields are ignored on deserialization.
/// - Unknown `dispatcher` / `extraScopes` values fall back to
///   [`Dispatcher::Unknown`] / [`ExtraScope::Unknown`] instead of failing.
///
/// Wire keys are camelCase (`evaluationPolicy`, `extraScopes`, ...); Rust
/// fields are snake_case bridged via `rename_all`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandDeclaration {
    /// Command name. Required.
    pub name: String,

    /// Human-readable description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,

    /// JSON Schema describing the parameters surface.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parameters: Option<serde_json::Value>,

    /// Dispatch surface. `Some(Tool)` routes the command into the model tool
    /// surface; `None` (absent on the wire) means bridge semantics (not in
    /// the tool surface).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dispatcher: Option<Dispatcher>,

    /// Evaluation policy attached by the declaring side.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evaluation_policy: Option<EvaluationPolicy>,

    /// Additional exposure scopes. Absent/empty = default conversation
    /// surface only.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_scopes: Vec<ExtraScope>,
}

impl CommandDeclaration {
    /// Parses a typed declaration from the raw JSON value carried in
    /// `Capabilities::commands` / `EffectiveCapabilities::remote_commands`.
    pub fn from_value(value: &serde_json::Value) -> Result<Self, serde_json::Error> {
        serde_json::from_value(value.clone())
    }

    /// Converts back to the raw JSON value form.
    pub fn to_value(&self) -> Result<serde_json::Value, serde_json::Error> {
        serde_json::to_value(self)
    }

    /// True when the command should be routed into the model tool surface.
    /// Only `Tool` is tool-facing: an absent dispatcher (bridge semantics),
    /// an explicit `Bridge`, and unknown dispatcher values are all not
    /// tool-facing.
    pub fn is_tool_facing(&self) -> bool {
        matches!(self.dispatcher, Some(Dispatcher::Tool))
    }

    /// The `requireToolCall` evaluation entry, if declared.
    pub fn require_tool_call(&self) -> Option<&RequireToolCall> {
        self.evaluation_policy
            .as_ref()
            .and_then(|p| p.require_tool_call.as_ref())
    }

    /// True when the command is additionally exposed in the compact scope.
    pub fn has_compact_scope(&self) -> bool {
        self.extra_scopes
            .iter()
            .any(|s| matches!(s, ExtraScope::Compact))
    }
}

/// Command dispatch surface.
///
/// The wire-known values are `"tool"` and `"bridge"`. Anything else (from a
/// newer peer) is preserved verbatim as `Unknown` — never a deserialization
/// failure. An absent dispatcher also means bridge semantics (`None`).
#[derive(Debug, Clone, PartialEq)]
pub enum Dispatcher {
    /// Routed into the model tool surface (`"tool"`).
    Tool,
    /// Bridge semantics, explicitly declared (`"bridge"`); same semantics as
    /// an absent dispatcher.
    Bridge,
    /// Unrecognized dispatcher string, preserved verbatim.
    Unknown(String),
}

impl Serialize for Dispatcher {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Dispatcher::Tool => serializer.serialize_str("tool"),
            Dispatcher::Bridge => serializer.serialize_str("bridge"),
            Dispatcher::Unknown(s) => serializer.serialize_str(s),
        }
    }
}

impl<'de> Deserialize<'de> for Dispatcher {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Ok(match s.as_str() {
            "tool" => Dispatcher::Tool,
            "bridge" => Dispatcher::Bridge,
            _ => Dispatcher::Unknown(s),
        })
    }
}

/// Additional exposure scope of a command declaration.
///
/// The only wire-known value is `"compact"` (compaction thread surface).
/// Anything else is preserved verbatim as `Unknown`.
#[derive(Debug, Clone, PartialEq)]
pub enum ExtraScope {
    /// Additionally exposed in the compact (compaction thread) surface.
    Compact,
    /// Unrecognized scope string, preserved verbatim.
    Unknown(String),
}

impl Serialize for ExtraScope {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            ExtraScope::Compact => serializer.serialize_str("compact"),
            ExtraScope::Unknown(s) => serializer.serialize_str(s),
        }
    }
}

impl<'de> Deserialize<'de> for ExtraScope {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Ok(match s.as_str() {
            "compact" => ExtraScope::Compact,
            _ => ExtraScope::Unknown(s),
        })
    }
}

/// Evaluation policy declared alongside a command.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EvaluationPolicy {
    /// Require the model to call a specific tool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub require_tool_call: Option<RequireToolCall>,
}

/// The single currently-defined evaluation entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RequireToolCall {
    /// Name of the tool the model must call.
    pub require: String,

    /// Message used to bounce the turn back when the tool was not called.
    pub on_failure: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_capabilities_serde_roundtrip() {
        let caps = Capabilities {
            commands: vec![serde_json::json!({
                "name": "new_activity",
                "description": "Create a new activity",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "title": { "type": "string" }
                    }
                }
            })],
            produce_events: vec![serde_json::json!({
                "name": "content_delta",
                "description": "Content delta output",
                "data": {
                    "type": "object",
                    "properties": {
                        "content": { "type": "string" }
                    }
                }
            })],
            accept_events: vec![serde_json::json!({
                "name": "upload",
                "description": "Upload request",
                "data": {
                    "type": "object",
                    "properties": {
                        "url": { "type": "string" }
                    }
                }
            })],
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

    #[test]
    fn test_remote_commands_full_schema() {
        // remote.commands contains a command with full schema (name + description + parameters)
        let local = Capabilities {
            commands: vec![],
            produce_events: vec![],
            accept_events: vec![],
        };
        let remote = Capabilities {
            commands: vec![serde_json::json!({
                "name": "new_activity",
                "description": "Create a new activity",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "title": { "type": "string" }
                    }
                }
            })],
            produce_events: vec![],
            accept_events: vec![],
        };

        let effective = EffectiveCapabilities::from_capabilities(&local, &remote);

        // Should preserve the full schema, not just the name field
        assert_eq!(effective.remote_commands.len(), 1);
        let cmd = &effective.remote_commands[0];
        assert_eq!(cmd["name"], "new_activity");
        assert_eq!(cmd["description"], "Create a new activity");
        assert!(cmd["parameters"].is_object());
    }

    // ---- CommandDeclaration ----

    #[test]
    fn test_command_declaration_full_roundtrip() {
        let json = serde_json::json!({
            "name": "new_activity",
            "description": "Create a new activity",
            "parameters": {
                "type": "object",
                "properties": { "title": { "type": "string" } }
            },
            "dispatcher": "tool",
            "evaluationPolicy": {
                "requireToolCall": {
                    "require": "report_to_user",
                    "onFailure": "You must reply via report_to_user."
                }
            },
            "extraScopes": ["compact"]
        });

        let decl = CommandDeclaration::from_value(&json).unwrap();
        assert_eq!(decl.name, "new_activity");
        assert_eq!(decl.description.as_deref(), Some("Create a new activity"));
        assert!(decl.parameters.as_ref().unwrap().is_object());
        assert_eq!(decl.dispatcher, Some(Dispatcher::Tool));
        let rtc = decl.require_tool_call().unwrap();
        assert_eq!(rtc.require, "report_to_user");
        assert_eq!(rtc.on_failure, "You must reply via report_to_user.");
        assert_eq!(decl.extra_scopes, vec![ExtraScope::Compact]);

        // Wire keys stay camelCase; a fully-populated declaration round-trips losslessly.
        let back = decl.to_value().unwrap();
        assert_eq!(back, json);

        let reparsed = CommandDeclaration::from_value(&back).unwrap();
        assert_eq!(decl, reparsed);
    }

    #[test]
    fn test_command_declaration_optional_fields_omitted_on_serialize() {
        let decl = CommandDeclaration {
            name: "cmd".into(),
            description: None,
            parameters: None,
            dispatcher: None,
            evaluation_policy: None,
            extra_scopes: vec![],
        };
        let back = decl.to_value().unwrap();
        assert_eq!(back, serde_json::json!({ "name": "cmd" }));
    }

    #[test]
    fn test_command_declaration_unknown_fields_tolerated() {
        let json = serde_json::json!({
            "name": "cmd",
            "futureField": { "anything": true }
        });
        let decl = CommandDeclaration::from_value(&json).unwrap();
        assert_eq!(decl.name, "cmd");
        assert!(decl.description.is_none());
    }

    #[test]
    fn test_command_declaration_absent_fields_default_semantics() {
        let json = serde_json::json!({ "name": "cmd" });
        let decl = CommandDeclaration::from_value(&json).unwrap();
        // Absent dispatcher = bridge semantics (not tool-facing).
        assert!(!decl.is_tool_facing());
        assert!(decl.dispatcher.is_none());
        // Absent scopes = default conversation surface only.
        assert!(!decl.has_compact_scope());
        assert!(decl.extra_scopes.is_empty());
        assert!(decl.require_tool_call().is_none());
    }

    #[test]
    fn test_command_declaration_unknown_values_fall_back() {
        let json = serde_json::json!({
            "name": "cmd",
            "dispatcher": "turbo",
            "extraScopes": ["compact", "sidebar"]
        });
        let decl = CommandDeclaration::from_value(&json).unwrap();
        assert_eq!(
            decl.dispatcher,
            Some(Dispatcher::Unknown("turbo".into()))
        );
        // Unknown dispatcher is conservatively not tool-facing.
        assert!(!decl.is_tool_facing());
        assert_eq!(
            decl.extra_scopes,
            vec![
                ExtraScope::Compact,
                ExtraScope::Unknown("sidebar".into())
            ]
        );
        // Unknown values serialize back verbatim (no data loss).
        let back = decl.to_value().unwrap();
        assert_eq!(back["dispatcher"], "turbo");
        assert_eq!(back["extraScopes"], serde_json::json!(["compact", "sidebar"]));
    }

    #[test]
    fn test_command_declaration_helpers() {
        let json = serde_json::json!({
            "name": "cmd",
            "dispatcher": "tool",
            "evaluationPolicy": {
                "requireToolCall": { "require": "report_to_user", "onFailure": "bounce" }
            },
            "extraScopes": ["compact"]
        });
        let decl = CommandDeclaration::from_value(&json).unwrap();
        assert!(decl.is_tool_facing());
        assert!(decl.has_compact_scope());
        assert_eq!(decl.require_tool_call().unwrap().require, "report_to_user");
        assert_eq!(decl.require_tool_call().unwrap().on_failure, "bounce");
    }

    #[test]
    fn test_command_declaration_explicit_bridge_dispatcher() {
        let json = serde_json::json!({ "name": "cmd", "dispatcher": "bridge" });
        let decl = CommandDeclaration::from_value(&json).unwrap();
        // Explicit bridge is a known wire value, distinct from Unknown.
        assert_eq!(decl.dispatcher, Some(Dispatcher::Bridge));
        // Bridge is not the model tool surface.
        assert!(!decl.is_tool_facing());
        // Serializes back verbatim as the wire value.
        let back = decl.to_value().unwrap();
        assert_eq!(back, json);
    }
}
