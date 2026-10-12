//! JSONL session files in the TypeScript pi v3 format: a `session` header line,
//! then entries linked by `parentId`. Unknown entry types are kept and skipped.

use std::collections::HashMap;
use std::hash::{BuildHasher, Hasher};
use std::io::Write;
use std::path::{Path, PathBuf};

use pi_ai::{Message, ThinkingLevel, UserContent, UserMessage};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

pub const SESSION_VERSION: u32 = 3;
pub const COMPACTION_SUMMARY_PREFIX: &str =
    "The conversation history before this point was compacted into the following summary:\n\n<summary>\n";
pub const COMPACTION_SUMMARY_SUFFIX: &str = "\n</summary>";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionHeader {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub version: Option<u32>,
    pub id: String,
    pub timestamp: String,
    pub cwd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session: Option<String>,
}

/// One entry line. Type-specific fields live in `data`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Entry {
    #[serde(rename = "type")]
    pub kind: String,
    pub id: String,
    pub parent_id: Option<String>,
    pub timestamp: String,
    #[serde(flatten)]
    pub data: Map<String, Value>,
}

impl Entry {
    pub fn message(&self) -> Option<Message> {
        (self.kind == "message").then(|| serde_json::from_value(self.data.get("message")?.clone()).ok()).flatten()
    }
}

/// Messages, model, and thinking level rebuilt from the current branch.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SessionContext {
    pub messages: Vec<Message>,
    /// `(provider, model id)` from the latest model change.
    pub model: Option<(String, String)>,
    pub thinking_level: Option<ThinkingLevel>,
}

#[derive(Clone)]
pub struct SessionManager {
    header: SessionHeader,
    entries: Vec<Entry>,
    leaf: Option<String>,
    file: Option<PathBuf>,
    written: usize,
}

/// `<agent>/sessions/--<cwd with separators replaced>--`, as in the TypeScript pi.
pub fn session_dir(agent_dir: &Path, cwd: &Path) -> PathBuf {
    let cwd = cwd.display().to_string();
    let safe = cwd.trim_start_matches(['/', '\\']).replace(['/', '\\', ':'], "-");
    agent_dir.join("sessions").join(format!("--{safe}--"))
}

impl SessionManager {
    /// A new session. `dir` is `None` for an in-memory session.
    pub fn create(cwd: &Path, dir: Option<&Path>) -> Self {
        let timestamp = iso_timestamp();
        let id = uuid_v7();
        let file = dir.map(|dir| dir.join(format!("{}_{id}.jsonl", timestamp.replace([':', '.'], "-"))));
        let header = SessionHeader {
            kind: "session".into(),
            version: Some(SESSION_VERSION),
            id,
            timestamp,
            cwd: cwd.display().to_string(),
            parent_session: None,
        };
        Self { header, entries: Vec::new(), leaf: None, file, written: 0 }
    }

