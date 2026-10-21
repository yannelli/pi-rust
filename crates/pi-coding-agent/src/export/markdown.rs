//! Markdown export of the current branch (pi-rust extension; upstream exports only HTML).
//! `/export` and `pi --export` write it when the output path ends in `.md`.

use std::collections::HashSet;
use std::path::PathBuf;

use serde_json::Value;

use super::html::{resolve_export_input, strip_sgr};
use super::session_file::SessionSnapshot;
use crate::session::Entry;
use crate::session::usage::usage_totals;

/// Lines of a tool result or command output kept in the export; a note replaces the rest.
pub const MAX_RESULT_LINES: usize = 200;
const TITLE_CHARS: usize = 80;

/// Whether an export to `path` writes Markdown: the path ends in `.md` (any case).
pub fn is_markdown_path(path: &str) -> bool {
    path.get(path.len().saturating_sub(3)..).is_some_and(|ext| ext.eq_ignore_ascii_case(".md"))
}

/// Write [`generate_markdown`] to `output` (with `~` expanded); returns the written path.
pub fn write_markdown(snapshot: &SessionSnapshot, output: &str) -> anyhow::Result<PathBuf> {
    let path = crate::config::expand_tilde(output);
    std::fs::write(&path, generate_markdown(snapshot))?;
    Ok(path)
}

/// `pi --export <input> <output.md>`: export any session file as Markdown.
pub fn export_markdown_from_file(input: &str, output: &str) -> anyhow::Result<PathBuf> {
    let resolved = resolve_export_input(input)?;
    write_markdown(&SessionSnapshot::load(&resolved)?, output)
}

/// The session header and the branch from the root to the leaf, as Markdown.
pub fn generate_markdown(snapshot: &SessionSnapshot) -> String {
    let branch = snapshot.branch();
    let mut writer = Writer { branch: &branch, consumed: HashSet::new(), started: false, out: String::new() };
    writer.header(snapshot);
    for index in 0..branch.len() {
        writer.entry(index);
    }
    let mut out = writer.out.trim_end().to_string();
    out.push('\n');
    out
}

struct Writer<'a> {
    branch: &'a [&'a Value],
    /// Branch indices of tool results already shown under their call.
    consumed: HashSet<usize>,
    /// Whether a message came before; the first model change only repeats the header.
    started: bool,
    out: String,
}

