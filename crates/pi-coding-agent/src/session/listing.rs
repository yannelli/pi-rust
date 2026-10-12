//! Session discovery: metadata for the resume picker, listing one project or all projects,
//! lookup by exact or partial id, rename, and delete.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;

use super::ids::{parse_iso, resolve_path, resolve_path_from};
use super::storage::{cwd_matches, header_from_raw, read_session_header_for_discovery};
use super::{SessionManager, session_dir};

/// Metadata for one session file.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionInfo {
    pub path: PathBuf,
    pub id: String,
    /// Working directory from the header; empty for old sessions.
    pub cwd: String,
    /// Display name from the latest `session_info` entry.
    pub name: Option<String>,
    /// File this session was forked from.
    pub parent_session_path: Option<String>,
    /// Header time in milliseconds.
    pub created: i64,
    /// Last user or assistant activity in milliseconds (header time, then file time, as fallback).
    pub modified: i64,
    /// The activity time as a `SystemTime`.
    pub changed: SystemTime,
    pub message_count: usize,
    /// First user message text, or `(no messages)`.
    pub first_message: String,
    /// All user and assistant text, for search.
    pub all_messages_text: String,
}

impl SessionInfo {
    /// The name shown in pickers: the session name, else the first message.
    pub fn display_name(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.first_message)
    }
}

fn message_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(" "),
        _ => String::new(),
    }
}

fn millis_of(time: SystemTime) -> i64 {
    time.duration_since(UNIX_EPOCH).map(|duration| duration.as_millis() as i64).unwrap_or_default()
}

/// Read one session file's metadata. `None` when the first parsed line is not a header.
pub fn build_session_info(path: &Path) -> Option<SessionInfo> {
    let text = std::fs::read(path).ok()?;
    let file_time = std::fs::metadata(path).and_then(|metadata| metadata.modified()).ok();
    let mut lines = text
        .split(|byte| *byte == b'\n')
        .filter_map(|line| serde_json::from_slice::<Value>(line).ok())
        .filter_map(|value| match value {
            Value::Object(object) => Some(object),
            _ => None,
        });
    let header = header_from_raw(&lines.next()?)?;
    let (mut message_count, mut first_message, mut name, mut last_activity) = (0, String::new(), None, None::<i64>);
    let mut all_messages = Vec::new();
    for entry in lines {
        let kind = entry.get("type").and_then(Value::as_str).unwrap_or_default();
        if kind == "session_info" {
            let trimmed = entry.get("name").and_then(Value::as_str).map(str::trim);
            name = trimmed.filter(|name| !name.is_empty()).map(String::from);
        }
        if kind != "message" {
            continue;
        }
        message_count += 1;
        let Some(message) = entry.get("message").and_then(Value::as_object) else { continue };
        let role = message.get("role").and_then(Value::as_str).unwrap_or_default();
        if !matches!(role, "user" | "assistant") || !message.contains_key("content") {
            continue;
        }
        let activity = message
            .get("timestamp")
            .and_then(Value::as_f64)
            .map(|time| time as i64)
            .or_else(|| entry.get("timestamp").and_then(Value::as_str).and_then(parse_iso));
        if let Some(activity) = activity {
            last_activity = Some(last_activity.unwrap_or(0).max(activity));
        }
        let text = message_text(&message["content"]);
        if text.is_empty() {
            continue;
        }
        if first_message.is_empty() && role == "user" {
            first_message = text.clone();
        }
        all_messages.push(text);
    }
    let created = parse_iso(&header.timestamp);
    let activity = last_activity.filter(|time| *time > 0).or(created).or_else(|| file_time.map(millis_of));
    let activity = activity.unwrap_or_default();
    Some(SessionInfo {
        path: path.to_path_buf(),
        id: header.id,
        cwd: header.cwd,
        name,
        parent_session_path: header.parent_session,
        created: created.unwrap_or_default(),
        modified: activity,
        changed: UNIX_EPOCH + Duration::from_millis(activity.max(0) as u64),
        message_count,
        first_message: if first_message.is_empty() { "(no messages)".into() } else { first_message },
        all_messages_text: all_messages.join(" "),
    })
}

fn jsonl_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(read_dir) = std::fs::read_dir(dir) else { return Vec::new() };
    read_dir
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "jsonl"))
        .collect()
}

/// Newest activity first; ties keep their order.
pub fn sort_session_infos(sessions: &mut [SessionInfo]) {
    sessions.sort_by_key(|session| std::cmp::Reverse(session.modified));
}

/// Every session file in `dir`, newest activity first.
pub fn list_sessions(dir: &Path) -> Vec<SessionInfo> {
    let mut files = jsonl_files(dir);
    files.sort_by(|a, b| b.file_name().cmp(&a.file_name()));
    let mut sessions: Vec<SessionInfo> = files.iter().filter_map(|path| build_session_info(path)).collect();
    sort_session_infos(&mut sessions);
    sessions
}

