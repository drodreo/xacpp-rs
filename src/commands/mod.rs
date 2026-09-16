//! XACPP protocol command types.
//!
//! Commands are request-response: the sender always expects a Response.
//!
//! Wire format uses externally-tagged enum serialization:
//! - Protocol commands (Negotiate/Establish/EstablishConfirm) use their own variant tag.
//! - Business commands use the `Generic` variant wrapper.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::activity_ref::ActivityRef;
use crate::capability::Capabilities;

pub mod lifecycle;

pub use lifecycle::{
    AvailableActivities, CancelActivityPayload, CompactActivityPayload, InvokeActivityPayload,
    LastActivityPayload, ListActivityPayload, NewActivityPayload, SwitchActivityPayload,
};

/// XACPP protocol command.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", rename_all_fields = "camelCase")]
pub enum XacppCommand {
    // ---- Protocol commands (handled by Peer layer) ----
    /// Negotiate capabilities (must precede Establish).
    Negotiate { capabilities: Capabilities },

    /// Establish a logical session.
    Establish {
        #[serde(skip_serializing_if = "Option::is_none")]
        credentials: Option<String>,
    },

    /// Confirm establishment after challenge verification.
    EstablishConfirm,

    // ---- Business command (handled by SessionHandler) ----
    /// Generic business command.
    ///
    /// `name` identifies the command type (e.g. "new_activity", "action_request").
    /// `arguments` carries the command-specific JSON payload.
    Generic {
        name: String,
        arguments: Value,
        /// Activity the command originates from. Optional at protocol level;
        /// validation is the command implementor's decision.
        #[serde(skip_serializing_if = "Option::is_none")]
        activity: Option<ActivityRef>,
    },
}

impl XacppCommand {
    /// Convenience constructor for generic business commands.
    pub fn generic(name: impl Into<String>, arguments: Value) -> Self {
        XacppCommand::Generic {
            name: name.into(),
            arguments,
            activity: None,
        }
    }

    /// Convenience constructor for generic commands with an activity reference.
    pub fn generic_with_activity(
        name: impl Into<String>,
        arguments: Value,
        activity: ActivityRef,
    ) -> Self {
        XacppCommand::Generic {
            name: name.into(),
            arguments,
            activity: Some(activity),
        }
    }

    /// Returns the command name for capability-matching purposes.
    ///
    /// Protocol commands return their variant name; generic commands return the `name` field.
    pub fn name(&self) -> &str {
        match self {
            XacppCommand::Negotiate { .. } => "negotiate",
            XacppCommand::Establish { .. } => "establish",
            XacppCommand::EstablishConfirm => "establish_confirm",
            XacppCommand::Generic { name, .. } => name,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_generic_with_activity_serialization() {
        let cmd = XacppCommand::generic_with_activity(
            "report_to_user",
            json!({"content":[]}),
            ActivityRef::new("act-1"),
        );

        let s = serde_json::to_string(&cmd).unwrap();
        assert_eq!(
            s,
            r#"{"generic":{"name":"report_to_user","arguments":{"content":[]},"activity":{"id":"act-1"}}}"#
        );
    }

    #[test]
    fn test_generic_without_activity_omits_field() {
        let cmd = XacppCommand::generic("new_activity", json!({"title":"t"}));

        let s = serde_json::to_string(&cmd).unwrap();
        assert_eq!(
            s,
            r#"{"generic":{"name":"new_activity","arguments":{"title":"t"}}}"#
        );
    }

    #[test]
    fn test_generic_with_activity_roundtrip() {
        let cmd = XacppCommand::generic_with_activity(
            "report_to_user",
            json!({"content":["hello"]}),
            ActivityRef::new("act-1"),
        );

        let s = serde_json::to_string(&cmd).unwrap();
        let de: XacppCommand = serde_json::from_str(&s).unwrap();
        match de {
            XacppCommand::Generic {
                name,
                arguments,
                activity,
            } => {
                assert_eq!(name, "report_to_user");
                assert_eq!(arguments, json!({"content":["hello"]}));
                assert_eq!(activity, Some(ActivityRef::new("act-1")));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn test_generic_deserialize_without_activity() {
        let cmd: XacppCommand = serde_json::from_str(
            r#"{"generic":{"name":"new_activity","arguments":{"title":"t"}}}"#,
        )
        .unwrap();
        match cmd {
            XacppCommand::Generic { activity, .. } => {
                assert_eq!(activity, None);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }
}
