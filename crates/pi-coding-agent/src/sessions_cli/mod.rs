//! `pi sessions` (a pi-rust extension): list, delete, and prune saved sessions. It reads session
//! files only: no network, no runtime, and no extensions. Only `rm` and `prune` write, and only
//! by deleting files.

mod format;
mod list;
mod remove;
mod scan;
#[cfg(test)]
mod tests;

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

use pi_ai::env::process_env_value;

pub use format::parse_duration;
pub use list::ListedSession;
pub use scan::{EventUsage, SessionEvent, scan_session_file, session_cost};

use crate::cli::CommandOutput;
use crate::config::expand_tilde;
use crate::resources::brand::{APP_NAME, CONFIG_DIR_NAME, session_dir_env_var};
use crate::session::SessionInfo;
use crate::session::listing::{list_all_sessions, list_project_sessions};
use crate::settings::{SettingsManager, SettingsOptions};

/// Asks a yes or no question on the terminal.
pub type Confirm<'a> = &'a mut dyn FnMut(&str) -> bool;

/// Where the commands find sessions.
#[derive(Clone, Debug)]
pub struct SessionScope {
    pub cwd: PathBuf,
    pub agent_dir: PathBuf,
    /// `--session-dir`, `PI_CODING_AGENT_SESSION_DIR`, or the `sessionDir` setting, as for `pi`.
    pub dir_override: Option<PathBuf>,
}

impl SessionScope {
    pub fn resolve(cwd: &Path, agent_dir: &Path, session_dir: Option<&str>) -> Self {
        let from_env = process_env_value(&session_dir_env_var()).filter(|dir| !dir.is_empty());
        let dir_override = session_dir.or(from_env.as_deref()).map(expand_tilde).or_else(|| {
            SettingsManager::create(cwd, agent_dir, SettingsOptions { project_trusted: true }).session_dir()
        });
        Self { cwd: cwd.to_path_buf(), agent_dir: agent_dir.to_path_buf(), dir_override }
    }

    /// Sessions of this project, or of every project with `all`; newest activity first.
    pub fn sessions(&self, all: bool) -> Vec<SessionInfo> {
        let dir = self.dir_override.as_deref();
        if all {
            list_all_sessions(dir, &self.agent_dir)
        } else {
            list_project_sessions(&self.cwd, dir, &self.agent_dir)
        }
    }
}

/// Options of `pi sessions`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Flags {
    pub all: bool,
    pub json: bool,
    pub yes: bool,
    pub dry_run: bool,
    pub help: bool,
    pub limit: Option<usize>,
    /// `--older-than`, in milliseconds.
    pub older_than: Option<i64>,
    pub session_dir: Option<String>,
    pub positional: Vec<String>,
}

const LIST_FLAGS: &[&str] = &["--all", "--json", "--limit", "--session-dir"];
const RM_FLAGS: &[&str] = &["--all", "--yes", "--session-dir"];
const PRUNE_FLAGS: &[&str] = &["--older-than", "--all", "--dry-run", "--yes", "--session-dir"];
const VALUED: &[&str] = &["--limit", "--older-than", "--session-dir"];

/// Parse `args`, accepting only the flags in `allowed` (plus `--help`). `--` ends the options.
pub fn parse_flags(args: &[String], allowed: &[&str]) -> Result<Flags, String> {
    let mut flags = Flags::default();
    let mut rest = args.iter();
    let mut options_done = false;
    while let Some(arg) = rest.next() {
        if options_done || !arg.starts_with('-') || arg == "-" {
            flags.positional.push(arg.clone());
            continue;
        }
        let (name, inline) = match arg.split_once('=') {
            Some((name, value)) => (name, Some(value.to_string())),
            None => (arg.as_str(), None),
        };
        let name = if name == "-y" { "--yes" } else { name };
        match name {
            "--" => options_done = true,
            "-h" | "--help" => flags.help = true,
            _ if !allowed.contains(&name) => return Err(format!("Unknown option {arg}")),
            _ if VALUED.contains(&name) => {
                let value = inline.or_else(|| rest.next().cloned()).ok_or_else(|| format!("{name} needs a value"))?;
                match name {
                    "--limit" => {
                        let limit = value.parse().map_err(|_| format!("--limit needs a number, not \"{value}\""))?;
                        flags.limit = Some(limit);
                    }
                    "--older-than" => flags.older_than = Some(parse_duration(&value)?),
                    _ => flags.session_dir = Some(value),
                }
            }
            _ if inline.is_some() => return Err(format!("{name} takes no value")),
            "--all" => flags.all = true,
            "--json" => flags.json = true,
            "--yes" => flags.yes = true,
            _ => flags.dry_run = true,
        }
    }
    Ok(flags)
}