/// Sessions for one project: its default directory under `agent_dir`, or `session_dir`
/// (filtered by cwd when that is a shared flat directory).
pub fn list_project_sessions(cwd: &Path, session_dir_override: Option<&Path>, agent_dir: &Path) -> Vec<SessionInfo> {
    let default_dir = session_dir(agent_dir, cwd);
    let dir = session_dir_override.map(resolve_path).unwrap_or_else(|| default_dir.clone());
    let resolved_cwd = resolve_path(cwd);
    let filter = dir != default_dir;
    list_sessions(&dir).into_iter().filter(|info| !filter || cwd_matches(&info.cwd, &resolved_cwd)).collect()
}

/// Sessions of every project under `<agent_dir>/sessions`, or every session in `custom_dir`.
pub fn list_all_sessions(custom_dir: Option<&Path>, agent_dir: &Path) -> Vec<SessionInfo> {
    if let Some(dir) = custom_dir {
        return list_sessions(&resolve_path(dir));
    }
    let root = resolve_path(agent_dir).join("sessions");
    let Ok(read_dir) = std::fs::read_dir(&root) else { return Vec::new() };
    let mut files: Vec<(PathBuf, Option<SystemTime>)> = read_dir
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .flat_map(|dir| jsonl_files(&dir))
        .map(|path| {
            let time = std::fs::metadata(&path).and_then(|metadata| metadata.modified()).ok();
            (path, time)
        })
        .collect();
    files.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| b.0.file_name().cmp(&a.0.file_name())));
    let mut sessions: Vec<SessionInfo> = files.iter().filter_map(|(path, _)| build_session_info(path)).collect();
    sort_session_infos(&mut sessions);
    sessions
}

impl SessionManager {
    /// The file of the session with exactly `id`, reading only headers.
    pub fn find_by_id(cwd: &Path, id: &str, session_dir_override: Option<&Path>, agent_dir: &Path) -> Option<PathBuf> {
        let default_dir = session_dir(agent_dir, cwd);
        let dir = session_dir_override.map(resolve_path).unwrap_or_else(|| default_dir.clone());
        let filter = dir != default_dir;
        let resolved_cwd = resolve_path(cwd);
        jsonl_files(&dir).into_iter().find(|path| {
            read_session_header_for_discovery(path)
                .is_some_and(|header| header.id == id && (!filter || cwd_matches(&header.cwd, &resolved_cwd)))
        })
    }
}

/// Where a `--session` or `--fork` argument points.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResolvedSession {
    /// The argument was a file path.
    Path(PathBuf),
    /// A session of the current project.
    Local(PathBuf),
    /// A session of another project, with its cwd.
    Global {
        path: PathBuf,
        cwd: String,
    },
    NotFound(String),
}

/// Resolve a path, or an exact or prefix session id: this project first, then all projects.
pub fn resolve_session_arg(
    arg: &str,
    cwd: &Path,
    session_dir_override: Option<&Path>,
    agent_dir: &Path,
) -> ResolvedSession {
    if arg.contains(['/', '\\']) || arg.ends_with(".jsonl") {
        return ResolvedSession::Path(resolve_path_from(Path::new(arg), cwd));
    }
    if let Some(path) = SessionManager::find_by_id(cwd, arg, session_dir_override, agent_dir) {
        return ResolvedSession::Local(path);
    }
    let local = list_project_sessions(cwd, session_dir_override, agent_dir);
    if let Some(info) = local.into_iter().find(|info| info.id.starts_with(arg)) {
        return ResolvedSession::Local(info.path);
    }
    let all = list_all_sessions(session_dir_override, agent_dir);
    let found = all.iter().find(|info| info.id == arg).or_else(|| all.iter().find(|info| info.id.starts_with(arg)));
    match found {
        Some(info) => ResolvedSession::Global { path: info.path.clone(), cwd: info.cwd.clone() },
        None => ResolvedSession::NotFound(arg.to_string()),
    }
}

/// Name a stored session. Blank names are ignored, as in the resume picker.
pub fn rename_session(path: &Path, name: &str) -> anyhow::Result<()> {
    let name = name.trim();
    if name.is_empty() {
        return Ok(());
    }
    SessionManager::open(path)?.append_session_info(name)?;
    Ok(())
}

/// How a session file was removed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeleteMethod {
    /// Moved to the trash with the `trash` command.
    Trash,
    /// Deleted for good because `trash` was unavailable or failed.
    Unlink,
}

/// Delete a session file: try the `trash` command first, then remove the file.
pub fn delete_session_file(path: &Path) -> anyhow::Result<DeleteMethod> {
    let text = path.display().to_string();
    let mut command = std::process::Command::new("trash");
    if text.starts_with('-') {
        command.arg("--");
    }
    let result = command.arg(path).stdin(std::process::Stdio::null()).output();
    if result.as_ref().is_ok_and(|output| output.status.success()) || !path.exists() {
        return Ok(DeleteMethod::Trash);
    }
    match std::fs::remove_file(path) {
        Ok(()) => Ok(DeleteMethod::Unlink),
        Err(error) => {
            let hint = match &result {
                Err(trash_error) => Some(trash_error.to_string()),
                Ok(output) => String::from_utf8_lossy(&output.stderr).lines().next().map(str::to_string),
            };
            match hint.filter(|hint| !hint.trim().is_empty()) {
                Some(hint) => anyhow::bail!("{error} (trash: {})", hint.chars().take(200).collect::<String>()),
                None => anyhow::bail!("{error}"),
            }
        }
    }
}
