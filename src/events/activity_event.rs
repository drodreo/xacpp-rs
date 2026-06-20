use serde::{Deserialize, Serialize};

use super::xacpp_event::XacppEvent;

/// Activity-scoped event envelope.
///
/// Wraps an `XacppEvent` with an `activity` field so the consumer can
/// identify which activity within a session produced the event.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct XacppActivityEvent {
    pub activity: String,
    pub event: XacppEvent,
}

impl XacppActivityEvent {
    /// Convenience constructor.
    pub fn new(activity: impl Into<String>, event: XacppEvent) -> Self {
        XacppActivityEvent {
            activity: activity.into(),
            event,
        }
    }
}
