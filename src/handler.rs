//! XACPP Handler type definitions.
//!
//! This module centralizes all handler-related types:
//!
//! - [`RequestHandler`]: Transport layer inbound request callback (parameter type for Transport on_request)
//! - [`XacppSessionHandler`]: Logical session handler (handles Command / Event)
//! - [`EstablishHandler`]: Peer Establish request handler

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;

use crate::commands::XacppCommand;
use crate::error::XacppError;
use crate::events::XacppActivityEvent;
use crate::message::{XacppRequest, XacppResponse};
use crate::transport::XacppTransport;

// ---- Request Callback Types ----

/// Transport layer Request handler callback type.
///
/// Receives Requests (Command or Event) from the peer, returns `Ok(response payload)` on success,
/// returns `Err(XacppError)` on failure (Transport automatically constructs Error response to send back).
///
/// Wrapped with `Arc` to support concurrent calls from multiple tasks (SocketTransport's spawn-per-request model).
pub type RequestHandler = Arc<
    dyn Fn(Option<String>, XacppRequest)
        -> Pin<Box<dyn Future<Output = Result<XacppResponse, XacppError>> + Send>>
        + Send
        + Sync,
>;

// ---- Session Handler ----

/// XACPP Session Handler trait.
///
/// Each logical session holds one implementation, handling Commands and Events from the peer.
#[async_trait]
pub trait XacppSessionHandler: Send + Sync {
    /// Handles Command.
    async fn on_command(&self, command: XacppCommand) -> Result<XacppResponse, XacppError>;

    /// Handles Event.
    async fn on_event(&self, event: XacppActivityEvent) -> Result<XacppResponse, XacppError>;
}

/// Decision made by the responder upon receiving an Establish request.
pub enum EstablishDecision {
    /// First connection: challenge required → responder returns EstablishPrepare.
    ChallengeRequired {
        challenge: String,
    },
    /// Credentials valid: direct establishment → responder returns Established.
    Established {
        session_id: String,
        handler: Arc<dyn XacppSessionHandler>,
    },
}

/// Peer Establish request handler — serve main function.
///
/// Responder invokes this when receiving Establish command from the peer.
/// Developer completes credential verification, creates and holds Session (for subsequent outgoing messages),
/// creates SessionHandler and returns it. Returning Err rejects the handshake.
///
/// ## Identity Contract
///
/// `credentials` is an identity **anchor** — it never carries user/agent identity directly.
/// Both sides maintain their own internal identity mapping:
///
/// - On first connection (`credentials == None`): the responder performs a trust process,
///   internally associates this connection with a specific user and agent, then issues credentials
///   as an opaque handle to that identity. Neither side transmits user/agent over the wire.
/// - On subsequent connections: the initiator presents saved credentials, the responder looks up
///   the previously associated identity and routes accordingly.
#[async_trait]
pub trait EstablishHandler: Send + Sync {
    /// Handles Establish request.
    ///
    /// `transport` is passed by Peer, for `on_establish` to create `XacppSession` internally.
    /// Returns [`EstablishDecision`] indicating whether to challenge or directly establish.
    async fn on_establish(
        &self,
        transport: Arc<dyn XacppTransport>,
        credentials: Option<String>,
    ) -> Result<EstablishDecision, XacppError>;

    /// Phase 3: EstablishConfirm received (challenge path only).
    /// Returns the session_id and handler for the newly created session.
    async fn on_establish_confirm(
        &self,
        transport: Arc<dyn XacppTransport>,
    ) -> Result<(String, Arc<dyn XacppSessionHandler>), XacppError>;
}
