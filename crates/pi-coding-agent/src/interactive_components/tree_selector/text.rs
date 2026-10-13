//! Row text of tree entries: display text, search text, copy text, tool call summaries, and
//! label timestamps. Entries are read from their stored JSON, as upstream reads them.

use std::collections::HashMap;

use serde_json::{Map, Value};

use super::nodes::{ToolCallInfo, TreeNode};
use crate::session::parse_iso;
use crate::theme::{ThemeColor, theme};

/// JavaScript truthiness of a JSON value.
fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(flag)) => *flag,
        Some(Value::Number(number)) => number.as_f64().is_some_and(|n| n != 0.0 && !n.is_nan()),
        Some(Value::String(text)) => !text.is_empty(),
        Some(_) => true,
    }
}

fn js_number(number: f64) -> String {
    if number.fract() == 0.0 && number.abs() < 1e21 { format!("{}", number as i64) } else { format!("{number}") }
}

/// JavaScript `String(value)` of a JSON value.
fn js_string(value: Option<&Value>) -> String {
    match value {
        None => "undefined".into(),
        Some(Value::Null) => "null".into(),
        Some(Value::Bool(flag)) => flag.to_string(),
        Some(Value::Number(number)) => number.as_f64().map(js_number).unwrap_or_default(),
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| if item.is_null() { String::new() } else { js_string(Some(item)) })
            .collect::<Vec<_>>()
            .join(","),
        Some(Value::Object(_)) => "[object Object]".into(),
    }
}

/// A value as `Array.prototype.join` prints it: `null` and `undefined` are empty.
fn join_part(value: Option<&Value>) -> String {
    if value.is_none_or(Value::is_null) { String::new() } else { js_string(value) }
}

/// `String(a || b || fallback)` over JSON arguments.
fn or_string(args: &Map<String, Value>, keys: &[&str], fallback: &str) -> String {
    keys.iter().map(|key| args.get(*key)).find(|value| truthy(*value)).map_or(fallback.to_string(), js_string)
}

/// JavaScript `text.length` (UTF-16 code units).
fn js_len(text: &str) -> usize {
    text.encode_utf16().count()
}

/// JavaScript `text.slice(0, end)`, keeping whole characters.
pub(super) fn js_slice(text: &str, end: usize) -> &str {
    let mut units = 0;
    for (index, ch) in text.char_indices() {
        units += ch.len_utf16();
        if units > end {
            return &text[..index];
        }
    }
    text
}

fn js_trim(text: &str) -> &str {
    text.trim_matches(|ch: char| ch.is_whitespace() || ch == '\u{feff}')
}

/// Tabs and newlines become spaces, then trimmed.
fn normalize(text: &str) -> String {
    js_trim(&text.replace(['\n', '\t'], " ")).to_string()
}

fn str_field<'a>(object: &'a Map<String, Value>, key: &str) -> &'a str {
    object.get(key).and_then(Value::as_str).unwrap_or_default()
}

/// Text of a string or of the text blocks of a content array.
pub(super) fn extract_full_content(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
            .map(|block| js_string(block.get("text")))
            .collect(),
        _ => String::new(),
    }
}

fn extract_content(content: Option<&Value>) -> String {
    js_slice(&extract_full_content(content), 200).to_string()
}

/// Whether content has non-blank text.
pub(super) fn has_text_content(content: Option<&Value>) -> bool {
    match content {
        Some(Value::String(text)) => !js_trim(text).is_empty(),
        Some(Value::Array(blocks)) => blocks.iter().any(|block| {
            block.get("type").and_then(Value::as_str) == Some("text")
                && block.get("text").and_then(Value::as_str).is_some_and(|text| !js_trim(text).is_empty())
        }),
        _ => false,
    }
}

fn message(node: &TreeNode) -> Option<&Map<String, Value>> {
    node.entry.raw().get("message").and_then(Value::as_object)
}

