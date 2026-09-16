//! Lifecycle command and response payload types (protocol body).
//!
//! Wire types for the seven lifecycle commands:
//!
//! | Command          | req arguments                       | resp                                                     |
//! |------------------|-------------------------------------|----------------------------------------------------------|
//! | new_activity     | `{title?}`                          | `activity_ready(ActivityInfo)`                            |
//! | last_activity    | `{}`                                | `activity_ready(ActivityInfo)` or `activity_not_found`    |
//! | switch_activity  | `{activity}`                        | `activity_ready(ActivityInfo)`                            |
//! | list_activity    | `{query?, pageNum?=1, pageSize?=20}`| `available_activities`                                    |
//! | invoke_activity  | `{activity, messages}`              | `acknowledge`                                             |
//! | cancel_activity  | `{activity, reason?}`               | `acknowledge`                                             |
//! | compact_activity | `{activity}`                        | `acknowledge`                                             |
//!
//! `activity` in request payloads is the operation target (a command business
//! argument), not the envelope source; envelope source semantics are carried by
//! `XacppCommand::Generic.activity`.
//!
//! Responses: `activity_ready` carries `ActivityInfo` as `data`; `acknowledge`
//! keeps the `XacppResponse::acknowledge()` constructor form; `activity_not_found`
//! keeps the `XacppResponse::activity_not_found()` constructor form.
//! `activity_ready` exists only as a response shape, never as an event.

use serde::{Deserialize, Serialize};

use crate::events::content::ContentPart;
use crate::events::payload::ActivityInfo;

// ---- Request payloads (command arguments) ----

/// `new_activity` request payload.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewActivityPayload {
    /// Title for the new activity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

/// `last_activity` request payload (empty arguments).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LastActivityPayload {}

/// `switch_activity` request payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SwitchActivityPayload {
    /// Operation target activity.
    pub activity: String,
}

/// `list_activity` request payload.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListActivityPayload {
    /// Filter query.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    /// Page number (defaults to 1 when omitted).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page_num: Option<u32>,
    /// Page size (defaults to 20 when omitted).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page_size: Option<u32>,
}

/// `invoke_activity` request payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InvokeActivityPayload {
    /// Operation target activity.
    pub activity: String,
    /// Input messages.
    pub messages: Vec<ContentPart>,
}

/// `cancel_activity` request payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CancelActivityPayload {
    /// Operation target activity.
    pub activity: String,
    /// Cancellation reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// `compact_activity` request payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactActivityPayload {
    /// Operation target activity.
    pub activity: String,
}

// ---- Response payloads ----

/// `available_activities` response payload (`list_activity`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AvailableActivities {
    /// Total number of activities matching the query.
    pub total: u64,
    /// One page of activities.
    pub activities: Vec<ActivityInfo>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::XacppCommand;
    use crate::events::content::TextPart;

    #[test]
    fn test_new_activity_payload_serialization() {
        let payload = NewActivityPayload {
            title: Some("research".into()),
        };
        let s = serde_json::to_string(&payload).unwrap();
        assert_eq!(s, r#"{"title":"research"}"#);

        let empty = NewActivityPayload::default();
        let s = serde_json::to_string(&empty).unwrap();
        assert_eq!(s, r#"{}"#);
    }

    #[test]
    fn test_last_activity_payload_serialization() {
        let payload = LastActivityPayload {};
        let s = serde_json::to_string(&payload).unwrap();
        assert_eq!(s, r#"{}"#);
    }

    #[test]
    fn test_switch_activity_payload_serialization() {
        let payload = SwitchActivityPayload {
            activity: "act-1".into(),
        };
        let s = serde_json::to_string(&payload).unwrap();
        assert_eq!(s, r#"{"activity":"act-1"}"#);
    }

    #[test]
    fn test_list_activity_payload_serialization() {
        let payload = ListActivityPayload {
            query: Some("agent".into()),
            page_num: Some(2),
            page_size: Some(10),
        };
        let s = serde_json::to_string(&payload).unwrap();
        assert_eq!(s, r#"{"query":"agent","pageNum":2,"pageSize":10}"#);

        let empty = ListActivityPayload::default();
        let s = serde_json::to_string(&empty).unwrap();
        assert_eq!(s, r#"{}"#);

        let de: ListActivityPayload = serde_json::from_str(r#"{}"#).unwrap();
        assert_eq!(de.query, None);
        assert_eq!(de.page_num, None);
        assert_eq!(de.page_size, None);
    }

    #[test]
    fn test_invoke_activity_payload_serialization() {
        let payload = InvokeActivityPayload {
            activity: "act-1".into(),
            messages: vec![ContentPart::Text(TextPart {
                text: "hi".into(),
                part_id: None,
            })],
        };
        let s = serde_json::to_string(&payload).unwrap();
        assert_eq!(s, r#"{"activity":"act-1","messages":[{"type":"text","text":"hi"}]}"#);
    }

    #[test]
    fn test_cancel_activity_payload_serialization() {
        let payload = CancelActivityPayload {
            activity: "act-1".into(),
            reason: Some("user abort".into()),
        };
        let s = serde_json::to_string(&payload).unwrap();
        assert_eq!(s, r#"{"activity":"act-1","reason":"user abort"}"#);

        let no_reason = CancelActivityPayload {
            activity: "act-1".into(),
            reason: None,
        };
        let s = serde_json::to_string(&no_reason).unwrap();
        assert_eq!(s, r#"{"activity":"act-1"}"#);
    }

    #[test]
    fn test_compact_activity_payload_serialization() {
        let payload = CompactActivityPayload {
            activity: "act-1".into(),
        };
        let s = serde_json::to_string(&payload).unwrap();
        assert_eq!(s, r#"{"activity":"act-1"}"#);
    }

    #[test]
    fn test_available_activities_serialization() {
        let payload = AvailableActivities {
            total: 1,
            activities: vec![ActivityInfo {
                activity: "act-1".into(),
                agent: "main".into(),
                title: Some("research".into()),
            }],
        };
        let s = serde_json::to_string(&payload).unwrap();
        assert_eq!(
            s,
            r#"{"total":1,"activities":[{"activity":"act-1","agent":"main","title":"research"}]}"#
        );
    }

    #[test]
    fn test_lifecycle_command_wire_anchor() {
        let cmd = XacppCommand::generic(
            "new_activity",
            serde_json::to_value(NewActivityPayload {
                title: Some("research".into()),
            })
            .unwrap(),
        );
        let s = serde_json::to_string(&cmd).unwrap();
        assert_eq!(
            s,
            r#"{"generic":{"name":"new_activity","arguments":{"title":"research"}}}"#
        );
    }

    #[test]
    fn test_activity_not_found_response_anchor() {
        let resp = crate::message::XacppResponse::activity_not_found();
        let s = serde_json::to_string(&resp).unwrap();
        assert_eq!(s, r#"{"kind":"generic","name":"activity_not_found","data":null}"#);
    }

    #[test]
    fn test_activity_ready_response_roundtrip() {
        let wire = r#"{"kind":"generic","name":"activity_ready","data":{"activity":"act-1","agent":"main","title":"research"}}"#;
        let resp: crate::message::XacppResponse = serde_json::from_str(wire).unwrap();
        match resp {
            crate::message::XacppResponse::Generic { name, data } => {
                assert_eq!(name, "activity_ready");
                let info: ActivityInfo = serde_json::from_value(data).unwrap();
                assert_eq!(info.activity, "act-1");
                assert_eq!(info.agent, "main");
                assert_eq!(info.title.as_deref(), Some("research"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }
}
