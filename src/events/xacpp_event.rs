//! XACPP Protocol Event Enum.
//!
//! Fully maps to x-agent's `AgentEvent`, serving as the standardized event stream between xagent and peers.
//! The only termination signal is `Complete`; other events do not imply termination semantics.

use serde::{Deserialize, Serialize};

use super::content::ContentPart;
use super::interaction::{
    ActionRequestEvent, NotifyEvent, QuestionEvent, SensitiveInfoOperationEvent,
};
use super::payload::{
    ActivityStartEvent, ContentDeltaEvent, ContentPartEvent, SecurityAlertEvent, ToolResultEvent,
    ToolUseEvent, TraceableEvent,
};
use super::upload::{TokenUsage, UploadEvent};

/// XACPP Protocol Event.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", rename_all_fields = "camelCase")]
pub enum XacppEvent {
    // ---- Content Output ----

    /// Unified multimodal content delta event (with runtime context).
    ContentDelta(ContentDeltaEvent),
    /// Unified multimodal content part event (with runtime context).
    ContentPart(ContentPartEvent),
    /// Thinking text output (delta).
    Think { content: String },

    // ---- System Signals ----

    /// System info output (single event).
    Info(TraceableEvent),
    /// System warning output (single event).
    Warn(TraceableEvent),
    /// Structured error event (non-terminating).
    Error(TraceableEvent),

    // ---- Interaction (Request-Response Pattern) ----

    /// Tool call authorization request. Peer responds via transport with ActionResponse.
    ActionRequest(ActionRequestEvent),
    /// Notification to user (one-way push, non-blocking).
    Notify(NotifyEvent),
    /// Question to user. Peer responds via transport with QuestionResponse.
    Question(QuestionEvent),
    /// Sensitive info operation request. Peer responds via transport with SensitiveInfoOperationResponse.
    SensitiveInfoOperation(SensitiveInfoOperationEvent),

    // ---- Activity Lifecycle ----

    /// Signal that SubActivity has entered resumable waiting state.
    WaitingCommand,
    /// SubActivity task started execution.
    ActivityStart(ActivityStartEvent),
    /// SubActivity task completed.
    ActivityDone { activity_id: String },
    /// SubActivity aborted by user.
    ActivityAborted { activity_id: String, reason: String },

    // ---- Tool Calls ----

    /// Tool call started execution.
    ToolUse(ToolUseEvent),
    /// Tool call finished execution.
    ToolResult(ToolResultEvent),

    // ---- Security ----

    /// Security alert event (non-blocking to main flow).
    SecurityAlert(SecurityAlertEvent),

    // ---- Upload ----

    /// File upload event (progress/complete/fail).
    Upload(UploadEvent),

    // ---- Engine Signals ----

    /// ReAct Loop single round iteration complete signal.
    PairComplete {
        context_window: u32,
        token_usage: TokenUsage,
    },
    /// The only termination signal for invoke. On normal completion, passes the last assistant message; on exception/cancellation, passes empty array.
    Complete { assistant_reply: Vec<ContentPart> },
}
