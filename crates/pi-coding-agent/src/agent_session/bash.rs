//! User shell commands (`!cmd`, `!!cmd`, RPC `bash`), ported from upstream `bash-executor.ts`:
//! streamed, sanitized output with a rolling buffer, tail truncation, and a temp file for
//! the full output. Results become `bashExecution` messages.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::LazyLock;
use std::time::Duration;

use regex::Regex;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::output_file::OutputFile;
use super::{AgentSession, AgentSessionEvent, lock};
use crate::messages::{BashExecutionMessage, CustomRole};
use crate::tools::truncate::{DEFAULT_MAX_BYTES, TruncationOptions, truncate_tail};

/// Outcome of one command.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BashResult {
    /// Combined stdout and stderr, sanitized and possibly truncated to the tail.
    pub output: String,
    /// `None` when the command was cancelled.
    pub exit_code: Option<i32>,
    pub cancelled: bool,
    pub truncated: bool,
    /// File holding the full output when it was truncated.
    pub full_output_path: Option<PathBuf>,
}

const SESSION_VARIABLES: [&str; 5] =
    ["PI_SESSION_ID", "PI_SESSION_FILE", "PI_PROVIDER", "PI_MODEL", "PI_REASONING_LEVEL"];
const MAX_PENDING_ANSI_CHARS: usize = 256;
/// How long to keep reading after the shell exits while descendants still hold the pipes.
const POST_EXIT_IDLE: Duration = Duration::from_millis(250);

static ANSI: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?s)\x1b\].*?(?:\x07|\x1b\\|\u{9c})|[\x1b\u{9b}][\[\]()#;?]*(?:\d{1,4}(?:[;:]\d{0,4})*)?[\dA-PR-TZcf-nq-uy=><~]",
    )
    .expect("valid ANSI pattern")
});

/// Remove ANSI escape sequences (upstream `stripAnsi`).
pub fn strip_ansi(text: &str) -> String {
    if !text.contains(['\x1b', '\u{9b}']) {
        return text.to_string();
    }
    ANSI.replace_all(text, "").into_owned()
}

/// Remove control characters other than tab and newlines, and Unicode interlinear annotations.
pub fn sanitize_binary_output(text: &str) -> String {
    text.chars()
        .filter(|c| !matches!(c, '\x00'..='\x08' | '\x0b' | '\x0c' | '\x0e'..='\x1f' | '\u{fff9}'..='\u{fffb}'))
        .collect()
}

fn is_unfinished_csi(rest: &str) -> bool {
    let rest = rest.trim_start_matches(['[', ']', '(', ')', '#', ';', '?']);
    let digits = rest.chars().take_while(char::is_ascii_digit).count();
    if digits == 0 {
        return rest.is_empty();
    }
    if digits > 4 {
        return false;
    }
    let mut rest = &rest[digits..];
    while let Some(after) = rest.strip_prefix([';', ':']) {
        let more = after.chars().take_while(char::is_ascii_digit).count();
        if more > 4 {
            return false;
        }
        rest = &after[more..];
    }
    rest.is_empty()
}

fn is_unfinished_osc(rest: &str) -> bool {
    let mut chars = rest.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x07' | '\u{9c}' => return false,
            '\x1b' if chars.peek() == Some(&'\\') => return false,
            _ => {}
        }
    }
    true
}

/// Split streamed text into a part safe to strip now and an unfinished escape sequence at
/// the end, which is prepended to the next chunk (upstream `splitIncompleteAnsiSuffix`).
pub fn split_incomplete_ansi_suffix(text: &str) -> (&str, &str) {
    if !text.contains(['\x1b', '\u{9b}']) {
        return (text, "");
    }
    let total = text.chars().count();
    let window_start = text.char_indices().nth(total.saturating_sub(MAX_PENDING_ANSI_CHARS)).map_or(0, |(i, _)| i);
    for (offset, c) in text[window_start..].char_indices() {
        let at = window_start + offset;
        let unfinished = match c {
            '\x1b' if text[at + 1..].starts_with(']') => is_unfinished_osc(&text[at + 2..]),
            '\x1b' | '\u{9b}' => is_unfinished_csi(&text[at + c.len_utf8()..]),
            _ => false,
        };
        if unfinished {
            return (&text[..at], &text[at..]);
        }
    }
    (text, "")
}

