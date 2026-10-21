//! Errors shared by every Chord layer, plus the stable remote service error codes.

use std::fmt;
use std::sync::{Arc, RwLock};

use crate::host2::ChordErrorCode;
use crate::json::Json;

/// Service errors that may cross a remote boundary, in upstream order.
pub const REMOTE_SERVICE_ERROR_CODES: [&str; 8] = [
    "service_not_allowed",
    "service_not_found",
    "service_mode_mismatch",
    "service_member_not_found",
    "service_member_mismatch",
    "service_instance_not_found",
    "service_stale_instance",
    "service_invalid_value",
];

/// A stable remote service error code.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RemoteServiceErrorCode {
    ServiceNotAllowed,
    ServiceNotFound,
    ServiceModeMismatch,
    ServiceMemberNotFound,
    ServiceMemberMismatch,
    ServiceInstanceNotFound,
    ServiceStaleInstance,
    ServiceInvalidValue,
}

impl RemoteServiceErrorCode {
    const ALL: [Self; 8] = [
        Self::ServiceNotAllowed,
        Self::ServiceNotFound,
        Self::ServiceModeMismatch,
        Self::ServiceMemberNotFound,
        Self::ServiceMemberMismatch,
        Self::ServiceInstanceNotFound,
        Self::ServiceStaleInstance,
        Self::ServiceInvalidValue,
    ];

    /// The wire spelling, for example `service_not_found`.
    pub fn as_str(self) -> &'static str {
        REMOTE_SERVICE_ERROR_CODES[self as usize]
    }

    /// Parse a wire spelling.
    pub fn parse(value: &str) -> Option<Self> {
        REMOTE_SERVICE_ERROR_CODES.iter().position(|code| *code == value).map(|index| Self::ALL[index])
    }
}

impl fmt::Display for RemoteServiceErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Whether a value is a remote service error code string.
pub fn is_remote_service_error_code(value: &Json) -> bool {
    value.as_str().is_some_and(|text| RemoteServiceErrorCode::parse(text).is_some())
}

/// A service error with a stable code and a sanitized message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteServiceError {
    pub code: RemoteServiceErrorCode,
    pub message: String,
}

impl RemoteServiceError {
    pub fn new(code: RemoteServiceErrorCode, message: impl Into<String>) -> Self {
        Self { code, message: message.into() }
    }
}

impl fmt::Display for RemoteServiceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for RemoteServiceError {}

/// Every failure Chord reports. Cloneable so one failure can reach several waiters.
#[derive(Clone, Debug, thiserror::Error)]
pub enum ChordError {
    /// A contract violation by the caller (JavaScript `TypeError`).
    #[error("{0}")]
    Type(String),
    /// A runtime or lifecycle failure (JavaScript `Error`).
    #[error("{0}")]
    Error(String),
    #[error(transparent)]
    Remote(#[from] RemoteServiceError),
    /// An operation path that does not resolve in the target.
    #[error("unresolvable path: {path}")]
    Path { path: String },
    /// A path segment that is reserved or not a valid array index.
    #[error("unsafe path segment: {segment}")]
    UnsafePath { segment: String },
    /// Several failures collected after attempting every step.
    #[error("{message}")]
    Aggregate { message: String, errors: Vec<ChordError> },
    /// Cancellation without an error reason.
    #[error("The operation was aborted")]
    Aborted,
    /// An application-defined failure.
    #[error("{0}")]
    Other(Arc<dyn std::error::Error + Send + Sync>),
    /// A host runtime error (upstream chord2 `ChordError`): `message` starts with `code/reason: `.
    /// Build it with [`ChordError::coded`] so the prefix matches the fields.
    #[error("{message}")]
    Coded {
        code: ChordErrorCode,
        reason: Option<Arc<str>>,
        message: Arc<str>,
        details: Option<Json>,
        #[source]
        cause: Option<Arc<ChordError>>,
    },
}

impl ChordError {
    pub fn type_error(message: impl Into<String>) -> Self {
        Self::Type(message.into())
    }

    pub fn error(message: impl Into<String>) -> Self {
        Self::Error(message.into())
    }

    pub fn remote(code: RemoteServiceErrorCode, message: impl Into<String>) -> Self {
        Self::Remote(RemoteServiceError::new(code, message))
    }

    pub fn other(error: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self::Other(Arc::new(error))
    }

    /// The host runtime code, when this is a [`ChordError::Coded`] error.
    pub fn code(&self) -> Option<ChordErrorCode> {
        match self {
            Self::Coded { code, .. } => Some(*code),
            _ => None,
        }
    }

    /// The finer cause inside the code, such as `args` for `invalid`.
    pub fn reason(&self) -> Option<&str> {
        match self {
            Self::Coded { reason, .. } => reason.as_deref(),
            _ => None,
        }
    }

    /// Structured detail of a coded error, such as the schema errors of `invalid`.
    pub fn details(&self) -> Option<&Json> {
        match self {
            Self::Coded { details, .. } => details.as_ref(),
            _ => None,
        }
    }

    /// Whether a new root may succeed: `superseded` and `upgrade_conflict`.
    pub fn retryable(&self) -> bool {
        self.code().is_some_and(ChordErrorCode::retryable)
    }

    /// The remote error code, when this is a remote service error.
    pub fn remote_code(&self) -> Option<RemoteServiceErrorCode> {
        match self {
            Self::Remote(error) => Some(error.code),
            _ => None,
        }
    }

    /// Collapse collected failures: none is `Ok`, one is itself, more are an aggregate.
    pub fn collect(errors: Vec<ChordError>, message: &str) -> Result<(), ChordError> {
        let mut errors = errors;
        match errors.len() {
            0 => Ok(()),
            1 => Err(errors.remove(0)),
            _ => Err(Self::Aggregate { message: message.to_string(), errors }),
        }
    }

    /// An aggregate of a primary failure followed by cleanup failures.
    pub fn with_cleanup(error: ChordError, cleanup: Vec<ChordError>, message: &str) -> ChordError {
        if cleanup.is_empty() {
            return error;
        }
        let mut errors = vec![error];
        errors.extend(cleanup);
        Self::Aggregate { message: message.to_string(), errors }
    }
}

pub type ChordResult<T> = Result<T, ChordError>;

/// A process-wide handler for failures with no other reporter.
pub type ErrorHook = Arc<dyn Fn(&ChordError) + Send + Sync>;

static UNHANDLED: RwLock<Option<ErrorHook>> = RwLock::new(None);

/// Install the process-wide handler for failures with no other reporter. The default writes to stderr.
pub fn set_unhandled_error_hook(hook: Option<ErrorHook>) {
    *UNHANDLED.write().unwrap_or_else(|poison| poison.into_inner()) = hook;
}

/// Report a failure that has no owner, like an uncaught exception in upstream's microtask.
pub fn report_unhandled(error: &ChordError) {
    let hook = UNHANDLED.read().unwrap_or_else(|poison| poison.into_inner()).clone();
    match hook {
        Some(hook) => hook(error),
        None => eprintln!("pi-chord: unhandled error: {error}"),
    }
}