/// Text that search matches against: label, type words, and content.
pub(super) fn searchable_text(node: &TreeNode) -> String {
    let raw = node.entry.raw();
    let mut parts: Vec<String> = node.label.iter().filter(|label| !label.is_empty()).cloned().collect();
    match node.entry.kind() {
        "message" => {
            let empty = Map::new();
            let msg = message(node).unwrap_or(&empty);
            parts.push(join_part(msg.get("role")));
            if truthy(msg.get("content")) {
                parts.push(extract_content(msg.get("content")));
            }
            if str_field(msg, "role") == "bashExecution" && truthy(msg.get("command")) {
                parts.push(js_string(msg.get("command")));
            }
        }
        "custom_message" => {
            parts.push(join_part(raw.get("customType")));
            match raw.get("content") {
                Some(Value::String(text)) => parts.push(text.clone()),
                content => parts.push(extract_content(content)),
            }
        }
        "compaction" => parts.push("compaction".into()),
        "branch_summary" => parts.extend(["branch summary".into(), join_part(raw.get("summary"))]),
        "session_info" => {
            parts.push("title".into());
            if truthy(raw.get("name")) {
                parts.push(js_string(raw.get("name")));
            }
        }
        "model_change" => parts.extend(["model".into(), join_part(raw.get("modelId"))]),
        "thinking_level_change" => parts.extend(["thinking".into(), join_part(raw.get("thinkingLevel"))]),
        "custom" => parts.extend(["custom".into(), join_part(raw.get("customType"))]),
        "context_edit" => {
            parts.extend(["context edit".into(), context_edit_kind(raw).into(), join_part(raw.get("targetId"))])
        }
        "label" => parts.extend(["label".into(), join_part(raw.get("label"))]),
        _ => {}
    }
    parts.join(" ")
}

fn context_edit_kind(raw: &Map<String, Value>) -> &'static str {
    if raw.get("replacement").is_some_and(Value::is_null) { "omit" } else { "replace" }
}

/// Full text copied for an entry, or `None` when it has no text.
pub(super) fn entry_copy_text(node: &TreeNode) -> Option<String> {
    let raw = node.entry.raw();
    let text = match node.entry.kind() {
        "message" => message(node).and_then(|msg| {
            if str_field(msg, "role") == "bashExecution" {
                msg.get("command").and_then(Value::as_str).map(String::from)
            } else if msg.contains_key("content") {
                let text = extract_full_content(msg.get("content"));
                if text.is_empty() && str_field(msg, "role") == "assistant" {
                    msg.get("errorMessage").and_then(Value::as_str).map(String::from)
                } else {
                    Some(text)
                }
            } else {
                None
            }
        }),
        "custom_message" => Some(extract_full_content(raw.get("content"))),
        "compaction" | "branch_summary" => raw.get("summary").and_then(Value::as_str).map(String::from),
        _ => None,
    };
    text.filter(|text| !js_trim(text).is_empty())
}

/// The colored row text of an entry, bold when selected.
pub(super) fn entry_display_text(
    node: &TreeNode,
    tool_calls: &HashMap<String, ToolCallInfo>,
    is_selected: bool,
) -> String {
    let theme = theme();
    let raw = node.entry.raw();
    let dim = |text: &str| theme.fg(ThemeColor::Dim, text);
    let result = match node.entry.kind() {
        "message" => {
            let empty = Map::new();
            let msg = message(node).unwrap_or(&empty);
            match str_field(msg, "role") {
                "user" => {
                    let content = normalize(&extract_content(msg.get("content")));
                    theme.fg(ThemeColor::Accent, "user: ") + &content
                }
                "assistant" => {
                    let label = theme.fg(ThemeColor::Success, "assistant: ");
                    let text = normalize(&extract_content(msg.get("content")));
                    if !text.is_empty() {
                        label + &text
                    } else if str_field(msg, "stopReason") == "aborted" {
                        label + &theme.fg(ThemeColor::Muted, "(aborted)")
                    } else if truthy(msg.get("errorMessage")) {
                        let error = normalize(&js_string(msg.get("errorMessage")));
                        label + &theme.fg(ThemeColor::Error, js_slice(&error, 80))
                    } else {
                        label + &theme.fg(ThemeColor::Muted, "(no content)")
                    }
                }
                "toolResult" => {
                    let id = msg.get("toolCallId").and_then(Value::as_str).filter(|id| !id.is_empty());
                    let call = id.and_then(|id| tool_calls.get(id));
                    match call {
                        Some(call) => theme.fg(ThemeColor::Muted, &format_tool_call(&call.name, &call.arguments)),
                        None => {
                            let name = msg.get("toolName").and_then(Value::as_str).unwrap_or("tool");
                            theme.fg(ThemeColor::Muted, &format!("[{name}]"))
                        }
                    }
                }
                "bashExecution" => dim(&format!("[bash]: {}", normalize(str_field(msg, "command")))),
                _ => dim(&format!("[{}]", js_string(msg.get("role")))),
            }
        }
        "custom_message" => {
            let label = format!("[{}]: ", js_string(raw.get("customType")));
            let content = match raw.get("content") {
                Some(Value::Array(blocks)) => blocks
                    .iter()
                    .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
                    .map(|block| join_part(block.get("text")))
                    .collect(),
                content => extract_full_content(content),
            };
            theme.fg(ThemeColor::CustomMessageLabel, &label) + &normalize(&content)
        }
        "compaction" => {
            let tokens = raw.get("tokensBefore").and_then(Value::as_f64).unwrap_or(f64::NAN);
            let tokens = (tokens / 1000.0 + 0.5).floor();
            let tokens = if tokens.is_nan() { "NaN".to_string() } else { js_number(tokens) };
            theme.fg(ThemeColor::BorderAccent, &format!("[compaction: {tokens}k tokens]"))
        }
        "branch_summary" => theme.fg(ThemeColor::Warning, "[branch summary]: ") + &normalize(str_field(raw, "summary")),
        "model_change" => dim(&format!("[model: {}]", js_string(raw.get("modelId")))),
        "thinking_level_change" => dim(&format!("[thinking: {}]", js_string(raw.get("thinkingLevel")))),
        "custom" => dim(&format!("[custom: {}]", js_string(raw.get("customType")))),
        "context_edit" => dim(&format!("[context {}: {}]", context_edit_kind(raw), js_string(raw.get("targetId")))),
        "label" => dim(&format!("[label: {}]", raw.get("label").and_then(Value::as_str).unwrap_or("(cleared)"))),
        "session_info" => {
            let name =
                if truthy(raw.get("name")) { dim(&js_string(raw.get("name"))) } else { theme.italic(&dim("empty")) };
            format!("{}{name}{}", dim("[title: "), dim("]"))
        }
        _ => String::new(),
    };
    if is_selected { theme.bold(&result) } else { result }
}