fn spawn_reader(mut stream: impl AsyncRead + Unpin + Send + 'static, sender: mpsc::UnboundedSender<Vec<u8>>) {
    tokio::spawn(async move {
        let mut buffer = vec![0u8; 8192];
        while let Ok(read) = stream.read(&mut buffer).await {
            if read == 0 || sender.send(buffer[..read].to_vec()).is_err() {
                break;
            }
        }
    });
}

async fn kill_process_tree(child: &mut tokio::process::Child) {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        let _ = tokio::process::Command::new("/bin/sh")
            .args(["-c", &format!("kill -KILL -- -{pid}")])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await;
    }
    let _ = child.start_kill();
}

/// Rolling output of one command (upstream `executeBashWithOperations` closures).
struct Collector<'a> {
    chunks: std::collections::VecDeque<String>,
    buffered: usize,
    total_bytes: usize,
    file: Option<OutputFile>,
    on_chunk: &'a mut (dyn FnMut(&str) + Send),
}

impl Collector<'_> {
    fn append(&mut self, raw: &str) {
        let text = sanitize_binary_output(&strip_ansi(raw)).replace('\r', "");
        if text.is_empty() {
            return;
        }
        if self.total_bytes > DEFAULT_MAX_BYTES {
            self.ensure_file();
        }
        if let Some(file) = &mut self.file {
            file.write(&text);
        }
        self.buffered += text.len();
        self.chunks.push_back(text.clone());
        while self.buffered > DEFAULT_MAX_BYTES * 2 && self.chunks.len() > 1 {
            if let Some(removed) = self.chunks.pop_front() {
                self.buffered -= removed.len();
            }
        }
        (self.on_chunk)(&text);
    }

    fn ensure_file(&mut self) {
        if self.file.is_none()
            && let Some(mut file) = OutputFile::create("pi-bash", ".log")
        {
            self.chunks.iter().for_each(|chunk| file.write(chunk));
            self.file = Some(file);
        }
    }
}

