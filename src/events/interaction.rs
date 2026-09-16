//! Interaction command/response payloads.
//!
//! These types serve as serialization targets for:
//! - Command `arguments` when `name` is "action_request", "question", or "sensitive_info_operation".
//! - Response `data` when `name` is "action", "question", or "sensitive_info_operation".
//!
//! The `responder` channel that existed in previous versions has been removed.
//! Interaction requests are now Commands: the transport layer's request-response
//! correlation handles matching responses back to the original sender.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::payload::AlertLevel;

// ---- Tool Call Authorization ----

/// Tool call authorization response.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum ActionResponse {
    /// Approve execution.
    Approve,
    /// Approve execution and always allow same request.
    ApproveAlways,
    /// Reject execution.
    Reject { reason: String },
}

/// Tool call authorization request payload (command arguments).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActionRequestPayload {
    pub request_id: String,
    pub tool_name: String,
    pub arguments: String,
    pub action_id: String,
    pub description: String,
    pub alert: AlertLevel,
    pub intent: String,
}

// ---- Notification ----

/// User notification event payload (one-way push via Event).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NotifyPayload {
    pub request_id: String,
    pub message: String,
}

// ---- Question ----

/// User question response.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum QuestionResponse {
    /// User selected an option.
    Answer { content: String },
    /// User declined to answer / skipped.
    Skip { reason: Option<String> },
}

/// User question request payload (command arguments).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuestionPayload {
    pub request_id: String,
    pub question: String,
    pub options: Vec<String>,
}

// ---- Sensitive Info ----

/// Sensitive info type.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum SensitiveInfoType {
    Secret,
    EnvVar,
}

/// Sensitive info item (masked display).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SensitiveInfoItem {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub key: String,
    pub display_text: String,
    pub hint: String,
    pub si_type: SensitiveInfoType,
}

/// Sensitive info operation type.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SensitiveInfoOperation {
    /// Collect: request user to fill in sensitive info.
    Collect { items: Vec<SensitiveInfoItem> },
    /// Delete: request user to confirm deletion.
    Delete { items: Vec<SensitiveInfoItem> },
}

/// Operation result for a single piece of sensitive info.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum SensitiveInfoResult {
    Provided { key: String, value: String },
    CollectSkipped { key: String, reason: Option<String> },
    Deleted { id: String },
    DeleteRejected { id: String, reason: Option<String> },
}

/// Sensitive info operation response.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SensitiveInfoOperationResponse {
    pub results: Vec<SensitiveInfoResult>,
}

/// Sensitive info operation request payload (command arguments).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SensitiveInfoOperationPayload {
    pub request_id: String,
    pub operation: SensitiveInfoOperation,
}

// ---- Convenience: build interaction commands ----

/// Builds an `action_request` command from a payload.
pub fn action_request_command(activity: &str, payload: &ActionRequestPayload) -> Value {
    serde_json::to_value(payload)
        .map(|p| {
            let mut map = if let Value::Object(m) = p {
                m
            } else {
                serde_json::Map::new()
            };
            map.insert("activity".to_string(), Value::String(activity.to_string()));
            Value::Object(map)
        })
        .unwrap_or(Value::Null)
}

/// Builds a `question` command from a payload.
pub fn question_command(activity: &str, payload: &QuestionPayload) -> Value {
    serde_json::to_value(payload)
        .map(|p| {
            let mut map = if let Value::Object(m) = p {
                m
            } else {
                serde_json::Map::new()
            };
            map.insert("activity".to_string(), Value::String(activity.to_string()));
            Value::Object(map)
        })
        .unwrap_or(Value::Null)
}

/// Builds a `sensitive_info_operation` command from a payload.
pub fn sensitive_info_command(activity: &str, payload: &SensitiveInfoOperationPayload) -> Value {
    serde_json::to_value(payload)
        .map(|p| {
            let mut map = if let Value::Object(m) = p {
                m
            } else {
                serde_json::Map::new()
            };
            map.insert("activity".to_string(), Value::String(activity.to_string()));
            Value::Object(map)
        })
        .unwrap_or(Value::Null)
}
