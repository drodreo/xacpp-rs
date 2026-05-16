//! Event payload types.

use serde::{Deserialize, Serialize};

use super::content::ContentPart;

/// Content part event payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContentPartEvent {
    /// Unique identifier for one engine run.
    pub round: String,
    /// Unique identifier for one loop iteration.
    pub pair: String,
    /// Actual content part.
    pub payload: ContentPart,
}

/// Content delta event payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContentDeltaEvent {
    /// Unique identifier for one engine run.
    pub round: String,
    /// Unique identifier for one loop iteration.
    pub pair: String,
    /// Actual content delta.
    pub payload: ContentPart,
}

/// Traceable event payload (shared by Info/Warn/Error).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TraceableEvent {
    /// Main sentence for user/interface (required).
    pub title: String,
    /// Extended details (optional, defaults to empty string).
    #[serde(default)]
    pub content: String,
}

/// Authorization request Alert level.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AlertLevel {
    #[default]
    Info,
    Warn,
    Critical,
}

/// Tool call started event payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolUseEvent {
    /// ToolRequest.id.
    pub request_id: String,
    /// Tool name.
    pub tool_name: String,
    /// Tool call sequence number.
    pub index: u32,
    /// Human-readable summary of tool arguments (may be truncated, not guaranteed parseable).
    pub arguments: String,
}

/// Tool call finished event payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolResultEvent {
    /// ToolRequest.id.
    pub request_id: String,
    /// Tool name.
    pub tool_name: String,
    /// Tool call sequence number.
    pub index: u32,
    /// Tool execution result content.
    #[serde(default)]
    pub parts: Vec<ContentPart>,
}

/// Security alert event payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SecurityAlertEvent {
    /// Event unique identifier.
    pub event_id: String,
    /// Associated tool name.
    pub tool_name: String,
    /// Alert level.
    pub alert_level: AlertLevel,
    /// Threat description.
    pub description: String,
    /// Threat type.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub threat_type: Option<String>,
    /// Matched attack pattern.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matched_pattern: Option<String>,
    /// Context snippet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_snippet: Option<String>,
}

/// SubActivity task started event payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActivityStartEvent {
    /// Task goal summary.
    pub goal: String,
    /// SubActivity unique identifier.
    pub activity: String,
    /// Additional metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<std::collections::HashMap<String, String>>,
}

/// Activity metadata shared across commands, responses, and events.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActivityInfo {
    pub activity: String,
    pub agent: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}
