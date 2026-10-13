//! Slash commands (upstream `slash-commands.ts`, `pi-commands.ts`, and the built-in list in
//! `agent.ts`): prompt files, pi's command list, and the commands ACP handles itself.

use std::path::Path;

use agent_client_protocol_schema::{AvailableCommand, AvailableCommandInput, UnstructuredCommandInput};
use serde_json::Value;

use crate::resources::brand::project_config_dir;

/// A prompt file under `prompts/` (upstream `FileSlashCommand`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileSlashCommand {
    pub name: String,
    pub description: String,
    pub content: String,
    /// `(user)`, `(project)`, or with a subdirectory, `(project:frontend)`.
    pub source: String,
}

fn parse_frontmatter(content: &str) -> (Vec<(String, String)>, String) {
    let mut fields = Vec::new();
    if !content.starts_with("---") {
        return (fields, content.to_string());
    }
    let Some(end) = content[3..].find("\n---").map(|index| index + 3) else {
        return (fields, content.to_string());
    };
    let block = content.get(4..end).unwrap_or_default();
    let rest = content.get(end + 4..).unwrap_or_default().trim().to_string();
    for line in block.split('\n') {
        let Some((key, value)) = line.split_once(':') else { continue };
        if !key.is_empty() && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            fields.push((key.to_string(), value.trim().to_string()));
        }
    }
    (fields, rest)
}

fn load_dir(dir: &Path, user: bool, subdir: &str, out: &mut Vec<FileSlashCommand>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        let Ok(kind) = entry.file_type() else { continue };
        if kind.is_dir() {
            let nested = if subdir.is_empty() { name } else { format!("{subdir}:{name}") };
            load_dir(&path, user, &nested, out);
            continue;
        }
        let Some(command) = name.strip_suffix(".md").filter(|_| kind.is_file()) else { continue };
        let Ok(raw) = std::fs::read_to_string(&path) else { continue };
        let (fields, content) = parse_frontmatter(&raw);
        let scope = if user { "user" } else { "project" };
        let source = if subdir.is_empty() { format!("({scope})") } else { format!("({scope}:{subdir})") };
        let mut description = fields.iter().find(|(key, _)| key == "description").map(|(_, value)| value.clone());
        if description.as_deref().is_none_or(str::is_empty) {
            description = content.split('\n').find(|line| !line.trim().is_empty()).map(|line| {
                let short: String = line.chars().take(60).collect();
                if line.chars().count() > 60 { format!("{short}...") } else { short }
            });
        }
        let description = match description.filter(|text| !text.is_empty()) {
            Some(text) => format!("{text} {source}"),
            None => source.clone(),
        };
        out.push(FileSlashCommand { name: command.to_string(), description, content, source });
    }
}

/// Prompt files of `<agent_dir>/prompts` and `<cwd>/.pi/prompts`, user ones first.
pub fn load_slash_commands(cwd: &Path, agent_dir: &Path) -> Vec<FileSlashCommand> {
    let mut commands = Vec::new();
    load_dir(&agent_dir.join("prompts"), true, "", &mut commands);
    load_dir(&project_config_dir(cwd).join("prompts"), false, "", &mut commands);
    commands
}

/// ACP commands for prompt files, de-duplicated by name (first wins).
pub fn to_available_commands(commands: &[FileSlashCommand]) -> Vec<AvailableCommand> {
    let list = commands.iter().map(|command| AvailableCommand::new(command.name.clone(), command.description.clone()));
    merge_commands(list.collect(), Vec::new())
}

