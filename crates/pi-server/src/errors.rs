//! Host and lifecycle errors that can safely cross the protocol boundary.

use std::fmt;

use pi_chord::{ChordError, RemoteServiceErrorCode};
use pi_protocol::ProtocolValidationError;

/// Message sent for every failure that is not safe to expose.
pub const INTERNAL_SERVER_ERROR_MESSAGE: &str = "Internal server error";

/// The stable codes a [`ServerError`] may carry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ServerErrorCode {
    Remote(RemoteServiceErrorCode),
    WrongServer,
    SessionNotFound,
    SessionAmbiguous,
    SessionNotAttached,
    ServerDraining,
}

impl ServerErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Remote(code) => code.as_str(),
            Self::WrongServer => "wrong_server",
            Self::SessionNotFound => "session_not_found",
            Self::SessionAmbiguous => "session_ambiguous",
            Self::SessionNotAttached => "session_not_attached",
            Self::ServerDraining => "server_draining",
        }
    }
}

impl fmt::Display for ServerErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A host or lifecycle error whose code and message reach the client unchanged.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct ServerError {
    pub code: ServerErrorCode,
    pub message: String,
}

impl ServerError {
    pub fn new(code: ServerErrorCode, message: impl Into<String>) -> Self {
        Self { code, message: message.into() }
    }

    pub fn wrong_server() -> Self {
        Self::new(ServerErrorCode::WrongServer, "Request was addressed to another server")
    }

    pub fn session_not_found(message: impl Into<String>) -> Self {
        Self::new(ServerErrorCode::SessionNotFound, message)
    }

    pub fn session_ambiguous() -> Self {
        Self::new(ServerErrorCode::SessionAmbiguous, "Session ID matches more than one session")
    }

    pub fn session_not_attached() -> Self {
        Self::new(ServerErrorCode::SessionNotAttached, "Session is not attached to this client")
    }

    pub fn server_draining() -> Self {
        Self::new(ServerErrorCode::ServerDraining, "Server is draining")
    }

    /// Find a `ServerError` carried by a Chord error.
    pub fn find(error: &ChordError) -> Option<&ServerError> {
        match error {
            ChordError::Other(inner) => inner.downcast_ref::<ServerError>(),
            _ => None,
        }
    }
}

impl From<ServerError> for ChordError {
    fn from(error: ServerError) -> Self {
        ChordError::other(error)
    }
}

/// A Chord error carrying a protocol validation failure, reported as `invalid_request`.
pub fn validation_error(message: impl Into<String>) -> ChordError {
    ChordError::other(ProtocolValidationError::new(message))
}

pub(crate) fn find_validation(error: &ChordError) -> Option<&ProtocolValidationError> {
    match error {
        ChordError::Other(inner) => inner.downcast_ref::<ProtocolValidationError>(),
        _ => None,
    }
}