    /// Open an existing file. Malformed lines are skipped; the last entry becomes the leaf.
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|error| anyhow::anyhow!("cannot read session {}: {error}", path.display()))?;
        let mut lines = text.lines().filter(|line| !line.trim().is_empty());
        let header: SessionHeader = lines
            .next()
            .and_then(|line| serde_json::from_str(line).ok())
            .filter(|header: &SessionHeader| header.kind == "session")
            .ok_or_else(|| anyhow::anyhow!("not a pi session file: {}", path.display()))?;
        let entries: Vec<Entry> = lines.filter_map(|line| serde_json::from_str(line).ok()).collect();
        let leaf = entries.last().map(|entry| entry.id.clone());
        let written = entries.len() + 1;
        Ok(Self { header, entries, leaf, file: Some(path.to_path_buf()), written })
    }

    /// The session file in `dir` with the latest change time.
    pub fn most_recent(dir: &Path) -> Option<PathBuf> {
        list_sessions(dir).into_iter().next().map(|info| info.path)
    }

    pub fn id(&self) -> &str {
        &self.header.id
    }

    pub fn file(&self) -> Option<&Path> {
        self.file.as_deref()
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    fn append(&mut self, kind: &str, data: Value) -> anyhow::Result<String> {
        let mut id = random_hex(8);
        while self.entries.iter().any(|entry| entry.id == id) {
            id = random_hex(8);
        }
        let Value::Object(data) = data else { anyhow::bail!("entry data must be an object") };
        self.entries.push(Entry {
            kind: kind.into(),
            id: id.clone(),
            parent_id: self.leaf.clone(),
            timestamp: iso_timestamp(),
            data,
        });
        self.leaf = Some(id.clone());
        self.flush()?;
        Ok(id)
    }

    pub fn append_message(&mut self, message: &Message) -> anyhow::Result<String> {
        self.append("message", json!({ "message": message }))
    }

    pub fn append_model_change(&mut self, provider: &str, model_id: &str) -> anyhow::Result<String> {
        self.append("model_change", json!({ "provider": provider, "modelId": model_id }))
    }

    pub fn append_thinking_level_change(&mut self, level: ThinkingLevel) -> anyhow::Result<String> {
        self.append("thinking_level_change", json!({ "thinkingLevel": level }))
    }

    pub fn append_compaction(
        &mut self,
        summary: &str,
        first_kept_entry_id: &str,
        tokens_before: u64,
    ) -> anyhow::Result<String> {
        self.append(
            "compaction",
            json!({ "summary": summary, "firstKeptEntryId": first_kept_entry_id, "tokensBefore": tokens_before }),
        )
    }

    /// Write entries not yet on disk. The header is written with the first assistant
    /// message, so sessions that never got a response leave no file behind.
    fn flush(&mut self) -> anyhow::Result<()> {
        let Some(path) = &self.file else { return Ok(()) };
        if self.written == 0 {
            let has_response = self.entries.iter().any(|entry| matches!(entry.message(), Some(Message::Assistant(_))));
            if !has_response {
                return Ok(());
            }
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
        }
        let mut file = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
        let mut text = String::new();
        if self.written == 0 {
            text.push_str(&serde_json::to_string(&self.header)?);
            text.push('\n');
            self.written = 1;
        }
        for entry in &self.entries[self.written - 1..] {
            text.push_str(&serde_json::to_string(entry)?);
            text.push('\n');
        }
        file.write_all(text.as_bytes())?;
        self.written = self.entries.len() + 1;
        Ok(())
    }

    /// Entries from the root to the leaf.
    pub fn branch(&self) -> Vec<&Entry> {
        let by_id: HashMap<&str, &Entry> = self.entries.iter().map(|entry| (entry.id.as_str(), entry)).collect();
        let mut path = Vec::new();
        let mut current = self.leaf.as_deref();
        while let Some(id) = current {
            let Some(entry) = by_id.get(id) else { break };
            path.push(*entry);
            current = entry.parent_id.as_deref();
        }
        path.reverse();
        path
    }

    /// Rebuild the model context. The latest compaction replaces older entries with its summary.
    pub fn context(&self) -> SessionContext {
        let path = self.branch();
        let mut context = SessionContext::default();
        for entry in &path {
            match entry.kind.as_str() {
                "model_change" => {
                    let field = |name: &str| entry.data.get(name).and_then(Value::as_str).map(String::from);
                    if let (Some(provider), Some(model)) = (field("provider"), field("modelId")) {
                        context.model = Some((provider, model));
                    }
                }
                "thinking_level_change" => {
                    context.thinking_level =
                        entry.data.get("thinkingLevel").and_then(|level| serde_json::from_value(level.clone()).ok());
                }
                _ => {}
            }
        }
        let compaction = path.iter().rposition(|entry| entry.kind == "compaction");
        let selected: Vec<&Entry> = match compaction {
            None => path,
            Some(index) => {
                let first_kept = path[index].data.get("firstKeptEntryId").and_then(Value::as_str).unwrap_or_default();
                let kept = path[..index].iter().skip_while(|entry| entry.id != first_kept).copied();
                std::iter::once(path[index]).chain(kept).chain(path[index + 1..].iter().copied()).collect()
            }
        };
        for entry in selected {
            if entry.kind == "compaction" {
                let summary = entry.data.get("summary").and_then(Value::as_str).unwrap_or_default();
                context.messages.push(summary_message(summary));
            } else if let Some(message) = entry.message() {
                context.messages.push(message);
            }
        }
        context
    }
}

/// The user message that stands in for compacted history.
pub fn summary_message(summary: &str) -> Message {
    Message::User(UserMessage {
        content: UserContent::Text(format!("{COMPACTION_SUMMARY_PREFIX}{summary}{COMPACTION_SUMMARY_SUFFIX}")),
        timestamp: pi_ai::now_ms(),
    })
}

#[derive(Clone, Debug)]
pub struct SessionInfo {
    pub path: PathBuf,
    pub id: String,
    pub changed: std::time::SystemTime,
    /// First user message text, for listings.
    pub first_message: String,
    pub message_count: usize,
}

/// Session files in `dir`, newest first.
pub fn list_sessions(dir: &Path) -> Vec<SessionInfo> {
    let Ok(read_dir) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut sessions: Vec<SessionInfo> = read_dir
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "jsonl"))
        .filter_map(|path| {
            let manager = SessionManager::open(&path).ok()?;
            let changed = std::fs::metadata(&path).and_then(|metadata| metadata.modified()).ok()?;
            let messages = manager.context().messages;
            let first_message = messages
                .iter()
                .find_map(|message| match message {
                    Message::User(user) => Some(user.content.text()),
                    _ => None,
                })
                .unwrap_or_default();
            Some(SessionInfo {
                id: manager.id().to_string(),
                path,
                changed,
                first_message,
                message_count: messages.len(),
            })
        })
        .collect();
    sessions.sort_by(|a, b| b.changed.cmp(&a.changed).then_with(|| b.path.cmp(&a.path)));
    sessions
}

