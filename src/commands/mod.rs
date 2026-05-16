//! XACPP protocol command types.
//!
//! Commands are passed through transport, driving the interaction flow between xagent and peers.
//! Command payload definitions are in submodules under the same directory.

use serde::{Deserialize, Serialize};

use crate::events::content::ContentPart;

/// XACPP protocol command.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", rename_all_fields = "camelCase")]
pub enum XacppCommand {
    /// Establish a logical session.
    ///
    /// First connection carries no credentials (`credentials` is None), responder asks user for trust;
    /// after user agrees, credentials and session identifier are issued. Subsequent connections carry saved credentials,
    /// responder verifies and issues session identifier.
    Establish {
        /// Authentication credentials. None for first connection.
        #[serde(skip_serializing_if = "Option::is_none")]
        credentials: Option<String>,
    },

    /// Confirm establishment after challenge verification (phase 3 of 3-way handshake).
    EstablishConfirm,

    /// Resume the last active Activity.
    LastActivity,

    /// Create a new Activity session.
    NewActivity {
        #[serde(skip_serializing_if = "Option::is_none")]
        title: Option<String>,
    },

    /// List available Activities with pagination.
    ListActivity {
        #[serde(skip_serializing_if = "Option::is_none")]
        query: Option<String>,
        page_num: u32,
        page_size: u32,
    },

    /// Switch to an existing Activity.
    SwitchActivity {
        /// Target activity unique identifier.
        activity: String,
    },

    /// Invoke an existing Activity to perform operations.
    InvokeActivity {
        /// Target activity identifier.
        activity: String,
        /// Input messages for the activity.
        messages: Vec<ContentPart>,
    },

    /// Compact Activity (reclaim resources / generate snapshot summary).
    CompactActivity {
        /// Target activity identifier.
        activity: String,
    },

    /// Cancel Activity.
    CancelActivity {
        /// Target activity identifier.
        activity: String,
    },

    /// Send a message outside of any activity context.
    Message {
        content: Vec<ContentPart>,
    },
}
