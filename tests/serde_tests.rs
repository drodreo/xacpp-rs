//! Serialization / deserialization correctness tests.
//!
//! Verifies round-trip of envelope layer + payload layer, including type tag routing
//! and payload nested structures. All business types now use the generic
//! `{ name, data }` / `{ name, arguments }` structure.

use serde_json::json;

use xacpp::activity_ref::ActivityRef;
use xacpp::commands::XacppCommand;
use xacpp::events::content::FileRef;
use xacpp::events::interaction::{
    ActionRequestPayload, ActionResponse, QuestionPayload, QuestionResponse, SensitiveInfoItem,
    SensitiveInfoOperation, SensitiveInfoOperationPayload, SensitiveInfoOperationResponse,
    SensitiveInfoResult, SensitiveInfoType, action_request_command, question_command,
    sensitive_info_command,
};
use xacpp::events::payload::{AlertLevel, TraceableEvent};
use xacpp::events::{XacppActivityEvent, XacppEvent};
use xacpp::message::{XacppEnvelope, XacppRequest, XacppResponse};

// =========================================================================
// XacppEvent round-trip (generic { name, data } structure)
// =========================================================================

#[test]
fn test_event_basic_roundtrip() {
    let event = XacppEvent::new("content_delta", json!({ "content": "hello" }));

    let s = serde_json::to_string(&event).unwrap();
    assert!(s.contains(r#""name":"content_delta""#), "json: {s}");
    assert!(s.contains(r#""data":{"content":"hello"}"#), "json: {s}");

    let de: XacppEvent = serde_json::from_str(&s).unwrap();
    assert_eq!(de.name, "content_delta");
    assert_eq!(de.data["content"], "hello");
}

#[test]
fn test_event_with_null_data_roundtrip() {
    let event = XacppEvent::new("complete", json!(null));

    let s = serde_json::to_string(&event).unwrap();
    assert!(s.contains(r#""name":"complete""#), "json: {s}");

    let de: XacppEvent = serde_json::from_str(&s).unwrap();
    assert_eq!(de.name, "complete");
    assert!(de.data.is_null());
}

#[test]
fn test_event_default_data_omitted() {
    // XacppEvent has #[serde(default)] on data, so deserialization without data should work
    let json = r#"{"name":"think"}"#;
    let de: XacppEvent = serde_json::from_str(json).unwrap();
    assert_eq!(de.name, "think");
    assert!(de.data.is_null()); // defaults to Null
}

#[test]
fn test_event_think_roundtrip() {
    let event = XacppEvent::new("think", json!({ "content": "thinking..." }));

    let s = serde_json::to_string(&event).unwrap();
    assert!(s.contains(r#""name":"think""#), "json: {s}");

    let de: XacppEvent = serde_json::from_str(&s).unwrap();
    assert_eq!(de.name, "think");
    assert_eq!(de.data["content"], "thinking...");
}

#[test]
fn test_event_info_roundtrip() {
    let payload = TraceableEvent {
        title: "started".into(),
        content: "".into(),
    };
    let event = XacppEvent::new("info", serde_json::to_value(&payload).unwrap());

    let s = serde_json::to_string(&event).unwrap();
    assert!(s.contains(r#""name":"info""#), "json: {s}");
    assert!(s.contains(r#""title":"started""#), "json: {s}");

    let de: XacppEvent = serde_json::from_str(&s).unwrap();
    assert_eq!(de.name, "info");
    assert_eq!(de.data["title"], "started");
}

// =========================================================================
// XacppCommand round-trip
// =========================================================================

#[test]
fn test_command_generic_roundtrip() {
    let cmd = XacppCommand::generic("new_activity", json!({ "title": "test" }));

    let s = serde_json::to_string(&cmd).unwrap();
    assert!(s.contains(r#""generic""#), "json: {s}");
    assert!(s.contains(r#""name":"new_activity""#), "json: {s}");
    assert!(s.contains(r#""arguments":{"title":"test"}"#), "json: {s}");

    let de: XacppCommand = serde_json::from_str(&s).unwrap();
    match de {
        XacppCommand::Generic {
            name, arguments, ..
        } => {
            assert_eq!(name, "new_activity");
            assert_eq!(arguments["title"], "test");
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn test_command_generic_empty_arguments() {
    let cmd = XacppCommand::generic("last_activity", json!({}));

    let s = serde_json::to_string(&cmd).unwrap();
    assert!(s.contains(r#""name":"last_activity""#), "json: {s}");

    let de: XacppCommand = serde_json::from_str(&s).unwrap();
    match de {
        XacppCommand::Generic {
            name, arguments, ..
        } => {
            assert_eq!(name, "last_activity");
            assert!(arguments.is_object());
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn test_command_establish_roundtrip() {
    let cmd = XacppCommand::Establish { credentials: None };

    let s = serde_json::to_string(&cmd).unwrap();
    assert!(s.contains(r#""establish""#), "json: {s}");

    let de: XacppCommand = serde_json::from_str(&s).unwrap();
    assert!(matches!(de, XacppCommand::Establish { credentials: None }));
}

#[test]
fn test_command_establish_with_credentials_roundtrip() {
    let cmd = XacppCommand::Establish {
        credentials: Some("my-creds".into()),
    };

    let s = serde_json::to_string(&cmd).unwrap();
    assert!(s.contains(r#""establish""#), "json: {s}");
    assert!(s.contains(r#""credentials":"my-creds""#), "json: {s}");

    let de: XacppCommand = serde_json::from_str(&s).unwrap();
    match de {
        XacppCommand::Establish { credentials } => {
            assert_eq!(credentials.as_deref(), Some("my-creds"));
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn test_command_negotiate_roundtrip() {
    let cmd = XacppCommand::Negotiate {
        capabilities: xacpp::capability::Capabilities {
            commands: vec![json!({ "name": "new_activity" })],
            produce_events: vec![json!({ "name": "content_delta" })],
            accept_events: Vec::new(),
        },
    };

    let s = serde_json::to_string(&cmd).unwrap();
    assert!(s.contains(r#""negotiate""#), "json: {s}");

    let de: XacppCommand = serde_json::from_str(&s).unwrap();
    match de {
        XacppCommand::Negotiate { capabilities } => {
            assert_eq!(capabilities.commands.len(), 1);
            assert_eq!(capabilities.produce_events.len(), 1);
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn test_command_establish_confirm_roundtrip() {
    let cmd = XacppCommand::EstablishConfirm;

    let s = serde_json::to_string(&cmd).unwrap();
    assert!(s.contains(r#""establish_confirm""#), "json: {s}");

    let de: XacppCommand = serde_json::from_str(&s).unwrap();
    assert!(matches!(de, XacppCommand::EstablishConfirm));
}

// =========================================================================
// XacppResponse round-trip
// =========================================================================

#[test]
fn test_response_acknowledge_roundtrip() {
    let resp = XacppResponse::acknowledge();

    let s = serde_json::to_string(&resp).unwrap();
    assert!(s.contains(r#""generic""#), "json: {s}");
    assert!(s.contains(r#""name":"acknowledge""#), "json: {s}");

    let de: XacppResponse = serde_json::from_str(&s).unwrap();
    match de {
        XacppResponse::Generic { name, data } => {
            assert_eq!(name, "acknowledge");
            assert!(data.is_null());
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn test_response_generic_roundtrip() {
    let resp = XacppResponse::generic("activity_ready", json!({ "activity": "act-1" }));

    let s = serde_json::to_string(&resp).unwrap();
    assert!(s.contains(r#""generic""#), "json: {s}");
    assert!(s.contains(r#""name":"activity_ready""#), "json: {s}");
    assert!(s.contains(r#""data":{"activity":"act-1"}"#), "json: {s}");

    let de: XacppResponse = serde_json::from_str(&s).unwrap();
    match de {
        XacppResponse::Generic { name, data } => {
            assert_eq!(name, "activity_ready");
            assert_eq!(data["activity"], "act-1");
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn test_response_error_roundtrip() {
    let resp = XacppResponse::Error {
        code: "internal_error".into(),
        message: "something went wrong".into(),
    };

    let s = serde_json::to_string(&resp).unwrap();
    assert!(s.contains(r#""error""#), "json: {s}");
    assert!(s.contains(r#""code":"internal_error""#), "json: {s}");
    assert!(
        s.contains(r#""message":"something went wrong""#),
        "json: {s}"
    );

    let de: XacppResponse = serde_json::from_str(&s).unwrap();
    match de {
        XacppResponse::Error { code, message } => {
            assert_eq!(code, "internal_error");
            assert_eq!(message, "something went wrong");
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn test_response_established_roundtrip() {
    let resp = XacppResponse::Established {
        session_id: "s1".into(),
        credentials: "creds".into(),
    };

    let s = serde_json::to_string(&resp).unwrap();
    assert!(s.contains(r#""established""#), "json: {s}");
    assert!(s.contains(r#""sessionId":"s1""#), "json: {s}");
    assert!(s.contains(r#""credentials":"creds""#), "json: {s}");

    let de: XacppResponse = serde_json::from_str(&s).unwrap();
    assert!(matches!(de, XacppResponse::Established { .. }));
}

#[test]
fn test_response_negotiated_roundtrip() {
    let resp = XacppResponse::Negotiated {
        capabilities: xacpp::capability::Capabilities {
            commands: vec![json!({ "name": "new_activity" })],
            produce_events: vec![],
            accept_events: Vec::new(),
        },
    };

    let s = serde_json::to_string(&resp).unwrap();
    assert!(s.contains(r#""negotiated""#), "json: {s}");

    let de: XacppResponse = serde_json::from_str(&s).unwrap();
    match de {
        XacppResponse::Negotiated { capabilities } => {
            assert_eq!(capabilities.commands.len(), 1);
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn test_response_establish_prepare_roundtrip() {
    let resp = XacppResponse::EstablishPrepare {
        challenge: "prove-yourself".into(),
    };

    let s = serde_json::to_string(&resp).unwrap();
    assert!(s.contains(r#""establish_prepare""#), "json: {s}");
    assert!(s.contains(r#""challenge":"prove-yourself""#), "json: {s}");

    let de: XacppResponse = serde_json::from_str(&s).unwrap();
    assert!(matches!(de, XacppResponse::EstablishPrepare { .. }));
}

#[test]
fn test_response_establish_reject_roundtrip() {
    let resp = XacppResponse::EstablishReject {
        reason: "bad creds".into(),
    };

    let s = serde_json::to_string(&resp).unwrap();
    assert!(s.contains(r#""establish_reject""#), "json: {s}");
    assert!(s.contains(r#""reason":"bad creds""#), "json: {s}");

    let de: XacppResponse = serde_json::from_str(&s).unwrap();
    assert!(matches!(de, XacppResponse::EstablishReject { .. }));
}

// =========================================================================
// XacppEnvelope wire format round-trip
// =========================================================================

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
        XacppEnvelope::Request {
            id,
            session_id: _,
            payload,
        } => {
            assert_eq!(id, "r1");
            assert!(matches!(
                payload,
                XacppRequest::Command(XacppCommand::Establish { credentials: None })
            ));
        }
        XacppEnvelope::Response { .. } => panic!("expected Request"),
    }
}

#[test]
fn test_wire_request_generic_command_roundtrip() {
    let wire = XacppEnvelope::Request {
        id: "r1".into(),
        session_id: Some("s1".into()),
        payload: XacppRequest::Command(XacppCommand::generic(
            "new_activity",
            json!({ "title": "test" }),
        )),
    };
    let json = serde_json::to_string(&wire).unwrap();
    assert!(json.contains(r#""type":"request""#), "json: {json}");
    assert!(json.contains(r#""id":"r1""#), "json: {json}");
    assert!(json.contains(r#""session_id":"s1""#), "json: {json}");
    assert!(json.contains(r#""kind":"command""#), "json: {json}");
    assert!(json.contains(r#""generic""#), "json: {json}");
    assert!(json.contains(r#""name":"new_activity""#), "json: {json}");

    let de: XacppEnvelope = serde_json::from_str(&json).unwrap();
    match de {
        XacppEnvelope::Request { id, payload, .. } => {
            assert_eq!(id, "r1");
            match payload {
                XacppRequest::Command(XacppCommand::Generic {
                    name, arguments, ..
                }) => {
                    assert_eq!(name, "new_activity");
                    assert_eq!(arguments["title"], "test");
                }
                other => panic!("unexpected: {other:?}"),
            }
        }
        XacppEnvelope::Response { .. } => panic!("expected Request"),
    }
}

#[test]
fn test_wire_request_event_roundtrip() {
    let wire = XacppEnvelope::Request {
        id: "r2".into(),
        session_id: None,
        payload: XacppRequest::Event(XacppActivityEvent {
            activity: ActivityRef::new("test-act"),
            event: XacppEvent::new("think", json!({ "content": "hi" })),
        }),
    };
    let json = serde_json::to_string(&wire).unwrap();
    assert!(json.contains(r#""type":"request""#), "json: {json}");
    assert!(json.contains(r#""id":"r2""#), "json: {json}");
    assert!(json.contains(r#""kind":"event""#), "json: {json}");
    assert!(
        json.contains(r#""activity":{"id":"test-act"}"#),
        "json: {json}"
    );
    assert!(json.contains(r#""name":"think""#), "json: {json}");

    let de: XacppEnvelope = serde_json::from_str(&json).unwrap();
    match de {
        XacppEnvelope::Request {
            id,
            session_id: _,
            payload,
        } => {
            assert_eq!(id, "r2");
            match payload {
                XacppRequest::Event(XacppActivityEvent { activity, event }) => {
                    assert_eq!(activity, ActivityRef::new("test-act"));
                    assert_eq!(event.name, "think");
                    assert_eq!(event.data["content"], "hi");
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
        payload: XacppResponse::Established {
            session_id: "123456".into(),
            credentials: "test-creds".into(),
        },
    };
    let json = serde_json::to_string(&wire).unwrap();
    assert!(json.contains(r#""type":"response""#), "json: {json}");
    assert!(json.contains(r#""id":"r1""#), "json: {json}");
    assert!(json.contains(r#""established""#), "json: {json}");
    assert!(json.contains(r#""sessionId":"123456""#), "json: {json}");

    let de: XacppEnvelope = serde_json::from_str(&json).unwrap();
    match de {
        XacppEnvelope::Response {
            id,
            session_id: _,
            payload,
        } => {
            assert_eq!(id, "r1");
            assert!(matches!(payload, XacppResponse::Established { .. }));
        }
        XacppEnvelope::Request { .. } => panic!("expected Response"),
    }
}

#[test]
fn test_wire_response_generic_roundtrip() {
    let wire = XacppEnvelope::Response {
        id: "r2".into(),
        session_id: None,
        payload: XacppResponse::generic("action", json!({ "requestId": "req-1" })),
    };
    let json = serde_json::to_string(&wire).unwrap();
    assert!(json.contains(r#""type":"response""#), "json: {json}");
    assert!(json.contains(r#""id":"r2""#), "json: {json}");
    assert!(json.contains(r#""generic""#), "json: {json}");
    assert!(json.contains(r#""name":"action""#), "json: {json}");
    assert!(json.contains(r#""requestId":"req-1""#), "json: {json}");

    let de: XacppEnvelope = serde_json::from_str(&json).unwrap();
    match de {
        XacppEnvelope::Response {
            id,
            session_id: _,
            payload,
        } => {
            assert_eq!(id, "r2");
            match payload {
                XacppResponse::Generic { name, data } => {
                    assert_eq!(name, "action");
                    assert_eq!(data["requestId"], "req-1");
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
        payload: XacppResponse::acknowledge(),
    };
    let json = serde_json::to_string(&wire).unwrap();
    assert!(json.contains(r#""type":"response""#), "json: {json}");
    assert!(json.contains(r#""id":"r3""#), "json: {json}");

    let de: XacppEnvelope = serde_json::from_str(&json).unwrap();
    match de {
        XacppEnvelope::Response {
            id,
            session_id: _,
            payload,
        } => {
            assert_eq!(id, "r3");
            match payload {
                XacppResponse::Generic { name, .. } => assert_eq!(name, "acknowledge"),
                other => panic!("unexpected: {other:?}"),
            }
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
    assert!(json.contains(r#""error""#), "json: {json}");
    assert!(json.contains(r#""code":"internal_error""#), "json: {json}");

    let de: XacppEnvelope = serde_json::from_str(&json).unwrap();
    match de {
        XacppEnvelope::Response {
            id,
            session_id: _,
            payload,
        } => {
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

// =========================================================================
// Deserialize from handwritten JSON (wire compatibility)
// =========================================================================

#[test]
fn test_deserialize_request_from_json() {
    let json = br#"{"type":"request","id":"r1","payload":{"kind":"command","payload":{"establish":{"credentials":null}}}}"#;
    let de: XacppEnvelope = serde_json::from_slice(json).unwrap();
    match de {
        XacppEnvelope::Request {
            id, session_id: _, ..
        } => assert_eq!(id, "r1"),
        XacppEnvelope::Response { .. } => panic!("expected Request"),
    }
}

#[test]
fn test_deserialize_generic_command_from_json() {
    let json = br#"{"type":"request","id":"r1","session_id":"s1","payload":{"kind":"command","payload":{"generic":{"name":"new_activity","arguments":{"title":"test"}}}}}"#;
    let de: XacppEnvelope = serde_json::from_slice(json).unwrap();
    match de {
        XacppEnvelope::Request {
            id,
            session_id,
            payload,
        } => {
            assert_eq!(id, "r1");
            assert_eq!(session_id.as_deref(), Some("s1"));
            match payload {
                XacppRequest::Command(XacppCommand::Generic {
                    name, arguments, ..
                }) => {
                    assert_eq!(name, "new_activity");
                    assert_eq!(arguments["title"], "test");
                }
                other => panic!("unexpected: {other:?}"),
            }
        }
        XacppEnvelope::Response { .. } => panic!("expected Request"),
    }
}

#[test]
fn test_deserialize_response_from_json() {
    let json = br#"{"type":"response","id":"r1","payload":{"kind":"established","sessionId":"s1","credentials":"test-creds"}}"#;
    let de: XacppEnvelope = serde_json::from_slice(json).unwrap();
    match de {
        XacppEnvelope::Response {
            id, session_id: _, ..
        } => assert_eq!(id, "r1"),
        XacppEnvelope::Request { .. } => panic!("expected Response"),
    }
}

#[test]
fn test_deserialize_generic_response_from_json() {
    let json = br#"{"type":"response","id":"r1","payload":{"kind":"generic","name":"acknowledge","data":null}}"#;
    let de: XacppEnvelope = serde_json::from_slice(json).unwrap();
    match de {
        XacppEnvelope::Response { id, payload, .. } => {
            assert_eq!(id, "r1");
            match payload {
                XacppResponse::Generic { name, data } => {
                    assert_eq!(name, "acknowledge");
                    assert!(data.is_null());
                }
                other => panic!("unexpected: {other:?}"),
            }
        }
        XacppEnvelope::Request { .. } => panic!("expected Response"),
    }
}

// =========================================================================
// Interaction payload serialization tests
// =========================================================================

#[test]
fn test_action_request_payload_roundtrip() {
    let payload = ActionRequestPayload {
        request_id: "req-1".into(),
        tool_name: "bash".into(),
        arguments: r#"{"command":"ls"}"#.into(),
        action_id: "act-1".into(),
        description: "list files".into(),
        alert: AlertLevel::Warn,
        intent: "list files".into(),
    };
    let json = serde_json::to_string(&payload).unwrap();
    assert!(json.contains(r#""requestId":"req-1""#), "json: {json}");
    assert!(json.contains(r#""toolName":"bash""#), "json: {json}");
    assert!(json.contains(r#""alert":"warn""#), "json: {json}");

    let de: ActionRequestPayload = serde_json::from_str(&json).unwrap();
    assert_eq!(de.request_id, "req-1");
    assert_eq!(de.tool_name, "bash");
    assert_eq!(de.alert, AlertLevel::Warn);
}

#[test]
fn test_action_response_roundtrip() {
    let resp = ActionResponse::Approve;
    let json = serde_json::to_string(&resp).unwrap();
    assert!(json.contains(r#""type":"approve""#), "json: {json}");

    let de: ActionResponse = serde_json::from_str(&json).unwrap();
    assert!(matches!(de, ActionResponse::Approve));

    let reject = ActionResponse::Reject {
        reason: "nope".into(),
    };
    let json = serde_json::to_string(&reject).unwrap();
    assert!(json.contains(r#""type":"reject""#), "json: {json}");
    assert!(json.contains(r#""reason":"nope""#), "json: {json}");

    let de: ActionResponse = serde_json::from_str(&json).unwrap();
    match de {
        ActionResponse::Reject { reason } => assert_eq!(reason, "nope"),
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn test_question_payload_roundtrip() {
    let payload = QuestionPayload {
        request_id: "req-2".into(),
        question: "continue?".into(),
        options: vec!["yes".into(), "no".into()],
    };
    let json = serde_json::to_string(&payload).unwrap();
    assert!(json.contains(r#""requestId":"req-2""#), "json: {json}");
    assert!(json.contains(r#""question":"continue?""#), "json: {json}");
    assert!(json.contains(r#""options""#), "json: {json}");

    let de: QuestionPayload = serde_json::from_str(&json).unwrap();
    assert_eq!(de.request_id, "req-2");
    assert_eq!(de.options.len(), 2);
}

#[test]
fn test_question_response_roundtrip() {
    let resp = QuestionResponse::Answer {
        content: "yes".into(),
    };
    let json = serde_json::to_string(&resp).unwrap();
    assert!(json.contains(r#""type":"answer""#), "json: {json}");
    assert!(json.contains(r#""content":"yes""#), "json: {json}");

    let de: QuestionResponse = serde_json::from_str(&json).unwrap();
    match de {
        QuestionResponse::Answer { content } => assert_eq!(content, "yes"),
        other => panic!("unexpected: {other:?}"),
    }

    let skip = QuestionResponse::Skip { reason: None };
    let json = serde_json::to_string(&skip).unwrap();
    assert!(json.contains(r#""type":"skip""#), "json: {json}");

    let de: QuestionResponse = serde_json::from_str(&json).unwrap();
    assert!(matches!(de, QuestionResponse::Skip { reason: None }));
}

#[test]
fn test_sensitive_info_operation_payload_roundtrip() {
    let payload = SensitiveInfoOperationPayload {
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
    };
    let json = serde_json::to_string(&payload).unwrap();
    assert!(json.contains(r#""requestId":"req-3""#), "json: {json}");
    assert!(json.contains(r#""collect""#), "json: {json}");
    assert!(json.contains(r#""siType":"secret""#), "json: {json}");

    let de: SensitiveInfoOperationPayload = serde_json::from_str(&json).unwrap();
    assert_eq!(de.request_id, "req-3");
    match de.operation {
        SensitiveInfoOperation::Collect { items } => {
            assert_eq!(items.len(), 1);
            assert_eq!(items[0].key, "API_KEY");
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn test_sensitive_info_result_roundtrip() {
    let results = vec![
        SensitiveInfoResult::Provided {
            key: "API_KEY".into(),
            value: "secret".into(),
        },
        SensitiveInfoResult::Deleted {
            id: "item-1".into(),
        },
    ];
    let resp = SensitiveInfoOperationResponse { results };
    let json = serde_json::to_string(&resp).unwrap();
    assert!(json.contains(r#""provided""#), "json: {json}");
    assert!(json.contains(r#""deleted""#), "json: {json}");

    let de: SensitiveInfoOperationResponse = serde_json::from_str(&json).unwrap();
    assert_eq!(de.results.len(), 2);
}

// =========================================================================
// Convenience function tests (interaction command builders)
// =========================================================================

#[test]
fn test_action_request_command_builder() {
    let payload = ActionRequestPayload {
        request_id: "req-1".into(),
        tool_name: "bash".into(),
        arguments: "{}".into(),
        action_id: "act-1".into(),
        description: "test".into(),
        alert: AlertLevel::Info,
        intent: "test".into(),
    };
    let args = action_request_command("act-1", &payload);

    // Should contain all payload fields plus "activity"
    assert_eq!(args["activity"], "act-1");
    assert_eq!(args["requestId"], "req-1");
    assert_eq!(args["toolName"], "bash");

    // Build full command and verify
    let cmd = XacppCommand::generic("action_request", args);
    let json = serde_json::to_string(&cmd).unwrap();
    assert!(json.contains(r#""name":"action_request""#), "json: {json}");
    assert!(json.contains(r#""activity":"act-1""#), "json: {json}");
}

#[test]
fn test_question_command_builder() {
    let payload = QuestionPayload {
        request_id: "req-2".into(),
        question: "continue?".into(),
        options: vec!["yes".into(), "no".into()],
    };
    let args = question_command("act-1", &payload);

    assert_eq!(args["activity"], "act-1");
    assert_eq!(args["requestId"], "req-2");
    assert_eq!(args["question"], "continue?");

    let cmd = XacppCommand::generic("question", args);
    let json = serde_json::to_string(&cmd).unwrap();
    assert!(json.contains(r#""name":"question""#), "json: {json}");
}

#[test]
fn test_sensitive_info_command_builder() {
    let payload = SensitiveInfoOperationPayload {
        request_id: "req-3".into(),
        operation: SensitiveInfoOperation::Collect {
            items: vec![SensitiveInfoItem {
                id: None,
                key: "API_KEY".into(),
                display_text: "API Key".into(),
                hint: "enter key".into(),
                si_type: SensitiveInfoType::Secret,
            }],
        },
    };
    let args = sensitive_info_command("act-1", &payload);

    assert_eq!(args["activity"], "act-1");
    assert_eq!(args["requestId"], "req-3");

    let cmd = XacppCommand::generic("sensitive_info_operation", args);
    let json = serde_json::to_string(&cmd).unwrap();
    assert!(
        json.contains(r#""name":"sensitive_info_operation""#),
        "json: {json}"
    );
}

// =========================================================================
// FileRef round-trip tests (unchanged)
// =========================================================================

#[test]
fn test_fileref_full_roundtrip() {
    let file_ref = FileRef {
        remote_url: "https://example.com/file.png".into(),
        local_uri: "/tmp/file.png".into(),
        remote_expires_at: Some("2026-05-18T12:00:00Z".into()),
        mime_type: "image/png".into(),
        require_organized: true,
        size_bytes: 1024,
        sha256: "abc123".into(),
    };

    let json = serde_json::to_string(&file_ref).unwrap();
    assert!(
        json.contains(r#""remoteUrl":"https://example.com/file.png""#),
        "json: {json}"
    );
    assert!(
        json.contains(r#""localUri":"/tmp/file.png""#),
        "json: {json}"
    );
    assert!(
        json.contains(r#""remoteExpiresAt":"2026-05-18T12:00:00Z""#),
        "json: {json}"
    );
    assert!(json.contains(r#""mimeType":"image/png""#), "json: {json}");
    assert!(json.contains(r#""requireOrganized":true"#), "json: {json}");
    assert!(json.contains(r#""sizeBytes":1024"#), "json: {json}");
    assert!(json.contains(r#""sha256":"abc123""#), "json: {json}");

    let de: FileRef = serde_json::from_str(&json).unwrap();
    assert_eq!(de, file_ref);
}

#[test]
fn test_fileref_defaults_roundtrip() {
    let file_ref = FileRef {
        remote_url: "https://example.com/file.png".into(),
        local_uri: "/tmp/file.png".into(),
        remote_expires_at: None,
        mime_type: "image/png".into(),
        require_organized: false,
        size_bytes: 1024,
        sha256: "".into(),
    };

    let json = serde_json::to_string(&file_ref).unwrap();
    assert!(!json.contains("remoteExpiresAt"), "json: {json}");
    assert!(json.contains(r#""requireOrganized":false"#), "json: {json}");

    let de: FileRef = serde_json::from_str(&json).unwrap();
    assert_eq!(de, file_ref);
}

#[test]
fn test_fileref_deserialize_legacy_format() {
    let json = r#"{"remoteUrl":"https://example.com/old.png","localUri":"/tmp/old.png","mimeType":"image/png","sizeBytes":512}"#;
    let de: FileRef = serde_json::from_str(json).unwrap();
    assert_eq!(de.remote_url, "https://example.com/old.png");
    assert_eq!(de.local_uri, "/tmp/old.png");
    assert_eq!(de.remote_expires_at, None);
    assert_eq!(de.mime_type, "image/png");
    assert_eq!(de.require_organized, false);
    assert_eq!(de.size_bytes, 512);
    assert_eq!(de.sha256, "");
}
