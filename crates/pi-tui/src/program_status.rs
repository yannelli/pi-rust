//! Program Status Protocol (OSC 7501): a program tells the terminal whether it is idle, working,
//! blocked on the user, done, or failed. Only the root record is supported.
//! Spec: <https://www.superlogical.com/rex/docs/build/program-status>

use std::sync::LazyLock;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use regex::Regex;

/// The state a program reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProgramState {
    /// Waiting for input with nothing to do.
    Idle,
    /// Busy.
    Working,
    /// Waiting on the user; see [`BlockedKind`].
    Blocked,
    /// Finished.
    Done,
    /// Failed.
    Error,
    /// Removes the status instead of reporting one.
    Clear,
}

impl ProgramState {
    /// The protocol name of the state.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Working => "working",
            Self::Blocked => "blocked",
            Self::Done => "done",
            Self::Error => "error",
            Self::Clear => "clear",
        }
    }
}

/// What a blocked program waits for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BlockedKind {
    /// A permission prompt.
    Permission,
    /// A question to the user.
    Question,
    /// An authentication step.
    Auth,
}

impl BlockedKind {
    /// The protocol name of the kind.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Permission => "permission",
            Self::Question => "question",
            Self::Auth => "auth",
        }
    }
}

/// A status report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgramStatus {
    /// The reported state. [`ProgramState::Clear`] removes the status.
    pub state: ProgramState,
    /// Stable program name, `[A-Za-z0-9_.+-]{1,32}`. Other values are omitted.
    pub app: Option<String>,
    /// What a blocked program waits for. Omitted for other states.
    pub kind: Option<BlockedKind>,
    /// One human-readable line. Control characters become spaces; longer text is cut to the spec limit.
    pub message: Option<String>,
}

impl ProgramStatus {
    /// A status with only a state.
    pub fn new(state: ProgramState) -> Self {
        Self { state, app: None, kind: None, message: None }
    }
}

/// Feature detection query. A supporting terminal replies with the same body.
pub const PROGRAM_STATUS_QUERY: &str = "\x1b]7501;?\x1b\\";

static REPLY_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\x1b\]7501;\?[^\x07\x1b]*(?:\x07|\x1b\\)$").expect("valid reply pattern"));
static APP_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Za-z0-9_.+-]{1,32}$").expect("valid app pattern"));
static CONTROL_CHARACTERS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[\x00-\x1f\x7f-\u{9f}]+").expect("valid control pattern"));

/// Decoded `msg` limit. Its base64 encoding stays under the 2732-byte encoded limit.
const MAX_MESSAGE_BYTES: usize = 2048;

/// Whether `sequence` is the reply to [`PROGRAM_STATUS_QUERY`]. Later spec revisions may add pairs after the `?`.
pub fn is_program_status_reply(sequence: &str) -> bool {
    REPLY_PATTERN.is_match(sequence)
}

fn truncate_utf8(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut end = 0;
    for (index, ch) in text.char_indices() {
        if index + ch.len_utf8() > max_bytes {
            break;
        }
        end = index + ch.len_utf8();
    }
    &text[..end]
}

/// Encodes a status report. Terminals discard reports whose text contains control characters, so they are replaced.
pub fn format_program_status(status: &ProgramStatus) -> String {
    let mut pairs = vec![format!("state={}", status.state.as_str())];
    if let Some(app) = status.app.as_deref().filter(|app| APP_PATTERN.is_match(app)) {
        pairs.push(format!("app={app}"));
    }
    if let (ProgramState::Blocked, Some(kind)) = (status.state, status.kind) {
        pairs.push(format!("kind={}", kind.as_str()));
    }
    let cleaned = CONTROL_CHARACTERS.replace_all(status.message.as_deref().unwrap_or(""), " ");
    let message = truncate_utf8(cleaned.trim(), MAX_MESSAGE_BYTES);
    if !message.is_empty() {
        pairs.push(format!("msg={}", STANDARD.encode(message)));
    }
    format!("\x1b]7501;{}\x1b\\", pairs.join(":"))
}