fn random_u64() -> u64 {
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    hasher.write_u128(nanos);
    hasher.finish()
}

fn random_hex(length: usize) -> String {
    let mut text = String::new();
    while text.len() < length {
        text.push_str(&format!("{:016x}", random_u64()));
    }
    text.truncate(length);
    text
}

/// A version 7 UUID: 48-bit millisecond timestamp, then random bits.
pub fn uuid_v7() -> String {
    let millis = pi_ai::now_ms() as u64 & 0xffff_ffff_ffff;
    let (high, low) = (random_u64(), random_u64());
    let time_high = (millis >> 16) as u32;
    let time_low = (millis & 0xffff) as u16;
    let version = 0x7000 | (high & 0x0fff) as u16;
    let variant = 0x8000 | ((high >> 16) & 0x3fff) as u16;
    format!("{time_high:08x}-{time_low:04x}-{version:04x}-{variant:04x}-{:012x}", low & 0xffff_ffff_ffff)
}

/// UTC time as `YYYY-MM-DDTHH:MM:SS.mmmZ`.
pub fn iso_timestamp() -> String {
    format_iso(pi_ai::now_ms())
}

pub fn format_iso(millis: i64) -> String {
    let seconds = millis.div_euclid(1000);
    let (days, second_of_day) = (seconds.div_euclid(86_400), seconds.rem_euclid(86_400));
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z - era * 146_097;
    let year_of_era = (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 { month_index + 3 } else { month_index - 9 };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        second_of_day / 3600,
        second_of_day % 3600 / 60,
        second_of_day % 60,
        millis.rem_euclid(1000)
    )
}

#[cfg(test)]
mod tests {
    use pi_ai::{AssistantMessage, ContentBlock};

    use super::*;

    fn assistant(text: &str) -> Message {
        let mut message = AssistantMessage::empty(&pi_ai::providers::faux::FauxProvider::model());
        message.content.push(ContentBlock::text(text));
        message.stop_reason = pi_ai::StopReason::Stop;
        Message::Assistant(message)
    }

    fn text(message: &Message) -> String {
        match message {
            Message::User(user) => user.content.text(),
            Message::Assistant(assistant) => assistant.text(),
            Message::ToolResult(_) => String::new(),
        }
    }

    #[test]
    fn formats_timestamps_and_ids() {
        assert_eq!(format_iso(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(format_iso(1_760_000_000_123), "2025-10-09T08:53:20.123Z");
        let id = uuid_v7();
        assert_eq!(id.len(), 36);
        assert_eq!(&id[14..15], "7");
        assert_ne!(uuid_v7(), id);
    }

    #[test]
    fn writes_lazily_and_round_trips_with_compaction() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = SessionManager::create(Path::new("/work"), Some(dir.path()));
        session.append_model_change("faux", "faux-model").unwrap();
        session.append_message(&Message::user("old question")).unwrap();
        let path = session.file().unwrap().to_path_buf();
        assert!(!path.exists(), "file is written after the first response");
        session.append_message(&assistant("old answer")).unwrap();
        assert!(path.exists());
        let kept = session.append_message(&Message::user("recent question")).unwrap();
        session.append_message(&assistant("recent answer")).unwrap();
        session.append_thinking_level_change(ThinkingLevel::High).unwrap();
        session.append_compaction("summary text", &kept, 1234).unwrap();
        session.append_message(&Message::user("after")).unwrap();

        let reopened = SessionManager::open(&path).unwrap();
        assert_eq!(reopened.entries().len(), session.entries().len());
        let context = reopened.context();
        assert_eq!(context.model, Some(("faux".into(), "faux-model".into())));
        assert_eq!(context.thinking_level, Some(ThinkingLevel::High));
        let texts: Vec<String> = context.messages.iter().map(text).collect();
        assert!(texts[0].starts_with(COMPACTION_SUMMARY_PREFIX) && texts[0].contains("summary text"));
        assert_eq!(&texts[1..], ["recent question", "recent answer", "after"]);
        let header: Value =
            serde_json::from_str(std::fs::read_to_string(&path).unwrap().lines().next().unwrap()).unwrap();
        assert_eq!(header["type"], "session");
        assert_eq!(header["version"], 3);
        let sessions = list_sessions(dir.path());
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].first_message, texts[0]);
    }

    #[test]
    fn session_dir_encodes_cwd() {
        assert_eq!(session_dir(Path::new("/a"), Path::new("/home/u/repo")), Path::new("/a/sessions/--home-u-repo--"));
    }
}
