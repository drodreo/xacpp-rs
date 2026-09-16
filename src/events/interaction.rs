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
use crate::activity_ref::ActivityRef;
use crate::commands::XacppCommand;

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
    pub operation: SensitiveInfoOperation,
}

// ---- Convenience: build interaction commands ----

/// Builds an `action_request` command with the activity in the command envelope.
pub fn action_request_command(activity: &str, payload: &ActionRequestPayload) -> XacppCommand {
    XacppCommand::generic_with_activity(
        "action_request",
        serde_json::to_value(payload).unwrap_or(Value::Null),
        ActivityRef::new(activity),
    )
}

/// Builds a `question` command with the activity in the command envelope.
pub fn question_command(activity: &str, payload: &QuestionPayload) -> XacppCommand {
    XacppCommand::generic_with_activity(
        "question",
        serde_json::to_value(payload).unwrap_or(Value::Null),
        ActivityRef::new(activity),
    )
}

/// Builds a `sensitive_info_operation` command with the activity in the command envelope.
pub fn sensitive_info_command(
    activity: &str,
    payload: &SensitiveInfoOperationPayload,
) -> XacppCommand {
    XacppCommand::generic_with_activity(
        "sensitive_info_operation",
        serde_json::to_value(payload).unwrap_or(Value::Null),
        ActivityRef::new(activity),
    )
}
