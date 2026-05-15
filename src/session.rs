//! XACPP logical session.
//!
//! Created via `XacppPeer::establish`, holds an independent session_id and credentials.
//! Multiple Sessions under the same Peer share the same connection.

use std::sync::Arc;

use crate::commands::XacppCommand;
use crate::error::XacppError;
use crate::events::XacppActivityEvent;
use crate::message::{XacppRequest, XacppResponse};
use crate::transport::XacppTransport;

/// XACPP logical session.
///
/// Created via `XacppPeer::establish`, holds an independent session_id and credentials.
/// Multiple Sessions under the same Peer share the same connection.
pub struct XacppSession {
    transport: Arc<dyn XacppTransport>,
    session_id: String,
    credentials: String,
}

impl XacppSession {
    pub(crate) fn new(
        transport: Arc<dyn XacppTransport>,
        session_id: String,
        credentials: String,
    ) -> Self {
        Self {
            transport,
            session_id,
            credentials,
        }
    }

    /// Session identifier.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Credentials issued by the responder.
    ///
    /// Caller can save them and pass them in during next `establish`.
    pub fn credentials(&self) -> &str {
        &self.credentials
    }

    /// Sends a command and waits for a response.
    pub async fn request_command(
        &self,
        command: XacppCommand,
    ) -> Result<XacppResponse, XacppError> {
        self.transport
            .send(Some(&self.session_id), XacppRequest::Command(command))
            .await
    }

    /// Sends an event and waits for a response.
    pub async fn request_event(
        &self,
        event: XacppActivityEvent,
    ) -> Result<XacppResponse, XacppError> {
        self.transport
            .send(Some(&self.session_id), XacppRequest::Event(event))
            .await
    }
}
