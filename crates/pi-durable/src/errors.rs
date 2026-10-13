//! Errors of the durable runtime. Upstream throws distinct error classes; one cloneable enum carries the same
//! cases here, so a failure can reach every waiter of a failed Session.

use std::sync::Arc;

use pi_chord::ChordError;

use crate::ids::ConversationId;

/// Every failure the durable runtime reports.
#[derive(Clone, Debug, thiserror::Error)]
pub enum DurableError {
    /// A caller contract violation (JavaScript `TypeError`).
    #[error("{0}")]
    Type(String),
    /// A runtime failure (JavaScript `Error`).
    #[error("{0}")]
    Error(String),
    /// A Storage read rejected an invalid request with no durable effect. It fails only that read.
    #[error("{0}")]
    StorageRequest(String),
    /// A Storage backend failure: I/O, SQLite, or corrupt data. It fails the Session.
    #[error("{0}")]
    Storage(String),
    /// A transaction read a table after its first table write.
    #[error("Tx.{0}() cannot read tables after the first table write")]
    ReadAfterWrite(String),
    /// The Session failed, so nothing runs or commits any more; the cause is the first error.
    #[error("Session failed after a storage error; close and reopen it")]
    SessionFailed(Arc<DurableError>),
    /// A submission reached a busy conversation and was not admitted.
    #[error("Conversation {0} is busy")]
    ConversationBusy(ConversationId),
    /// A Chord failure, including cancellation of the caller's context.
    #[error(transparent)]
    Chord(ChordError),
}

pub type DurableResult<T> = Result<T, DurableError>;

impl DurableError {
    pub fn type_error(message: impl Into<String>) -> Self {
        Self::Type(message.into())
    }

    pub fn error(message: impl Into<String>) -> Self {
        Self::Error(message.into())
    }

    pub fn storage(message: impl Into<String>) -> Self {
        Self::Storage(message.into())
    }

    pub fn storage_request(message: impl Into<String>) -> Self {
        Self::StorageRequest(message.into())
    }

    pub fn session_failed(cause: DurableError) -> Self {
        match cause {
            failed @ Self::SessionFailed(_) => failed,
            cause => Self::SessionFailed(Arc::new(cause)),
        }
    }

    /// The cancellation error of an aborted context.
    pub fn aborted() -> Self {
        Self::Chord(ChordError::Aborted)
    }

    /// Whether this is the cancellation of a context rather than a failure.
    pub fn is_aborted(&self) -> bool {
        matches!(self, Self::Chord(ChordError::Aborted))
    }

    pub fn is_storage_request(&self) -> bool {
        matches!(self, Self::StorageRequest(_))
    }

    /// The first error of a failed Session, when this is `SessionFailed`.
    pub fn session_failure_cause(&self) -> Option<&DurableError> {
        match self {
            Self::SessionFailed(cause) => Some(cause),
            _ => None,
        }
    }

    /// Upstream error class name: `TypeError`, `StorageRequestError`, `SessionFailed`, and so on.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Type(_) => "TypeError",
            Self::Error(_) | Self::Storage(_) => "Error",
            Self::StorageRequest(_) => "StorageRequestError",
            Self::ReadAfterWrite(_) => "ReadAfterWrite",
            Self::SessionFailed(_) => "SessionFailed",
            Self::ConversationBusy(_) => "ConversationBusy",
            Self::Chord(ChordError::Type(_)) => "TypeError",
            Self::Chord(ChordError::Aborted) => "AbortError",
            Self::Chord(_) => "Error",
        }
    }
}

impl From<ChordError> for DurableError {
    fn from(error: ChordError) -> Self {
        match error {
            ChordError::Other(inner) => match inner.downcast_ref::<DurableError>() {
                Some(durable) => durable.clone(),
                None => Self::Chord(ChordError::Other(inner)),
            },
            other => Self::Chord(other),
        }
    }
}

impl From<DurableError> for ChordError {
    fn from(error: DurableError) -> Self {
        match error {
            DurableError::Chord(chord) => chord,
            DurableError::Type(message) => ChordError::Type(message),
            other => ChordError::Other(Arc::new(other)),
        }
    }
}

impl From<serde_json::Error> for DurableError {
    fn from(error: serde_json::Error) -> Self {
        Self::Type(error.to_string())
    }
}
