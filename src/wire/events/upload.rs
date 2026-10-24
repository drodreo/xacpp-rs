//! File upload and token usage.

use serde::{Deserialize, Serialize};

use super::content::FileRef;

/// File upload event.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "upload_event", rename_all = "camelCase")]
pub enum UploadEvent {
    /// Upload progress update.
    Progress {
        name: String,
        uploaded: u64,
        total: u64,
    },
    /// Single file upload completed.
    Completed { name: String, media_source: FileRef },
    /// Single file upload failed.
    Error { name: String, error: String },
}

/// Token usage information.
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TokenUsage {
    pub id: String,
    pub message: String,
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
    /// Cache-hit input tokens read from the provider-side prompt cache for
    /// this call.
    ///
    /// `None` = the provider did not report cache data; `Some(0)` = the
    /// provider explicitly reported zero cache hits.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_hit_tokens: Option<u32>,
}
