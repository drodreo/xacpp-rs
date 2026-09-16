use serde::{Deserialize, Serialize};

use super::xacpp_event::XacppEvent;
use crate::activity_ref::ActivityRef;

/// Activity-scoped event envelope.
///
/// Wraps an `XacppEvent` with an `activity` reference so the consumer can
/// identify which activity within a session produced the event.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct XacppActivityEvent {
    pub activity: ActivityRef,
    pub event: XacppEvent,
}

impl XacppActivityEvent {
    /// Convenience constructor.
    pub fn new(activity: impl Into<String>, event: XacppEvent) -> Self {
        XacppActivityEvent {
            activity: ActivityRef::new(activity.into()),
            event,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_activity_event_serialization_contains_structured_activity() {
        let event = XacppActivityEvent::new(
            "act-1",
            XacppEvent::new("think", json!({ "content": "hi" })),
        );

        let s = serde_json::to_string(&event).unwrap();
        assert!(s.contains(r#""activity":{"id":"act-1"}"#), "json: {s}");
        assert!(s.contains(r#""name":"think""#), "json: {s}");
    }

    #[test]
    fn test_activity_event_roundtrip() {
        let event = XacppActivityEvent::new(
            "act-1",
            XacppEvent::new("think", json!({ "content": "hi" })),
        );

        let s = serde_json::to_string(&event).unwrap();
        let de: XacppActivityEvent = serde_json::from_str(&s).unwrap();
        assert_eq!(de.activity, ActivityRef::new("act-1"));
        assert_eq!(de.event.name, "think");
        assert_eq!(de.event.data["content"], "hi");
    }

    #[test]
    fn test_activity_event_rejects_legacy_string_activity() {
        // 0.8.0 breaking change: legacy string-form activity must fail deterministically.
        let result: Result<XacppActivityEvent, _> =
            serde_json::from_str(r#"{"activity":"act-1","event":{"name":"think","data":null}}"#);
        assert!(result.is_err());
    }
}
