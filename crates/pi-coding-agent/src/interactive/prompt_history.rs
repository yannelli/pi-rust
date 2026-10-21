//! Prompt history across sessions and projects (pi-rust extension). Submitted prompts are
//! appended to `<agent dir>/prompt-history.jsonl`, one JSON string per line, oldest first, and
//! loaded into the editor at startup for Up, Down, and the reverse search.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use pi_ai::auth::store_lock::FileLock;

use super::mode::InteractiveMode;

/// The history file name in the agent directory.
pub const PROMPT_HISTORY_FILE: &str = "prompt-history.jsonl";

const COMPACT_LOCK_STALE: Duration = Duration::from_secs(10);

/// The prompt history file of one agent directory.
#[derive(Clone, Debug)]
pub struct PromptHistoryFile {
    path: PathBuf,
    max_entries: usize,
}

impl PromptHistoryFile {
    pub fn new(agent_dir: &Path, max_entries: usize) -> Self {
        Self { path: agent_dir.join(PROMPT_HISTORY_FILE), max_entries }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn max_entries(&self) -> usize {
        self.max_entries
    }

    /// The newest `max_entries` prompts, oldest first. Malformed lines are skipped and repeated
    /// prompts in a row count once. A file with more than twice that many lines is trimmed.
    pub fn load(&self) -> Vec<String> {
        let Ok(text) = std::fs::read_to_string(&self.path) else { return Vec::new() };
        let (mut entries, lines) = parse(&text);
        if lines > self.max_entries.saturating_mul(2) {
            self.compact();
        }
        entries.drain(..entries.len().saturating_sub(self.max_entries));
        entries
    }

    /// Appends one prompt as one write, so lines from concurrent processes do not interleave.
    pub fn append(&self, text: &str) -> std::io::Result<()> {
        let text = text.trim();
        if text.is_empty() || self.max_entries == 0 {
            return Ok(());
        }
        let mut file = open_private(OpenOptions::new().read(true).append(true), &self.path)?;
        let mut line = if ends_mid_line(&mut file) { "\n".to_string() } else { String::new() };
        line.push_str(&serde_json::to_string(text)?);
        line.push('\n');
        file.write_all(line.as_bytes())
    }

    /// Rewrites the file with the newest entries. Another process that holds the lock is
    /// already doing it; an append that lands between the read and the rename is lost.
    fn compact(&self) {
        let Ok(Some(_lock)) = FileLock::try_acquire(&self.path, COMPACT_LOCK_STALE) else { return };
        let Ok(text) = std::fs::read_to_string(&self.path) else { return };
        let (entries, _) = parse(&text);
        let keep = &entries[entries.len().saturating_sub(self.max_entries)..];
        let body: String =
            keep.iter().filter_map(|entry| serde_json::to_string(entry).ok()).map(|line| line + "\n").collect();
        let temp = self.path.with_file_name(format!("{PROMPT_HISTORY_FILE}.{}.tmp", std::process::id()));
        let written = open_private(OpenOptions::new().write(true).truncate(true), &temp)
            .and_then(|mut file| file.write_all(body.as_bytes()))
            .and_then(|()| std::fs::rename(&temp, &self.path));
        if written.is_err() {
            let _ = std::fs::remove_file(&temp);
        }
    }
}

/// Opens `path` with `options`, creating it with mode 0600 on Unix.
fn open_private(options: &mut OpenOptions, path: &Path) -> std::io::Result<File> {
    options.create(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(options, 0o600);
    options.open(path)
}

/// Whether the file ends without a newline, as after an interrupted write.
fn ends_mid_line(file: &mut File) -> bool {
    let Ok(length) = file.metadata().map(|metadata| metadata.len()) else { return false };
    let mut last = [0u8];
    length > 0
        && file.seek(SeekFrom::Start(length - 1)).is_ok()
        && file.read_exact(&mut last).is_ok()
        && last[0] != b'\n'
}

/// The prompts of `text` in file order, and its line count.
fn parse(text: &str) -> (Vec<String>, usize) {
    let mut entries: Vec<String> = Vec::new();
    let mut lines = 0;
    for line in text.lines() {
        lines += 1;
        let Ok(entry) = serde_json::from_str::<String>(line) else { continue };
        let entry = entry.trim();
        if !entry.is_empty() && entries.last().map(String::as_str) != Some(entry) {
            entries.push(entry.to_string());
        }
    }
    (entries, lines)
}

impl InteractiveMode {
    /// Loads the saved prompts into the editor when `promptHistory.enabled` is on.
    pub(super) fn load_prompt_history(&self) {
        let settings = self.settings();
        let max_entries = settings.prompt_history_max_entries();
        if settings.prompt_history_enabled() && max_entries > 0 {
            self.editor.set_prompt_history(PromptHistoryFile::new(&self.agent_dir, max_entries));
        }
    }
}

#[cfg(test)]
#[path = "prompt_history_tests.rs"]
mod tests;