/// Split arguments on spaces and tabs, honoring single and double quotes.
pub fn parse_command_args(input: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    for c in input.chars() {
        if let Some(open) = quote {
            if c == open {
                quote = None;
            } else {
                current.push(c);
            }
            continue;
        }
        match c {
            '"' | '\'' => quote = Some(c),
            ' ' | '\t' => {
                if !current.is_empty() {
                    args.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(c),
        }
    }
    if !current.is_empty() {
        args.push(current);
    }
    args
}

/// Replace `$@` with all arguments and `$1`, `$2`, ... with each one.
pub fn substitute_args(content: &str, args: &[String]) -> String {
    let all = content.replace("$@", &args.join(" "));
    let mut out = String::with_capacity(all.len());
    let mut chars = all.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '$' || !chars.peek().is_some_and(char::is_ascii_digit) {
            out.push(c);
            continue;
        }
        let mut digits = String::new();
        while let Some(digit) = chars.peek().filter(|c| c.is_ascii_digit()) {
            digits.push(*digit);
            chars.next();
        }
        let index = digits.parse::<usize>().unwrap_or(0);
        if let Some(arg) = index.checked_sub(1).and_then(|index| args.get(index)) {
            out.push_str(arg);
        }
    }
    out
}

/// Expand a leading `/command` from the prompt files; other text is returned as is.
pub fn expand_slash_command(text: &str, commands: &[FileSlashCommand]) -> String {
    let Some(rest) = text.strip_prefix('/') else { return text.to_string() };
    let (name, args) = rest.split_once(' ').unwrap_or((rest, ""));
    match commands.iter().find(|command| command.name == name) {
        Some(command) => substitute_args(&command.content, &parse_command_args(args)),
        None => text.to_string(),
    }
}

/// Commands from pi's `get_commands` data: extension commands only when asked, and skill
/// commands unless `enableSkillCommands` is off.
pub fn commands_from_pi(data: &Value, enable_skill_commands: bool, include_extension: bool) -> Vec<AvailableCommand> {
    let list = data.get("commands").or_else(|| data.get("data").and_then(|data| data.get("commands")));
    let mut out = Vec::new();
    for command in list.and_then(Value::as_array).into_iter().flatten() {
        let name = command["name"].as_str().map(str::trim).unwrap_or_default();
        if name.is_empty() {
            continue;
        }
        let source = command["source"].as_str().unwrap_or_default();
        if (!include_extension && source == "extension") || (!enable_skill_commands && name.starts_with("skill:")) {
            continue;
        }
        let description = command["description"].as_str().map(str::trim).unwrap_or_default();
        let description = if description.is_empty() {
            let location = command["location"].as_str().unwrap_or_default();
            let parts: Vec<&str> = [source, location].into_iter().filter(|part| !part.is_empty()).collect();
            if parts.is_empty() { "(command)".to_string() } else { format!("({})", parts.join(":")) }
        } else {
            description.to_string()
        };
        out.push(AvailableCommand::new(name, description));
    }
    out
}

fn with_hint(name: &str, description: &str, hint: Option<&str>) -> AvailableCommand {
    let command = AvailableCommand::new(name, description);
    match hint {
        Some(hint) => command.input(AvailableCommandInput::Unstructured(UnstructuredCommandInput::new(hint))),
        None => command,
    }
}

/// The commands ACP mode answers without a model call.
pub fn builtin_available_commands() -> Vec<AvailableCommand> {
    vec![
        with_hint("compact", "Manually compact the session context", Some("optional custom instructions")),
        with_hint("autocompact", "Toggle automatic context compaction", Some("on|off|toggle")),
        with_hint("export", "Export session to an HTML file in the session cwd", None),
        with_hint("session", "Show session stats (messages, tokens, cost, session file)", None),
        with_hint("name", "Set session display name", Some("<name>")),
        with_hint(
            "steering",
            "Get/set pi steering message delivery mode (how queued steering messages are delivered)",
            Some("(no args to show) all | one-at-a-time"),
        ),
        with_hint(
            "follow-up",
            "Get/set pi follow-up message delivery mode (how queued follow-up messages are delivered)",
            Some("(no args to show) all | one-at-a-time"),
        ),
        with_hint("changelog", "Show pi changelog", None),
    ]
}

/// Concatenate, keeping the first command of each name.
pub fn merge_commands(first: Vec<AvailableCommand>, second: Vec<AvailableCommand>) -> Vec<AvailableCommand> {
    let mut seen = std::collections::HashSet::new();
    first.into_iter().chain(second).filter(|command| seen.insert(command.name.clone())).collect()
}
