//! Client failures. Cloneable so one failure can reject every waiter.

use std::sync::Arc;

use pi_chord::{ChordError, RemoteServiceErrorCode};
use pi_protocol::{ProtocolError, ProtocolValidationError};

#[derive(Clone, Debug, thiserror::Error)]
pub enum ClientError {
    /// A bounded error reported by the server.
    #[error("{message}")]
    Server { code: String, message: String },
    /// The connection is gone. `cause` holds the transport failure, when there was one.
    #[error("{message}")]
    Disconnected { message: String, cause: Option<Arc<ClientError>> },
    #[error("Client is disposed")]
    Disposed,
    #[error("{0}")]
    Validation(#[from] ProtocolValidationError),
    /// The caller's abort signal fired, with its reason.
    #[error("{0}")]
    Aborted(ChordError),
    #[error("{0}")]
    Io(Arc<std::io::Error>),
    /// Invalid options or arguments.
    #[error("{0}")]
    Type(String),
    #[error("{0}")]
    Other(String),
}

impl ClientError {
    /// The upstream error class name, such as `ServerError` or `DisconnectedError`.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Server { .. } => "ServerError",
            Self::Disconnected { .. } => "DisconnectedError",
            Self::Disposed => "ClientDisposedError",
            Self::Validation(_) => "ProtocolValidationError",
            Self::Aborted(_) => "AbortError",
            Self::Type(_) => "TypeError",
            Self::Io(_) | Self::Other(_) => "Error",
        }
    }

    pub fn disconnected() -> Self {
        Self::Disconnected { message: "Client is disconnected".into(), cause: None }
    }

    pub fn disconnected_with(message: impl Into<String>) -> Self {
        Self::Disconnected { message: message.into(), cause: None }
    }

    pub fn validation(message: impl Into<String>) -> Self {
        Self::Validation(ProtocolValidationError::new(message))
    }

    pub fn server(error: &ProtocolError) -> Self {
        Self::Server { code: error.code.clone(), message: error.message.clone() }
    }

    /// The server error code, when this is a server error.
    pub fn code(&self) -> Option<&str> {
        match self {
            Self::Server { code, .. } => Some(code),
            _ => None,
        }
    }

    /// Wrap a transport failure as a disconnection that keeps the original as its cause.
    pub fn to_disconnected(self) -> Self {
        match self {
            Self::Disconnected { .. } => self,
            other => Self::Disconnected { message: other.to_string(), cause: Some(Arc::new(other)) },
        }
    }

    /// The I/O error kind of this error or any cause.
    pub fn io_kind(&self) -> Option<std::io::ErrorKind> {
        match self {
            Self::Io(error) => Some(error.kind()),
            Self::Disconnected { cause: Some(cause), .. } => cause.io_kind(),
            _ => None,
        }
    }
}

impl From<std::io::Error> for ClientError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(Arc::new(error))
    }
}

impl From<ClientError> for ChordError {
    fn from(error: ClientError) -> Self {
        match error {
            ClientError::Server { ref code, ref message } => match RemoteServiceErrorCode::parse(code) {
                Some(code) => ChordError::remote(code, message.clone()),
                None => ChordError::other(error),
            },
            ClientError::Aborted(reason) => reason,
            other => ChordError::other(other),
        }
    }
}
