//! Built-in slash commands that RPC and ACP modes run without a model call: `/usage`,
//! `/context`, `/extensions`, `/fast`, and `/reload`, plus the session commands RPC mode answers
//! as ACP mode does (`/session`, `/changelog`, `/export`, `/name`). Output is plain text that
//! also reads as Markdown; tables are fenced code blocks.

mod context;
mod extensions;
mod session_commands;

use std::sync::Arc;

pub use context::{ContextReport, ContextStyle};
pub use session_commands::{
    changelog_text, export_blocker, export_file_name, format_session_stats, run_session_command,
};

use crate::agent_session::AgentSession;
use crate::usage_cli::{fetch_from, report_text};

/// A built-in command as clients list it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HeadlessCommand {
    pub name: &'static str,
    pub description: &'static str,
    pub input_hint: Option<&'static str>,
}

const fn command(name: &'static str, description: &'static str, input_hint: Option<&'static str>) -> HeadlessCommand {
    HeadlessCommand { name, description, input_hint }
}

/// The commands [`run_headless_command`] runs.
pub const HEADLESS_COMMANDS: [HeadlessCommand; 5] = [
    command("usage", "Show subscription limits (5-hour, weekly) of logged-in providers", Some("[provider]")),
    command("context", "Show how the model's context window is used", None),
    command(
        "extensions",
        "List extensions, show what this session loaded, or enable and disable one",
        Some("list | status | enable <name-or-path> | disable <name-or-path>"),
    ),
    command("fast", "Toggle fast mode for models that support it", Some("[on|off]")),
    command("reload", "Reload extensions, skills, prompts, and context files", None),
];

/// The commands [`run_session_command`] runs; ACP mode answers them through its backend.
pub const SESSION_COMMANDS: [HeadlessCommand; 4] = [
    command("session", "Show session stats (messages, tokens, cost, session file)", None),
    command("changelog", "Show pi changelog", None),
    command("export", "Export session to an HTML file in the session cwd", None),
    command("name", "Set session display name", Some("<name>")),
];

/// What a command printed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HeadlessOutput {
    pub text: String,
    /// The session reloaded its extensions and resources, so its command list can differ.
    pub reloaded: bool,
}

impl HeadlessOutput {
    fn plain(text: impl Into<String>) -> Self {
        Self { text: text.into(), reloaded: false }
    }
}

/// `(name, arguments)` of a `/name arguments` prompt.
pub fn split_slash_command(text: &str) -> Option<(&str, &str)> {
    let body = text.trim().strip_prefix('/')?;
    let (name, args) = body.split_once(char::is_whitespace).unwrap_or((body, ""));
    (!name.is_empty()).then(|| (name, args.trim()))
}

fn fenced(text: &str) -> String {
    format!("```text\n{text}\n```")
}

/// Run built-in command `name` of [`HEADLESS_COMMANDS`]; `None` for another name.
pub async fn run_headless_command(session: &Arc<AgentSession>, name: &str, args: &str) -> Option<HeadlessOutput> {
    Some(match name {
        "usage" => {
            let provider = args.split_whitespace().next();
            let usages = fetch_from(session.model_runtime().usage(), provider).await;
            HeadlessOutput::plain(fenced(&report_text(&usages, pi_ai::now_ms() / 1000, &|_, text| text.to_string())))
        }
        "context" => HeadlessOutput::plain(fenced(&ContextReport::of_session(session).text(&|_, text| text.into()))),
        "extensions" => extensions::run(session, args).await,
        "fast" => HeadlessOutput::plain(fast(session, args)),
        "reload" => reload(session).await,
        _ => return None,
    })
}

/// The fast mode `/fast [on|off]` asks for; without a choice it toggles `current`.
pub fn requested_fast_mode(choice: Option<&str>, current: bool) -> Result<bool, String> {
    match choice.map(str::to_lowercase).as_deref() {
        None => Ok(!current),
        Some("on") => Ok(true),
        Some("off") => Ok(false),
        Some(other) => Err(format!("Unknown fast mode \"{other}\". Use on or off.")),
    }
}

pub fn fast_mode_status(enabled: bool, available: bool) -> &'static str {
    match (enabled, available) {
        (false, _) => "Fast mode off",
        (true, true) => "Fast mode on",
        (true, false) => "Fast mode on (the current model has no fast mode)",
    }
}

fn fast(session: &AgentSession, args: &str) -> String {
    let choice = Some(args).filter(|args| !args.is_empty());
    match requested_fast_mode(choice, session.fast_mode()) {
        Ok(enabled) => {
            session.set_fast_mode(enabled, true);
            fast_mode_status(enabled, session.fast_mode_available()).to_string()
        }
        Err(error) => error,
    }
}

/// Reload the session's extensions, skills, prompts, and context files, as the TUI `/reload`
/// does without its keybindings and themes.
async fn reload(session: &Arc<AgentSession>) -> HeadlessOutput {
    if session.is_streaming() {
        return HeadlessOutput::plain("Wait for the current response to finish before reloading.");
    }
    if session.is_compacting() {
        return HeadlessOutput::plain("Wait for compaction to finish before reloading.");
    }
    if let Err(error) = session.reload_extensions().await {
        return HeadlessOutput::plain(format!("Reload failed: {error:#}"));
    }
    let mut lines = vec!["Reloaded extensions, skills, prompts, and context files.".to_string()];
    {
        let services = session.services();
        let loader = services.resources();
        let resources = loader.resources();
        lines.push(format!(
            "Extensions: {}, skills: {}, prompts: {}, context files: {}",
            resources.extensions.len(),
            resources.skills.len(),
            resources.prompts.len(),
            resources.context_files.len()
        ));
    }
    if let Some(runner) = session.extension_runner() {
        lines.extend(
            runner.load_errors().into_iter().map(|error| format!("Failed to load {}: {}", error.path, error.error)),
        );
    }
    HeadlessOutput { text: lines.join("\n"), reloaded: true }
}
