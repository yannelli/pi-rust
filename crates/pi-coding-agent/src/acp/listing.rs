//! pi session files for `session/list` and `session/load` (upstream `pi-sessions.ts`): the
//! header gives id and cwd, the latest `session_info` name or the first user message gives
//! the title, and the latest message gives `updatedAt`.

use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::translate::title_from_texts;
use crate::session::ids::{format_iso, parse_iso};

const TAIL_BYTES: u64 = 256 * 1024;
const HEAD_BYTES: usize = 64 * 1024;
const TITLE_SCAN_LINES: usize = 2000;

/// One session file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PiSessionListItem {
    pub session_id: String,
    pub cwd: String,
    pub title: Option<String>,
    pub updated_at: Option<String>,
    pub session_file: PathBuf,
}

fn walk_jsonl(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(kind) = entry.file_type() else { continue };
        if kind.is_dir() {
            walk_jsonl(&path, out);
        } else if kind.is_file() && path.extension().is_some_and(|ext| ext == "jsonl") {
            out.push(path);
        }
    }
}

fn first_line(path: &Path) -> Option<String> {
    let mut buffer = vec![0u8; HEAD_BYTES];
    let read = std::fs::File::open(path).ok()?.read(&mut buffer).ok()?;
    if read == 0 {
        return None;
    }
    let text = String::from_utf8_lossy(&buffer[..read]).to_string();
    Some(text.split('\n').next().unwrap_or_default().trim().to_string())
}

fn read_tail(path: &Path) -> std::io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let size = file.metadata()?.len();
    file.seek(SeekFrom::Start(size.saturating_sub(TAIL_BYTES)))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(String::from_utf8_lossy(&bytes).to_string())
}

fn parse_header(line: &str) -> Option<(String, String)> {
    let header: Value = serde_json::from_str(line).ok()?;
    if header["type"] != "session" {
        return None;
    }
    Some((header["id"].as_str()?.to_string(), header["cwd"].as_str()?.to_string()))
}

fn records(text: &str) -> impl DoubleEndedIterator<Item = Value> + '_ {
    text.lines().map(str::trim).filter(|line| !line.is_empty()).filter_map(|line| serde_json::from_str(line).ok())
}

fn session_info_name(record: &Value) -> Option<String> {
    (record["type"] == "session_info").then(|| record["name"].as_str().map(|name| name.trim().to_string())).flatten()
}

fn title_from_tail(tail: &str) -> Option<String> {
    records(tail).rev().find_map(|record| session_info_name(&record))
}

/// The last `session_info` name of the whole file, for names older than the tail window.
fn scan_session_info_name(path: &Path) -> Option<String> {
    let reader = BufReader::new(std::fs::File::open(path).ok()?);
    let mut last = None;
    for line in reader.split(b'\n').map_while(Result::ok) {
        let text = String::from_utf8_lossy(&line);
        if let Ok(record) = serde_json::from_str::<Value>(text.trim())
            && let Some(name) = session_info_name(&record)
        {
            last = Some(name);
        }
    }
    last
}

fn normalized_timestamp(record: &Value) -> Option<String> {
    record["timestamp"].as_str().and_then(parse_iso).map(format_iso)
}

/// The latest message timestamp, else any latest timestamp (pi's `/resume` order).
fn updated_at_from_tail(tail: &str) -> Option<String> {
    let parsed: Vec<Value> = records(tail).collect();
    parsed
        .iter()
        .rev()
        .filter(|record| record["type"] == "message")
        .find_map(normalized_timestamp)
        .or_else(|| parsed.iter().rev().find_map(normalized_timestamp))
}

/// The first user message as a title.
fn title_from_head(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    for line in text.lines().take(TITLE_SCAN_LINES) {
        let Ok(record) = serde_json::from_str::<Value>(line.trim()) else { continue };
        if record["type"] != "message" || record["message"]["role"] != "user" {
            continue;
        }
        let content = &record["message"]["content"];
        let texts: Vec<&str> = match content {
            Value::String(text) => vec![text.as_str()],
            Value::Array(blocks) => blocks
                .iter()
                .filter(|block| block["type"] == "text")
                .filter_map(|block| block["text"].as_str())
                .collect(),
            _ => Vec::new(),
        };
        if let Some(title) = title_from_texts(&texts) {
            return Some(title);
        }
    }
    None
}

/// The display title of a session file: its name, else its first user message.
pub fn read_session_title(path: &Path, tail: Option<&str>) -> Option<String> {
    let owned;
    let tail = match tail {
        Some(tail) => tail,
        None => {
            owned = read_tail(path).ok()?;
            &owned
        }
    };
    let name = title_from_tail(tail).or_else(|| scan_session_info_name(path));
    name.filter(|name| !name.is_empty()).or_else(|| title_from_head(path))
}

fn mtime_iso(path: &Path) -> Option<String> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    let millis = modified.duration_since(std::time::UNIX_EPOCH).ok()?.as_millis();
    Some(format_iso(millis as i64))
}

/// Every session under `root`, most recently updated first.
pub fn list_pi_sessions(root: &Path) -> Vec<PiSessionListItem> {
    let mut files = Vec::new();
    walk_jsonl(root, &mut files);
    let mut items: Vec<PiSessionListItem> = files
        .into_iter()
        .filter_map(|file| {
            let (session_id, cwd) = parse_header(&first_line(&file)?)?;
            let (title, updated_at) = match read_tail(&file) {
                Ok(tail) => (read_session_title(&file, Some(&tail)), updated_at_from_tail(&tail)),
                Err(_) => (None, None),
            };
            let updated_at = updated_at.or_else(|| mtime_iso(&file));
            Some(PiSessionListItem { session_id, cwd, title, updated_at, session_file: file })
        })
        .collect();
    items.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    items
}

pub fn find_pi_session(root: &Path, session_id: &str) -> Option<PiSessionListItem> {
    list_pi_sessions(root).into_iter().find(|item| item.session_id == session_id)
}
