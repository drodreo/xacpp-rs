//! XACPP protocol command types.
//!
//! Commands are request-response: the sender always expects a Response.
//!
//! Wire format uses externally-tagged enum serialization:
//! - Protocol commands (Negotiate/Establish/EstablishConfirm) use their own variant tag.
//! - Business commands use the `Generic` variant wrapper.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::capability::Capabilities;

/// XACPP protocol command.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", rename_all_fields = "camelCase")]
pub enum XacppCommand {
    // ---- Protocol commands (handled by Peer layer) ----

    /// Negotiate capabilities (must precede Establish).
    Negotiate {
        capabilities: Capabilities,
    },

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
    },
}

impl XacppCommand {
    /// Convenience constructor for generic business commands.
    pub fn generic(name: impl Into<String>, arguments: Value) -> Self {
        XacppCommand::Generic {
            name: name.into(),
            arguments,
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
