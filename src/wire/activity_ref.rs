//! Activity reference shared by command and event envelopes.

use serde::{Deserialize, Serialize};

/// Identifies the activity a command/event originates from.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ActivityRef {
    pub id: String,
}

impl ActivityRef {
    /// Convenience constructor.
    pub fn new(id: impl Into<String>) -> Self {
        ActivityRef { id: id.into() }
    }
}