fn shorten_path(path: &str) -> String {
    let home = ["HOME", "USERPROFILE"].iter().filter_map(|key| std::env::var(key).ok()).find(|home| !home.is_empty());
    match home {
        Some(home) if path.starts_with(&home) => format!("~{}", &path[home.len()..]),
        _ => path.to_string(),
    }
}

/// A one-line summary of a tool call, such as `[read: ~/src/main.rs:1-20]`.
pub(super) fn format_tool_call(name: &str, args: &Map<String, Value>) -> String {
    let path = || shorten_path(&or_string(args, &["path", "file_path"], ""));
    match name {
        "read" => {
            let mut display = path();
            let offset = args.get("offset").map(|value| value.as_f64());
            let limit = args.get("limit").map(|value| value.as_f64().unwrap_or(0.0));
            if offset.is_some() || limit.is_some() {
                let start = offset.flatten().unwrap_or(1.0);
                let end = limit.map(|limit| start + limit - 1.0).filter(|end| *end != 0.0 && !end.is_nan());
                display += &format!(":{}", js_number(start));
                if let Some(end) = end {
                    display += &format!("-{}", js_number(end));
                }
            }
            format!("[read: {display}]")
        }
        "write" => format!("[write: {}]", path()),
        "edit" => format!("[edit: {}]", path()),
        "bash" => {
            let raw_command = or_string(args, &["command"], "");
            let command = normalize(&raw_command);
            let ellipsis = if js_len(&raw_command) > 50 { "..." } else { "" };
            format!("[bash: {}{ellipsis}]", js_slice(&command, 50))
        }
        "grep" => {
            let pattern = or_string(args, &["pattern"], "");
            format!("[grep: /{pattern}/ in {}]", shorten_path(&or_string(args, &["path"], ".")))
        }
        "find" => {
            let pattern = or_string(args, &["pattern"], "");
            format!("[find: {pattern} in {}]", shorten_path(&or_string(args, &["path"], ".")))
        }
        "ls" => format!("[ls: {}]", shorten_path(&or_string(args, &["path"], "."))),
        _ => {
            let json = serde_json::to_string(args).unwrap_or_default();
            let ellipsis = if js_len(&json) > 40 { "..." } else { "" };
            format!("[{name}: {}{ellipsis}]", js_slice(&json, 40))
        }
    }
}

/// `HH:MM` today, `M/D HH:MM` this year, else `YY/M/D HH:MM`, in local time.
pub(super) fn format_label_timestamp(timestamp: &str) -> String {
    let Some(millis) = parse_iso(timestamp) else { return "aN/NaN/NaN NaN:NaN".into() };
    let Ok(instant) = jiff::Timestamp::from_millisecond(millis) else { return "aN/NaN/NaN NaN:NaN".into() };
    let zone = jiff::tz::TimeZone::system();
    let date = instant.to_zoned(zone.clone());
    let now = jiff::Timestamp::now().to_zoned(zone);
    let time = format!("{:02}:{:02}", date.hour(), date.minute());
    if date.date() == now.date() {
        return time;
    }
    if date.year() == now.year() {
        return format!("{}/{} {time}", date.month(), date.day());
    }
    let year = date.year().to_string();
    let short = &year[year.len().saturating_sub(2)..];
    format!("{short}/{}/{} {time}", date.month(), date.day())
}