/// Run `command` with the shell in `cwd`, streaming sanitized chunks to `on_chunk`. A
/// cancelled command returns the output it produced so far.
pub async fn execute_bash_command(
    command: &str,
    cwd: &Path,
    shell_path: Option<&Path>,
    cancel: &CancellationToken,
    on_chunk: &mut (dyn FnMut(&str) + Send),
) -> anyhow::Result<BashResult> {
    let config = crate::tools::bash_shell_config(shell_path)?;
    let from_stdin = config.transport == crate::tools::CommandTransport::Stdin;
    let mut process = tokio::process::Command::new(&config.shell);
    for name in SESSION_VARIABLES {
        process.env_remove(name);
    }
    process.args(&config.args);
    if !from_stdin {
        process.arg(command);
    }
    process
        .current_dir(cwd)
        .envs(pi_ai::env::child_process_env())
        .stdin(if from_stdin { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    process.process_group(0);
    let mut child = process.spawn().map_err(|error| anyhow::anyhow!("Failed to start shell: {error}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        let script = command.as_bytes().to_vec();
        tokio::spawn(async move {
            let _ = stdin.write_all(&script).await;
            let _ = stdin.shutdown().await;
        });
    }
    let (sender, mut receiver) = mpsc::unbounded_channel();
    if let Some(stdout) = child.stdout.take() {
        spawn_reader(stdout, sender.clone());
    }
    if let Some(stderr) = child.stderr.take() {
        spawn_reader(stderr, sender);
    }
    let mut collector = Collector { chunks: Default::default(), buffered: 0, total_bytes: 0, file: None, on_chunk };
    let mut pending_bytes: Vec<u8> = Vec::new();
    let mut pending_ansi = String::new();
    let mut feed = |collector: &mut Collector<'_>, data: &[u8], last: bool| {
        collector.total_bytes += data.len();
        pending_bytes.extend_from_slice(data);
        let valid = match std::str::from_utf8(&pending_bytes) {
            Ok(_) => pending_bytes.len(),
            Err(error) if error.error_len().is_none() && !last => error.valid_up_to(),
            Err(_) => pending_bytes.len(),
        };
        let decoded = String::from_utf8_lossy(&pending_bytes[..valid]).into_owned();
        pending_bytes.drain(..valid);
        let joined = std::mem::take(&mut pending_ansi) + &decoded;
        let (complete, pending) = if last { (joined.as_str(), "") } else { split_incomplete_ansi_suffix(&joined) };
        pending_ansi = pending.to_string();
        collector.append(complete);
    };
    let mut exit_code = None;
    let mut status_done = false;
    let mut streams_done = false;
    let mut cancelled = false;
    let far = tokio::time::Instant::now() + Duration::from_secs(86_400 * 365);
    let mut idle_deadline = far;
    while !(status_done && streams_done) {
        tokio::select! {
            _ = cancel.cancelled(), if !cancelled => {
                cancelled = true;
                kill_process_tree(&mut child).await;
            }
            chunk = receiver.recv(), if !streams_done => match chunk {
                Some(data) => {
                    feed(&mut collector, &data, false);
                    if status_done {
                        idle_deadline = tokio::time::Instant::now() + POST_EXIT_IDLE;
                    }
                }
                None => streams_done = true,
            },
            status = child.wait(), if !status_done => {
                exit_code = status.ok().and_then(|status| status.code());
                status_done = true;
                idle_deadline = tokio::time::Instant::now() + POST_EXIT_IDLE;
            }
            _ = tokio::time::sleep_until(idle_deadline), if status_done => streams_done = true,
        }
    }
    feed(&mut collector, &[], true);
    let full: String = collector.chunks.iter().map(String::as_str).collect();
    let truncation = truncate_tail(&full, TruncationOptions::default());
    if truncation.truncated {
        collector.ensure_file();
    }
    let full_output_path = collector.file.take().map(OutputFile::finish);
    Ok(BashResult {
        output: if truncation.truncated { truncation.content } else { full },
        exit_code: if cancelled { None } else { exit_code },
        cancelled,
        truncated: truncation.truncated,
        full_output_path,
    })
}

impl AgentSession {
    /// Run a user shell command (`!cmd`; `exclude_from_context` for `!!cmd`) and record it.
    /// Each chunk goes to `on_chunk` and out as `bash_execution_update` with `id`.
    pub async fn execute_bash(
        &self,
        command: &str,
        exclude_from_context: bool,
        id: Option<&str>,
        on_chunk: &mut (dyn FnMut(&str) + Send),
    ) -> anyhow::Result<BashResult> {
        let cancel = CancellationToken::new();
        let key = {
            let mut state = lock(&self.state);
            state.next_bash += 1;
            let key = state.next_bash;
            state.bash.push((key, cancel.clone()));
            key
        };
        let prefix = self.settings().shell_command_prefix().filter(|prefix| !prefix.is_empty());
        let resolved = match prefix {
            Some(prefix) => format!("{prefix}\n{command}"),
            None => command.to_string(),
        };
        let cwd = self.session_manager().cwd().to_path_buf();
        let shell = self.settings().shell_path();
        let mut forward = |delta: &str| {
            on_chunk(delta);
            self.emit(&AgentSessionEvent::BashExecutionUpdate { id: id.map(String::from), delta: delta.to_string() });
        };
        let result = execute_bash_command(&resolved, &cwd, shell.as_deref(), &cancel, &mut forward).await;
        lock(&self.state).bash.retain(|(running, _)| *running != key);
        let result = result?;
        self.record_bash_result(command, &result, exclude_from_context);
        Ok(result)
    }

    /// Add a command result to the session. During a run it waits until the run ends, so it
    /// never lands between a tool call and its result.
    pub fn record_bash_result(&self, command: &str, result: &BashResult, exclude_from_context: bool) {
        let message = BashExecutionMessage {
            command: command.to_string(),
            output: result.output.clone(),
            exit_code: result.exit_code.map(i64::from),
            cancelled: result.cancelled,
            truncated: result.truncated,
            full_output_path: result.full_output_path.as_ref().map(|path| path.display().to_string()),
            timestamp: pi_ai::now_ms(),
            exclude_from_context: exclude_from_context.then_some(true),
        }
        .into_message();
        if self.is_streaming() {
            lock(&self.state).pending_bash.push(message);
            return;
        }
        super::listener::report_save_error(self.session_manager().append_message(&message));
        self.refresh_context();
    }

    /// Cancel running shell commands.
    pub fn abort_bash(&self) {
        for (_, token) in &lock(&self.state).bash {
            token.cancel();
        }
    }

    pub fn is_bash_running(&self) -> bool {
        !lock(&self.state).bash.is_empty()
    }

    pub fn has_pending_bash_messages(&self) -> bool {
        !lock(&self.state).pending_bash.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_and_splits_ansi_sequences() {
        assert_eq!(strip_ansi("\x1b[31mred\x1b[0m \x1b]0;title\x07done"), "red done");
        assert_eq!(split_incomplete_ansi_suffix("ok\x1b[3"), ("ok", "\x1b[3"));
        assert_eq!(split_incomplete_ansi_suffix("ok\x1b]8;;http://x"), ("ok", "\x1b]8;;http://x"));
        assert_eq!(split_incomplete_ansi_suffix("ok\x1b[31m"), ("ok\x1b[31m", ""));
        assert_eq!(split_incomplete_ansi_suffix("ok\x1b"), ("ok", "\x1b"));
        assert_eq!(sanitize_binary_output("a\x00b\tc\x07d\n"), "ab\tcd\n");
    }

    #[tokio::test]
    async fn runs_commands_streams_output_and_reports_exit_codes() {
        let dir = tempfile::tempdir().unwrap();
        let mut chunks = String::new();
        let mut collect = |chunk: &str| chunks.push_str(chunk);
        let cancel = CancellationToken::new();
        let result =
            execute_bash_command("printf 'a\\r\\n\\033[1mb\\033[0m'; exit 3", dir.path(), None, &cancel, &mut collect)
                .await
                .unwrap();
        assert_eq!(result.output, "a\nb");
        assert_eq!(result.exit_code, Some(3));
        assert!(!result.cancelled && !result.truncated);
        assert_eq!(chunks, "a\nb");
    }

    #[tokio::test]
    async fn truncates_long_output_and_keeps_the_full_text() {
        let dir = tempfile::tempdir().unwrap();
        let cancel = CancellationToken::new();
        let result = execute_bash_command("seq 1 30000", dir.path(), None, &cancel, &mut |_: &str| {}).await.unwrap();
        assert!(result.truncated);
        assert!(result.output.trim_end().ends_with("\n30000"));
        let full = std::fs::read_to_string(result.full_output_path.unwrap()).unwrap();
        assert!(full.starts_with("1\n2\n") && full.ends_with("30000\n"));
    }

    #[tokio::test]
    async fn cancellation_stops_the_command() {
        let dir = tempfile::tempdir().unwrap();
        let cancel = CancellationToken::new();
        let trigger = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            trigger.cancel();
        });
        let result =
            execute_bash_command("echo start; sleep 30", dir.path(), None, &cancel, &mut |_: &str| {}).await.unwrap();
        assert!(result.cancelled);
        assert_eq!(result.exit_code, None);
        assert_eq!(result.output, "start\n");
    }
}
