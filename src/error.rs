//! XACPP error types.
//!
//! Unified error type across the crate, covering connection, processing, and application layers.
//! Predefined variants ensure consistent error codes and descriptions across all consumers.
//! Application layer can extend with custom errors via `Application` variant.
//!
//! Wire transmission is carried by `XacppResponse::Error` (`code` + `message`),
//! this type is only used within the process.

/// XACPP error.
#[derive(Debug, Clone, thiserror::Error)]
pub enum XacppError {
    // ---- Connection ----
    /// No connection established when performing operation.
    #[error("not connected")]
    NotConnected,
    /// Duplicate connection.
    #[error("already connected")]
    AlreadyConnected,
    /// Connection interrupted.
    #[error("connection closed")]
    Closed,

    // ---- Processing ----
    /// No handler registered for request type.
    #[error("no handler registered")]
    NoHandler,
    /// Request payload cannot be parsed.
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    /// Handshake rejected.
    #[error("establish rejected: {reason}")]
    EstablishReject { reason: String },
    /// Internal handler error.
    #[error("internal error: {0}")]
    Internal(String),

    // ---- Application Layer ----
    /// Application layer custom error.
    #[error("[{code}] {message}")]
    Application { code: String, message: String },
}

impl XacppError {
    /// Machine-readable error code.
    pub fn code(&self) -> &str {
        match self {
            Self::NotConnected => "not_connected",
            Self::AlreadyConnected => "already_connected",
            Self::Closed => "closed",
            Self::NoHandler => "no_handler",
            Self::InvalidRequest(_) => "invalid_request",
            Self::EstablishReject { .. } => "establish_rejected",
            Self::Internal(_) => "internal_error",
            Self::Application { code, .. } => code,
        }
    }
}
