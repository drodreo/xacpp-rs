//! Interaction event payloads (request-response pattern).
//!
//! Events carry `responder` (oneshot Sender), consistent with x-agent's AgentEvent structure.
//! During cross-process transmission, responder is `#[serde(skip)]` skipped; peer returns `XacppResponse`
//! via Transport's on_event handler, and Transport auto-packs envelope to send back.

use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;

use super::payload::AlertLevel;
use crate::message::XacppResponse;

// ---- Tool Call Authorization ----

/// Tool call authorization response.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", rename_all_fields = "camelCase")]
pub enum ActionResponse {
    /// Approve execution.
    Approve,
    /// Approve execution and always allow same request.
    ApproveAlways,
    /// Reject execution.
    Reject { reason: String },
}

/// Tool call authorization request event payload.
///
/// For in-process usage, consumer replies `XacppResponse` via `responder`.
/// During cross-process transmission, `responder` is None (auto-filled on deserialization), peer responds via transport.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActionRequestEvent {
    pub request_id: String,
    pub tool_name: String,
    pub arguments: String,
    pub action_id: String,
    pub description: String,
    pub alert: AlertLevel,
    /// Callback channel. Consumer sends authorization decision via this channel.
    #[serde(skip)]
    pub responder: Option<oneshot::Sender<XacppResponse>>,
}

// ---- Notification ----

/// User notification event payload (one-way push, non-blocking wait for reply).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NotifyEvent {
    pub request_id: String,
    pub message: String,
}

// ---- Question ----

/// User question response.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", rename_all_fields = "camelCase")]
pub enum QuestionResponse {
    /// User selected an option.
    Answer { content: String },
    /// User declined to answer / skipped.
    Skip { reason: Option<String> },
}

/// User question event payload.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuestionEvent {
    pub request_id: String,
    pub question: String,
    pub options: Vec<String>,
    /// Callback channel. Consumer replies with answer via this channel.
    #[serde(skip)]
    pub responder: Option<oneshot::Sender<XacppResponse>>,
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
#[serde(tag = "type", rename_all = "snake_case", rename_all_fields = "camelCase")]
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

/// Sensitive info operation request event payload.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SensitiveInfoOperationEvent {
    pub request_id: String,
    pub operation: SensitiveInfoOperation,
    /// Callback channel. Consumer sends operation result via this channel.
    #[serde(skip)]
    pub responder: Option<oneshot::Sender<XacppResponse>>,
}
