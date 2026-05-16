//! Serialization / deserialization correctness tests.
//!
//! Verifies round-trip of envelope layer + payload layer, including type tag routing and payload nested structures.

use xacpp::commands::XacppCommand;
use xacpp::events::{XacppActivityEvent, XacppEvent};
use xacpp::events::interaction::{
    ActionRequestEvent, ActionResponse, QuestionEvent,
    SensitiveInfoOperationEvent, SensitiveInfoOperation, SensitiveInfoType,
    SensitiveInfoItem,
};
use xacpp::events::payload::{AlertLevel, TraceableEvent};
use xacpp::events::ActivityInfo;
use xacpp::message::{XacppEnvelope, XacppRequest, XacppResponse};

// ---- XacppEvent self round-trip ----

#[test]
fn test_event_action_request_roundtrip() {
    let event = XacppEvent::ActionRequest(ActionRequestEvent {
        request_id: "req-1".into(),
        tool_name: "bash".into(),
        arguments: r#"{"command":"ls"}"#.into(),
        action_id: "act-1".into(),
        description: "list files".into(),
        alert: AlertLevel::Warn,
        responder: None,
    });

    let json = serde_json::to_string(&event).unwrap();
    assert!(json.contains(r#""type":"action_request""#), "json: {json}");

    let de: XacppEvent = serde_json::from_str(&json).unwrap();
    assert!(matches!(de, XacppEvent::ActionRequest(_)));
}

#[test]
fn test_event_question_roundtrip() {
    let event = XacppEvent::Question(QuestionEvent {
        request_id: "req-2".into(),
        question: "continue?".into(),
        options: vec!["yes".into(), "no".into()],
        responder: None,
    });

    let json = serde_json::to_string(&event).unwrap();
    assert!(json.contains(r#""type":"question""#), "json: {json}");

    let de: XacppEvent = serde_json::from_str(&json).unwrap();
    assert!(matches!(de, XacppEvent::Question(_)));
}

#[test]
fn test_event_sensitive_info_roundtrip() {
    let event = XacppEvent::SensitiveInfoOperation(SensitiveInfoOperationEvent {
        request_id: "req-3".into(),
        operation: SensitiveInfoOperation::Collect {
            items: vec![SensitiveInfoItem {
                id: None,
                key: "API_KEY".into(),
                display_text: "API Key".into(),
                hint: "enter your key".into(),
                si_type: SensitiveInfoType::Secret,
            }],
        },
        responder: None,
    });

    let json = serde_json::to_string(&event).unwrap();
    assert!(json.contains(r#""type":"sensitive_info_operation""#), "json: {json}");

    let de: XacppEvent = serde_json::from_str(&json).unwrap();
    assert!(matches!(de, XacppEvent::SensitiveInfoOperation(_)));
}

#[test]
fn test_event_think_roundtrip() {
    let event = XacppEvent::Think { content: "thinking...".into() };
    let json = serde_json::to_string(&event).unwrap();
    assert!(json.contains(r#""type":"think""#), "json: {json}");

    let de: XacppEvent = serde_json::from_str(&json).unwrap();
    assert!(matches!(de, XacppEvent::Think { .. }));
}

#[test]
fn test_event_info_roundtrip() {
    let event = XacppEvent::Info(TraceableEvent {
        title: "started".into(),
        content: "".into(),
    });
    let json = serde_json::to_string(&event).unwrap();
    assert!(json.contains(r#""type":"info""#), "json: {json}");

    let de: XacppEvent = serde_json::from_str(&json).unwrap();
    assert!(matches!(de, XacppEvent::Info(_)));
}

// ---- XacppEnvelope round-trip ----

#[test]
fn test_wire_request_command_roundtrip() {
    let wire = XacppEnvelope::Request {
        id: "r1".into(),
        session_id: None,
        payload: XacppRequest::Command(XacppCommand::Establish { credentials: None }),
    };
    let json = serde_json::to_string(&wire).unwrap();
    assert!(json.contains(r#""type":"request""#), "json: {json}");
    assert!(json.contains(r#""id":"r1""#), "json: {json}");
    assert!(json.contains(r#""kind":"command""#), "json: {json}");

    let de: XacppEnvelope = serde_json::from_str(&json).unwrap();
    match de {
        XacppEnvelope::Request { id, session_id: _, payload } => {
            assert_eq!(id, "r1");
            assert!(matches!(payload, XacppRequest::Command(XacppCommand::Establish { credentials: None })));
        }
        XacppEnvelope::Response { .. } => panic!("expected Request"),
    }
}

#[test]
fn test_wire_request_event_roundtrip() {
    let inner_event = XacppEvent::ActionRequest(ActionRequestEvent {
        request_id: "req-1".into(),
        tool_name: "bash".into(),
        arguments: "{}".into(),
        action_id: "act-1".into(),
        description: "test".into(),
        alert: AlertLevel::Info,
        responder: None,
    });
    let wire = XacppEnvelope::Request {
        id: "r2".into(),
        session_id: None,
        payload: XacppRequest::Event(XacppActivityEvent {
            activity: "test-act".into(),
            event: inner_event,
        }),
    };
    let json = serde_json::to_string(&wire).unwrap();
    assert!(json.contains(r#""type":"request""#), "json: {json}");
    assert!(json.contains(r#""id":"r2""#), "json: {json}");
    assert!(json.contains(r#""kind":"event""#), "json: {json}");
    assert!(json.contains(r#""type":"action_request""#), "json: {json}");

    let de: XacppEnvelope = serde_json::from_str(&json).unwrap();
    match de {
        XacppEnvelope::Request { id, session_id: _, payload } => {
            assert_eq!(id, "r2");
            match payload {
                XacppRequest::Event(XacppActivityEvent { event: XacppEvent::ActionRequest(e), .. }) => {
                    assert_eq!(e.request_id, "req-1");
                }
                other => panic!("unexpected: {other:?}"),
            }
        }
        XacppEnvelope::Response { .. } => panic!("expected Request"),
    }
}

#[test]
fn test_wire_response_established_roundtrip() {
    let wire = XacppEnvelope::Response {
        id: "r1".into(),
        session_id: None,
        payload: XacppResponse::Established { session_id: "123456".into(), credentials: "test-creds".into() },
    };
    let json = serde_json::to_string(&wire).unwrap();
    assert!(json.contains(r#""type":"response""#), "json: {json}");
    assert!(json.contains(r#""id":"r1""#), "json: {json}");
    assert!(json.contains(r#""kind":"established""#), "json: {json}");
    assert!(json.contains(r#""sessionId":"123456""#), "json: {json}");

    let de: XacppEnvelope = serde_json::from_str(&json).unwrap();
    match de {
        XacppEnvelope::Response { id, session_id: _, payload } => {
            assert_eq!(id, "r1");
            assert!(matches!(payload, XacppResponse::Established { .. }));
        }
        XacppEnvelope::Request { .. } => panic!("expected Response"),
    }
}

#[test]
fn test_wire_response_action_roundtrip() {
    let wire = XacppEnvelope::Response {
        id: "r2".into(),
        session_id: None,
        payload: XacppResponse::Action {
            request_id: "req-1".into(),
            response: ActionResponse::Approve,
        },
    };
    let json = serde_json::to_string(&wire).unwrap();
    assert!(json.contains(r#""type":"response""#), "json: {json}");
    assert!(json.contains(r#""kind":"action""#), "json: {json}");
    assert!(json.contains(r#""requestId":"req-1""#), "json: {json}");

    let de: XacppEnvelope = serde_json::from_str(&json).unwrap();
    match de {
        XacppEnvelope::Response { session_id: _, payload, .. } => {
            match payload {
                XacppResponse::Action { request_id, response } => {
                    assert_eq!(request_id, "req-1");
                    assert!(matches!(response, ActionResponse::Approve));
                }
                other => panic!("unexpected: {other:?}"),
            }
        }
        XacppEnvelope::Request { .. } => panic!("expected Response"),
    }
}

#[test]
fn test_wire_response_sensitive_info_operation_roundtrip() {
    use xacpp::events::interaction::{SensitiveInfoOperationResponse, SensitiveInfoResult};
    let wire = XacppEnvelope::Response {
        id: "r5".into(),
        session_id: None,
        payload: XacppResponse::SensitiveInfoOperation {
            request_id: "req-1".into(),
            response: SensitiveInfoOperationResponse {
                results: vec![
                    SensitiveInfoResult::Provided {
                        key: "API_KEY".into(),
                        value: "secret".into(),
                    },
                ],
            },
        },
    };
    let json = serde_json::to_string(&wire).unwrap();
    assert!(json.contains(r#""type":"response""#), "json: {json}");
    assert!(json.contains(r#""kind":"sensitive_info_operation""#), "json: {json}");
    assert!(json.contains(r#""requestId":"req-1""#), "json: {json}");
    // After flatten, results appear directly at response level
    assert!(json.contains(r#""results""#), "json: {json}");

    let de: XacppEnvelope = serde_json::from_str(&json).unwrap();
    match de {
        XacppEnvelope::Response { id, session_id: _, payload } => {
            assert_eq!(id, "r5");
            match payload {
                XacppResponse::SensitiveInfoOperation { request_id, response } => {
                    assert_eq!(request_id, "req-1");
                    assert_eq!(response.results.len(), 1);
                }
                other => panic!("unexpected: {other:?}"),
            }
        }
        XacppEnvelope::Request { .. } => panic!("expected Response"),
    }
}

#[test]
fn test_wire_response_acknowledge_roundtrip() {
    let wire = XacppEnvelope::Response {
        id: "r3".into(),
        session_id: None,
        payload: XacppResponse::Acknowledge,
    };
    let json = serde_json::to_string(&wire).unwrap();
    assert!(json.contains(r#""type":"response""#), "json: {json}");
    assert!(json.contains(r#""id":"r3""#), "json: {json}");
    assert!(json.contains(r#""kind":"acknowledge""#), "json: {json}");

    let de: XacppEnvelope = serde_json::from_str(&json).unwrap();
    match de {
        XacppEnvelope::Response { id, session_id: _, payload } => {
            assert_eq!(id, "r3");
            assert!(matches!(payload, XacppResponse::Acknowledge));
        }
        XacppEnvelope::Request { .. } => panic!("expected Response"),
    }
}

#[test]
fn test_wire_response_error_roundtrip() {
    let wire = XacppEnvelope::Response {
        id: "r4".into(),
        session_id: None,
        payload: XacppResponse::Error {
            code: "internal_error".into(),
            message: "something went wrong".into(),
        },
    };
    let json = serde_json::to_string(&wire).unwrap();
    assert!(json.contains(r#""type":"response""#), "json: {json}");
    assert!(json.contains(r#""id":"r4""#), "json: {json}");
    assert!(json.contains(r#""kind":"error""#), "json: {json}");
    assert!(json.contains(r#""code":"internal_error""#), "json: {json}");
    assert!(json.contains(r#""message":"something went wrong""#), "json: {json}");

    let de: XacppEnvelope = serde_json::from_str(&json).unwrap();
    match de {
        XacppEnvelope::Response { id, session_id: _, payload } => {
            assert_eq!(id, "r4");
            match payload {
                XacppResponse::Error { code, message } => {
                    assert_eq!(code, "internal_error");
                    assert_eq!(message, "something went wrong");
                }
                other => panic!("unexpected: {other:?}"),
            }
        }
        XacppEnvelope::Request { .. } => panic!("expected Response"),
    }
}

// ---- Deserialize from handwritten JSON ----

#[test]
fn test_deserialize_request_from_json() {
    let json = br#"{"type":"request","id":"r1","payload":{"kind":"command","payload":{"establish":{"credentials":null}}}}"#;
    let de: XacppEnvelope = serde_json::from_slice(json).unwrap();
    match de {
        XacppEnvelope::Request { id, session_id: _, .. } => assert_eq!(id, "r1"),
        XacppEnvelope::Response { .. } => panic!("expected Request"),
    }
}

#[test]
fn test_deserialize_response_from_json() {
    let json = br#"{"type":"response","id":"r1","payload":{"kind":"established","sessionId":"s1","credentials":"test-creds"}}"#;
    let de: XacppEnvelope = serde_json::from_slice(json).unwrap();
    match de {
        XacppEnvelope::Response { id, session_id: _, .. } => assert_eq!(id, "r1"),
        XacppEnvelope::Request { .. } => panic!("expected Response"),
    }
}

// ---- Interaction event responder skip verification ----

#[test]
fn test_action_request_with_responder_serializes_without_it() {
    let (tx, _rx) = tokio::sync::oneshot::channel::<XacppResponse>();
    let event = XacppEvent::ActionRequest(ActionRequestEvent {
        request_id: "req-r".into(),
        tool_name: "bash".into(),
        arguments: "{}".into(),
        action_id: "act-r".into(),
        description: "test".into(),
        alert: AlertLevel::Info,
        responder: Some(tx),
    });

    let json = serde_json::to_string(&event).unwrap();
    assert!(!json.contains("responder"), "json: {json}");
    assert!(json.contains("req-r"), "json: {json}");

    let de: XacppEvent = serde_json::from_str(&json).unwrap();
    match de {
        XacppEvent::ActionRequest(e) => assert!(e.responder.is_none()),
        other => panic!("unexpected: {other:?}"),
    }
}

// ---- New Command / Response / Event round-trip tests ----

#[test]
fn test_command_last_activity_roundtrip() {
    let cmd = XacppCommand::LastActivity;
    let json = serde_json::to_string(&cmd).unwrap();
    assert_eq!(json, "\"last_activity\"");

    let de: XacppCommand = serde_json::from_str(&json).unwrap();
    assert!(matches!(de, XacppCommand::LastActivity));
}

#[test]
fn test_command_list_activity_with_query_roundtrip() {
    let cmd = XacppCommand::ListActivity {
        query: Some("test".into()),
        page_num: 1,
        page_size: 10,
    };
    let json = serde_json::to_string(&cmd).unwrap();
    assert!(json.contains("\"list_activity\""), "json: {json}");
    assert!(json.contains("\"query\":\"test\""), "json: {json}");
    assert!(json.contains("\"pageNum\":1"), "json: {json}");
    assert!(json.contains("\"pageSize\":10"), "json: {json}");

    let de: XacppCommand = serde_json::from_str(&json).unwrap();
    match de {
        XacppCommand::ListActivity { query, page_num, page_size } => {
            assert_eq!(query.as_deref(), Some("test"));
            assert_eq!(page_num, 1);
            assert_eq!(page_size, 10);
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn test_command_list_activity_without_query_roundtrip() {
    let cmd = XacppCommand::ListActivity {
        query: None,
        page_num: 1,
        page_size: 10,
    };
    let json = serde_json::to_string(&cmd).unwrap();
    assert!(json.contains("\"list_activity\""), "json: {json}");
    assert!(!json.contains("query"), "json: {json}");
    assert!(json.contains("\"pageNum\":1"), "json: {json}");
    assert!(json.contains("\"pageSize\":10"), "json: {json}");

    let de: XacppCommand = serde_json::from_str(&json).unwrap();
    match de {
        XacppCommand::ListActivity { query, page_num, page_size } => {
            assert!(query.is_none());
            assert_eq!(page_num, 1);
            assert_eq!(page_size, 10);
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn test_command_switch_activity_roundtrip() {
    let cmd = XacppCommand::SwitchActivity {
        activity: "act-1".into(),
    };
    let json = serde_json::to_string(&cmd).unwrap();
    assert!(json.contains("\"switch_activity\""), "json: {json}");
    assert!(json.contains("\"activity\":\"act-1\""), "json: {json}");

    let de: XacppCommand = serde_json::from_str(&json).unwrap();
    match de {
        XacppCommand::SwitchActivity { activity } => {
            assert_eq!(activity, "act-1");
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn test_response_activity_ready_roundtrip() {
    let wire = XacppEnvelope::Response {
        id: "r1".into(),
        session_id: None,
        payload: XacppResponse::ActivityReady {
            info: ActivityInfo {
                activity: "act-1".into(),
                agent: "x-agent".into(),
                title: Some("test title".into()),
            },
        },
    };
    let json = serde_json::to_string(&wire).unwrap();
    assert!(json.contains("\"kind\":\"activity_ready\""), "json: {json}");
    assert!(json.contains("\"activity\":\"act-1\""), "json: {json}");
    assert!(json.contains("\"agent\":\"x-agent\""), "json: {json}");
    assert!(json.contains("\"title\":\"test title\""), "json: {json}");

    let de: XacppEnvelope = serde_json::from_str(&json).unwrap();
    match de {
        XacppEnvelope::Response { payload: XacppResponse::ActivityReady { info }, .. } => {
            assert_eq!(info.activity, "act-1");
            assert_eq!(info.agent, "x-agent");
            assert_eq!(info.title.as_deref(), Some("test title"));
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn test_response_activity_not_found_roundtrip() {
    let wire = XacppEnvelope::Response {
        id: "r1".into(),
        session_id: None,
        payload: XacppResponse::ActivityNotFound,
    };
    let json = serde_json::to_string(&wire).unwrap();
    assert!(json.contains("\"kind\":\"activity_not_found\""), "json: {json}");

    let de: XacppEnvelope = serde_json::from_str(&json).unwrap();
    assert!(matches!(de, XacppEnvelope::Response { payload: XacppResponse::ActivityNotFound, .. }));
}

#[test]
fn test_response_available_activities_roundtrip() {
    let wire = XacppEnvelope::Response {
        id: "r1".into(),
        session_id: None,
        payload: XacppResponse::AvailableActivities {
            total: 2,
            activities: vec![
                ActivityInfo {
                    activity: "act-1".into(),
                    agent: "x-agent".into(),
                    title: Some("title 1".into()),
                },
                ActivityInfo {
                    activity: "act-2".into(),
                    agent: "x-agent".into(),
                    title: None,
                },
            ],
        },
    };
    let json = serde_json::to_string(&wire).unwrap();
    assert!(json.contains("\"kind\":\"available_activities\""), "json: {json}");
    assert!(json.contains("\"total\":2"), "json: {json}");
    assert!(json.contains("\"activities\""), "json: {json}");

    let de: XacppEnvelope = serde_json::from_str(&json).unwrap();
    match de {
        XacppEnvelope::Response { payload: XacppResponse::AvailableActivities { total, activities }, .. } => {
            assert_eq!(total, 2);
            assert_eq!(activities.len(), 2);
            assert_eq!(activities[0].activity, "act-1");
            assert_eq!(activities[1].activity, "act-2");
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn test_event_activity_updates_roundtrip() {
    let event = XacppEvent::ActivityUpdates(ActivityInfo {
        activity: "act-1".into(),
        agent: "x-agent".into(),
        title: Some("updated title".into()),
    });
    let json = serde_json::to_string(&event).unwrap();
    assert!(json.contains("\"type\":\"activity_updates\""), "json: {json}");
    assert!(json.contains("\"activity\":\"act-1\""), "json: {json}");

    let de: XacppEvent = serde_json::from_str(&json).unwrap();
    match de {
        XacppEvent::ActivityUpdates(info) => {
            assert_eq!(info.activity, "act-1");
            assert_eq!(info.agent, "x-agent");
        }
        other => panic!("unexpected: {other:?}"),
    }
}
