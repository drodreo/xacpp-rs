//! XACPP Protocol Event Types.
//!
//! `XacppEvent` is a generic `{ name, data }` structure.
//! Type definitions for common event payloads are kept in submodules
//! and serve as serialization targets for the `data` field.

pub mod activity_event;
pub mod content;
pub mod interaction;
pub mod payload;
pub mod upload;
pub mod xacpp_event;

pub use content::{AudioPart, ContentPart, FilePart, FileRef, ImagePart, TextPart, VideoPart};
pub use interaction::{
    ActionRequestPayload, ActionResponse, NotifyPayload, QuestionPayload, QuestionResponse,
    SensitiveInfoItem, SensitiveInfoOperation, SensitiveInfoOperationPayload,
    SensitiveInfoOperationResponse, SensitiveInfoResult, SensitiveInfoType,
};
pub use payload::{
    ActivityInfo, ActivityStartEvent, AlertLevel, ContentDeltaEvent, ContentPartEvent,
    SecurityAlertEvent, ToolResultEvent, ToolUseEvent, TraceableEvent,
};
pub use upload::{TokenUsage, UploadEvent};
pub use xacpp_event::XacppEvent;
pub use activity_event::XacppActivityEvent;
