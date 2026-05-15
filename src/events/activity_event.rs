use serde::{Deserialize, Serialize};
use super::xacpp_event::XacppEvent;

/// Activity-scoped event envelope.
///
/// Wraps an `XacppEvent` with an `activity` so the consumer can
/// identify which activity within a session produced the event.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct XacppActivityEvent {
    pub activity: String,
    pub event: XacppEvent,
}
