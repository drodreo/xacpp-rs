//! XACPP Protocol Event Types.
//!
//! `XacppEvent` fully maps to x-agent's `AgentEvent`,
//! serving as the standardized event stream between xagent and peers (e.g., xabot).

pub mod content;
pub mod interaction;
pub mod payload;
pub mod upload;
pub mod xacpp_event;

pub use content::{AudioPart, ContentPart, FileRef, ImagePart, TextPart, VideoPart};
pub use interaction::{
    ActionRequestEvent, ActionResponse, NotifyEvent, QuestionEvent, QuestionResponse,
    SensitiveInfoItem, SensitiveInfoOperation, SensitiveInfoOperationEvent,
    SensitiveInfoOperationResponse, SensitiveInfoResult, SensitiveInfoType,
};
pub use payload::{
    ActivityStartEvent, AlertLevel, ContentDeltaEvent, ContentPartEvent, SecurityAlertEvent,
    ToolResultEvent, ToolUseEvent, TraceableEvent,
};
pub use upload::{TokenUsage, UploadEvent};
pub use xacpp_event::XacppEvent;