impl Writer<'_> {
    fn block(&mut self, text: &str) {
        if text.trim().is_empty() {
            return;
        }
        self.out.push_str(text.trim_end_matches('\n'));
        self.out.push_str("\n\n");
    }

    fn header(&mut self, snapshot: &SessionSnapshot) {
        let title = session_name(&snapshot.entries)
            .or_else(|| self.branch.iter().find_map(|entry| first_prompt_title(entry)))
            .unwrap_or_else(|| format!("{} session", super::APP_NAME));
        self.block(&format!("# {title}"));
        let mut lines = Vec::new();
        if let Some(timestamp) = snapshot.header.get("timestamp").and_then(Value::as_str) {
            lines.push(format!("- **Date:** {}", display_timestamp(timestamp)));
        }
        let models = session_models(&snapshot.entries);
        if !models.is_empty() {
            let label = if models.len() == 1 { "Model" } else { "Models" };
            let list: Vec<String> = models.iter().map(|model| inline_code(model)).collect();
            lines.push(format!("- **{label}:** {}", list.join(", ")));
        }
        if let Some(cwd) = snapshot.header.get("cwd").and_then(Value::as_str) {
            lines.push(format!("- **Working directory:** {}", inline_code(cwd)));
        }
        let entries: Vec<Entry> =
            snapshot.entries.iter().filter_map(|entry| Some(Entry::from_raw(entry.as_object()?.clone()))).collect();
        let totals = usage_totals(&entries);
        if totals.tokens() > 0 {
            let parts = [
                ("input", totals.input),
                ("output", totals.output),
                ("cache read", totals.cache_read),
                ("cache write", totals.cache_write),
            ];
            let parts: Vec<String> = parts
                .iter()
                .filter(|(_, count)| *count > 0)
                .map(|(name, count)| format!("{name} {}", thousands(*count)))
                .collect();
            lines.push(format!("- **Tokens:** {} ({})", thousands(totals.tokens()), parts.join(", ")));
        }
        if totals.cost > 0.0 {
            lines.push(format!("- **Cost:** ${:.3}", totals.cost));
        }
        if !lines.is_empty() {
            self.block(&lines.join("\n"));
        }
        self.block("---");
    }

    fn entry(&mut self, index: usize) {
        let entry = self.branch[index];
        match str_field(entry, "type") {
            "message" => {
                self.message(index, &entry["message"]);
                self.started = true;
            }
            "compaction" => {
                let mut note = "**Compaction:**".to_string();
                if let Some(tokens) = entry.get("tokensBefore").and_then(Value::as_f64) {
                    note.push_str(&format!(" compacted from {} tokens.", thousands(tokens.max(0.0) as u64)));
                }
                self.block(&quoted(&format!("{note}\n\n{}", str_field(entry, "summary"))));
            }
            "branch_summary" => {
                self.block(&quoted(&format!("**Branch summary:**\n\n{}", str_field(entry, "summary"))));
            }
            "custom_message" => self.custom(entry),
            "model_change" if self.started => {
                let model = format!("{}/{}", str_field(entry, "provider"), str_field(entry, "modelId"));
                self.block(&format!("*Switched to model {}*", inline_code(&model)));
            }
            _ => {}
        }
    }

    fn message(&mut self, index: usize, message: &Value) {
        match str_field(message, "role") {
            "user" => {
                self.block("## User");
                let text = content_text(&message["content"]);
                match parse_skill(&text) {
                    Some(skill) => {
                        self.block(&details(&format!("Skill: {}", skill.name), skill.content));
                        self.block(skill.message);
                    }
                    None => self.block(&text),
                }
                self.images(&message["content"]);
            }
            "assistant" => self.assistant(index, message),
            "toolResult" if !self.consumed.contains(&index) => {
                self.block(&format!("### Tool result: {}", inline_code(str_field(message, "toolName"))));
                self.tool_result(message);
            }
            "bashExecution" => self.bash(message),
            "custom" => self.custom(message),
            _ => {}
        }
    }

    fn assistant(&mut self, index: usize, message: &Value) {
        self.block("## Assistant");
        for block in message["content"].as_array().into_iter().flatten() {
            match str_field(block, "type") {
                "text" => self.block(str_field(block, "text")),
                "thinking" if !str_field(block, "thinking").trim().is_empty() => {
                    self.block(&details("Thinking", str_field(block, "thinking")));
                }
                "toolCall" => self.tool_call(index, block),
                _ => {}
            }
        }
        match str_field(message, "stopReason") {
            "aborted" => self.block("*Aborted*"),
            "error" => {
                let error = message.get("errorMessage").and_then(Value::as_str).unwrap_or("Unknown error");
                self.block(&format!("*Error: {error}*"));
            }
            _ => {}
        }
    }

    fn tool_call(&mut self, index: usize, call: &Value) {
        self.block(&format!("### Tool call: {}", inline_code(str_field(call, "name"))));
        let arguments = serde_json::to_string_pretty(call.get("arguments").unwrap_or(&Value::Null)).unwrap_or_default();
        self.block(&fenced(&arguments, "json"));
        let (id, branch) = (str_field(call, "id"), self.branch);
        let later = branch.iter().enumerate().skip(index + 1);
        let found = later
            .take_while(|(_, entry)| entry.pointer("/message/role").and_then(Value::as_str) != Some("assistant"))
            .find(|(position, entry)| {
                !self.consumed.contains(position)
                    && entry.pointer("/message/role").and_then(Value::as_str) == Some("toolResult")
                    && entry.pointer("/message/toolCallId").and_then(Value::as_str) == Some(id)
            })
            .map(|(position, entry)| (position, &entry["message"]));
        if let Some((position, result)) = found {
            self.consumed.insert(position);
            self.tool_result(result);
        }
    }

    fn tool_result(&mut self, result: &Value) {
        let error = result.get("isError").and_then(Value::as_bool) == Some(true);
        self.block(if error { "**Result (error):**" } else { "**Result:**" });
        let text = strip_sgr(&content_text(&result["content"]));
        let has_image = result["content"].as_array().is_some_and(|blocks| blocks.iter().any(is_image));
        if !text.trim().is_empty() || !has_image {
            self.output(&text);
        }
        self.images(&result["content"]);
    }

    fn bash(&mut self, message: &Value) {
        self.block("## Bash");
        let output = strip_sgr(str_field(message, "output"));
        self.output(&format!("$ {}\n{output}", str_field(message, "command")));
        let exit_code = message.get("exitCode").and_then(Value::as_i64);
        if message.get("cancelled").and_then(Value::as_bool) == Some(true) {
            self.block("*Cancelled*");
        } else if let Some(code) = exit_code.filter(|code| *code != 0) {
            self.block(&format!("*Exit code {code}*"));
        }
    }

    fn custom(&mut self, item: &Value) {
        let text = content_text(&item["content"]);
        self.block(&quoted(&format!("**{}:**\n\n{text}", str_field(item, "customType"))));
        self.images(&item["content"]);
    }

    /// A fenced block of `text`, cut to [`MAX_RESULT_LINES`] lines with a note.
    fn output(&mut self, text: &str) {
        let text = text.trim_end_matches('\n');
        if text.trim().is_empty() {
            return self.block("*No output*");
        }
        let total = text.lines().count();
        if total <= MAX_RESULT_LINES {
            return self.block(&fenced(text, ""));
        }
        let kept: Vec<&str> = text.lines().take(MAX_RESULT_LINES).collect();
        self.block(&fenced(&kept.join("\n"), ""));
        self.block(&format!("*Output truncated: showing {MAX_RESULT_LINES} of {total} lines.*"));
    }

    fn images(&mut self, content: &Value) {
        for block in content.as_array().into_iter().flatten().filter(|block| is_image(block)) {
            let mime = block.get("mimeType").and_then(Value::as_str).unwrap_or("image");
            self.block(&format!("*[Image: {mime}]*"));
        }
    }
}

