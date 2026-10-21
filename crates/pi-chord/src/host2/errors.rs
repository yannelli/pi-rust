//! Port of upstream `errors.ts`: the error codes callers see, and helpers that build coded errors.
//! A coded error is [`ChordError::Coded`]; its message starts with `code/reason: ` like upstream's.

use std::fmt;
use std::future::Future;
use std::sync::Arc;

use crate::error::{ChordError, ChordResult};
use crate::json::Json;

/// Error codes callers see (guide table), in upstream order. A refused or aborted reload keeps the
/// code of its cause.
pub const CHORD_ERROR_CODES: [&str; 12] = [
    "aborted",
    "contract_mismatch",
    "forbidden",
    "gone",
    "invalid",
    "not_in_variant",
    "not_ready",
    "released",
    "superseded",
    "unavailable",
    "upgrade_conflict",
    "upgrade_required",
];

/// One of [`CHORD_ERROR_CODES`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ChordErrorCode {
    Aborted,
    ContractMismatch,
    Forbidden,
    Gone,
    Invalid,
    NotInVariant,
    NotReady,
    Released,
    Superseded,
    Unavailable,
    UpgradeConflict,
    UpgradeRequired,
}

impl ChordErrorCode {
    pub const ALL: [Self; 12] = [
        Self::Aborted,
        Self::ContractMismatch,
        Self::Forbidden,
        Self::Gone,
        Self::Invalid,
        Self::NotInVariant,
        Self::NotReady,
        Self::Released,
        Self::Superseded,
        Self::Unavailable,
        Self::UpgradeConflict,
        Self::UpgradeRequired,
    ];

    /// The wire spelling, for example `contract_mismatch`.
    pub fn as_str(self) -> &'static str {
        CHORD_ERROR_CODES[self as usize]
    }

    pub fn parse(value: &str) -> Option<Self> {
        CHORD_ERROR_CODES.iter().position(|code| *code == value).map(|index| Self::ALL[index])
    }

    /// Whether a new root may succeed where this failed.
    pub fn retryable(self) -> bool {
        matches!(self, Self::Superseded | Self::UpgradeConflict)
    }
}

impl fmt::Display for ChordErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The optional parts of a coded error (upstream's constructor options).
#[derive(Clone, Debug, Default)]
pub struct ErrorOptions {
    pub cause: Option<ChordError>,
    pub details: Option<Json>,
    /// Finer cause inside a code, for logs and tests: `invalid` + `args`, `unavailable` + `unknown_type`.
    pub reason: Option<String>,
}

impl ErrorOptions {
    pub fn reason(reason: impl Into<String>) -> Self {
        Self { reason: Some(reason.into()), ..Self::default() }
    }

    pub fn with_cause(mut self, cause: ChordError) -> Self {
        self.cause = Some(cause);
        self
    }

    pub fn with_details(mut self, details: Json) -> Self {
        self.details = Some(details);
        self
    }
}

impl ChordError {
    /// A coded error; the message becomes `code/reason: message` (`code: message` without a reason).
    pub fn coded(code: ChordErrorCode, message: impl AsRef<str>, options: ErrorOptions) -> Self {
        let reason = options.reason.map(Arc::<str>::from);
        let message = match reason.as_deref().filter(|reason| !reason.is_empty()) {
            Some(reason) => format!("{code}/{reason}: {}", message.as_ref()),
            None => format!("{code}: {}", message.as_ref()),
        };
        let cause = options.cause.map(Arc::new);
        Self::Coded { code, reason, message: message.into(), details: options.details, cause }
    }

    /// A coded error without reason, details or cause.
    pub fn code_only(code: ChordErrorCode, message: impl AsRef<str>) -> Self {
        Self::coded(code, message, ErrorOptions::default())
    }

    /// A coded error with a reason.
    pub fn with_reason(code: ChordErrorCode, reason: impl Into<String>, message: impl AsRef<str>) -> Self {
        Self::coded(code, message, ErrorOptions::reason(reason))
    }
}

/// Whether `error` is a coded error, of `code` when given.
pub fn is_chord_error(error: &ChordError, code: Option<ChordErrorCode>) -> bool {
    error.code().is_some_and(|actual| code.is_none_or(|code| actual == code))
}

/// For factories and methods that refuse a caller: `return Err(forbidden(None))`.
pub fn forbidden(message: Option<&str>) -> ChordError {
    ChordError::code_only(ChordErrorCode::Forbidden, message.unwrap_or("forbidden"))
}

pub fn invalid(reason: &str, message: impl AsRef<str>, details: Option<Json>) -> ChordError {
    let options = ErrorOptions { details, ..ErrorOptions::reason(reason) };
    ChordError::coded(ChordErrorCode::Invalid, message, options)
}

/// Keeps a coded error; anything else becomes `unavailable` with the original as cause.
pub fn as_chord_error(error: ChordError, context: &str) -> ChordError {
    if error.code().is_some() {
        return error;
    }
    let message = format!("{context}: {error}");
    ChordError::coded(ChordErrorCode::Unavailable, message, ErrorOptions::default().with_cause(error))
}

/// Runs every step in order, then fails with the one error, or an aggregate when several failed.
pub async fn run_all<I, F>(steps: I, message: &str) -> ChordResult<()>
where
    I: IntoIterator<Item = F>,
    F: Future<Output = ChordResult<()>>,
{
    let mut errors = Vec::new();
    for step in steps {
        if let Err(error) = step.await {
            errors.push(error);
        }
    }
    ChordError::collect(errors, message)
}