fn usage_error(message: &str, usage: &str) -> CommandOutput {
    CommandOutput { code: 1, stdout: String::new(), stderr: format!("Error: {message}\nUsage: {usage}\n") }
}

fn help(text: String) -> CommandOutput {
    CommandOutput { stdout: text, ..CommandOutput::default() }
}

pub fn sessions_help() -> String {
    format!(
        "Usage:
  {APP_NAME} sessions [list] [--all] [--json] [--limit <n>]
  {APP_NAME} sessions rm <id-prefix|path>... [--all] [--yes]
  {APP_NAME} sessions prune --older-than <duration> [--all] [--dry-run] [--yes]

List, delete, or prune the saved sessions of the current project, newest first. Sessions are
the JSONL files under ~/{CONFIG_DIR_NAME}/agent/sessions/, or in --session-dir.

Commands:
  list    Short id, last activity, message count, cost, and name or first prompt
  rm      Delete sessions by id prefix (as listed) or file path; an id not found in this
          project is looked up in all projects, and a prefix that matches several is refused
  prune   Delete sessions with no activity for the given duration

Options:
  --all                    Use the sessions of every project
  --json                   Print the list as a JSON array
  --limit <n>              List at most n sessions
  --older-than <duration>  Age for prune: a number and a unit (s, m, h, d, w), such as 30d or 12h
  --dry-run                Show what prune would delete; delete nothing
  -y, --yes                Delete without asking; needed when stdin or stdout is not a terminal
  --session-dir <dir>      Directory for session storage and lookup
"
    )
}

/// `pi sessions ...` with the arguments after `sessions`. `confirm` is `None` when stdin or
/// stdout is not a terminal; deleting then needs `--yes`.
pub fn run_sessions(args: &[String], cwd: &Path, agent_dir: &Path, confirm: Option<Confirm<'_>>) -> CommandOutput {
    let (command, rest) = match args.first().map(String::as_str) {
        Some(command @ ("list" | "ls" | "rm" | "prune")) => (command, &args[1..]),
        Some("help") => return help(sessions_help()),
        Some(word) if !word.starts_with('-') => {
            return usage_error(&format!("Unknown command {word}"), &format!("{APP_NAME} sessions [list|rm|prune]"));
        }
        _ => ("list", args),
    };
    let allowed = match command {
        "rm" => RM_FLAGS,
        "prune" => PRUNE_FLAGS,
        _ => LIST_FLAGS,
    };
    let usage = format!("{APP_NAME} sessions --help");
    let flags = match parse_flags(rest, allowed) {
        Ok(flags) if flags.help => return help(sessions_help()),
        Ok(flags) => flags,
        Err(error) => return usage_error(&error, &usage),
    };
    let scope = SessionScope::resolve(cwd, agent_dir, flags.session_dir.as_deref());
    match command {
        "rm" => remove::remove(&flags, &scope, confirm),
        "prune" => remove::prune(&flags, &scope, confirm),
        _ if !flags.positional.is_empty() => {
            usage_error(&format!("Unexpected argument {}", flags.positional[0]), &usage)
        }
        _ => list::list(&flags, &scope),
    }
}

/// Read `y` or `yes` from the terminal after printing `question`.
fn ask_terminal(question: &str) -> bool {
    print!("{question}");
    let _ = std::io::stdout().flush();
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer).is_ok() && matches!(answer.trim().to_lowercase().as_str(), "y" | "yes")
}

/// Run `pi sessions ...`; `None` when `raw` is another command.
pub fn run_sessions_command(raw: &[String], cwd: &Path, agent_dir: &Path) -> Option<CommandOutput> {
    if raw.first().map(String::as_str) != Some("sessions") {
        return None;
    }
    let terminal = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    let mut ask = ask_terminal;
    let confirm: Option<Confirm<'_>> = if terminal { Some(&mut ask) } else { None };
    Some(run_sessions(&raw[1..], cwd, agent_dir, confirm))
}

/// Run `pi sessions ...`; `None` when `raw` is another command.
pub fn run_command(raw: &[String], cwd: &Path, agent_dir: &Path) -> Option<CommandOutput> {
    run_sessions_command(raw, cwd, agent_dir)
}