fn str_field<'v>(value: &'v Value, key: &str) -> &'v str {
    value.get(key).and_then(Value::as_str).unwrap_or_default()
}

fn is_image(block: &Value) -> bool {
    str_field(block, "type") == "image"
}

/// The text of string content, or of the text blocks of block content, joined by newlines.
fn content_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter(|block| str_field(block, "type") == "text")
            .map(|block| str_field(block, "text"))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// The latest `session_info` name, as `SessionManager::session_name` reads it.
fn session_name(entries: &[Value]) -> Option<String> {
    entries
        .iter()
        .rev()
        .find(|entry| str_field(entry, "type") == "session_info")
        .map(|entry| str_field(entry, "name").trim().to_string())
        .filter(|name| !name.is_empty())
}

/// The first line of a user prompt, shortened to [`TITLE_CHARS`] characters.
fn first_prompt_title(entry: &Value) -> Option<String> {
    let message = entry.get("message").filter(|message| str_field(message, "role") == "user")?;
    let text = content_text(&message["content"]);
    let text = match parse_skill(&text) {
        Some(skill) if skill.message.trim().is_empty() => format!("Skill: {}", skill.name),
        Some(skill) => skill.message.to_string(),
        None => text,
    };
    let line = text.lines().map(str::trim).find(|line| !line.is_empty())?;
    if line.chars().count() <= TITLE_CHARS {
        return Some(line.to_string());
    }
    Some(format!("{}...", line.chars().take(TITLE_CHARS).collect::<String>().trim_end()))
}

/// `provider/model` of each assistant message, in order of first use.
fn session_models(entries: &[Value]) -> Vec<String> {
    let mut models: Vec<String> = Vec::new();
    for message in entries.iter().filter_map(|entry| entry.get("message")) {
        let model = str_field(message, "model");
        if str_field(message, "role") != "assistant" || model.is_empty() {
            continue;
        }
        let provider = str_field(message, "provider");
        let name = if provider.is_empty() { model.to_string() } else { format!("{provider}/{model}") };
        if !models.contains(&name) {
            models.push(name);
        }
    }
    models
}

/// `2026-01-02T03:04:05.678Z` as `2026-01-02 03:04:05 UTC`; other values unchanged.
fn display_timestamp(timestamp: &str) -> String {
    match (timestamp.get(..10), timestamp.get(10..11), timestamp.get(11..19)) {
        (Some(date), Some("T"), Some(time)) if timestamp.ends_with('Z') => format!("{date} {time} UTC"),
        _ => timestamp.to_string(),
    }
}

fn thousands(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::new();
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

fn longest_backtick_run(text: &str) -> usize {
    text.split(|c| c != '`').map(str::len).max().unwrap_or(0)
}

/// A fenced code block whose fence is longer than any backtick run in `content`.
pub(crate) fn fenced(content: &str, info: &str) -> String {
    let fence = "`".repeat((longest_backtick_run(content) + 1).max(3));
    format!("{fence}{info}\n{content}\n{fence}")
}

/// A code span that survives backticks in `text`.
pub(crate) fn inline_code(text: &str) -> String {
    let ticks = "`".repeat(longest_backtick_run(text) + 1);
    let pad = if text.starts_with('`') || text.ends_with('`') { " " } else { "" };
    format!("{ticks}{pad}{text}{pad}{ticks}")
}

/// `text` as a Markdown block quote.
pub(crate) fn quoted(text: &str) -> String {
    let lines: Vec<String> =
        text.trim_end().lines().map(|line| if line.is_empty() { ">".into() } else { format!("> {line}") }).collect();
    lines.join("\n")
}

/// A collapsed `<details>` block; a literal `</details>` in `body` is escaped so it cannot close it.
pub(crate) fn details(summary: &str, body: &str) -> String {
    let body = body.trim().replace("</details>", "&lt;/details>");
    format!("<details>\n<summary>{summary}</summary>\n\n{body}\n\n</details>")
}

struct Skill<'t> {
    name: &'t str,
    content: &'t str,
    message: &'t str,
}

/// A user prompt that starts with an expanded `/skill:name` block (upstream `parseSkillBlock`).
fn parse_skill(text: &str) -> Option<Skill<'_>> {
    let rest = text.strip_prefix("<skill name=\"")?;
    let (name, rest) = rest.split_once('"')?;
    let rest = rest.strip_prefix(" location=\"")?;
    let (_, rest) = rest.split_once("\">\n")?;
    let (content, message) = rest.split_once("\n</skill>")?;
    let message = message.strip_prefix("\n\n").or_else(|| message.is_empty().then_some(""))?;
    Some(Skill { name, content, message: message.trim() })
}
