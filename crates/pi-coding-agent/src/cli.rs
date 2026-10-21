//! Command-line parsing (upstream `cli/args.ts`), including multi-letter short forms such as
//! `-nt`. Unknown `--flags` are kept for extensions; `main` reports the ones nobody claims.

use pi_ai::ThinkingLevel;

use crate::agent_session::RunLimits;
use crate::settings::schema::TuiMode;
use crate::settings::tools::tool_list_error;

#[path = "cli_help.rs"]
mod help;
#[path = "cli_table.rs"]
pub mod table;

pub use help::{HelpFlag, help_text, help_text_with_flags};

/// Exit code and output of a subcommand that runs before the arguments become a prompt.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CommandOutput {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OutputMode {
    #[default]
    Text,
    Json,
    /// JSON-RPC over stdin and stdout; the protocol belongs to the machine-interface stream.
    Rpc,
    /// Agent Client Protocol over stdin and stdout, for editors such as Zed.
    Acp,
}

/// Value of an unknown `--flag`: `--flag=value` or `--flag value`, or `true` without one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FlagValue {
    Bool,
    String(String),
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Args {
    pub help: bool,
    pub version: bool,
    /// `--mode text|json|rpc|acp`; `json`, `rpc`, and `acp` imply non-interactive output.
    pub mode: Option<OutputMode>,
    /// `--terminal-login`: ACP Terminal Auth relaunches pi interactively with this flag.
    pub terminal_login: bool,
    pub print: bool,
    pub continue_session: bool,
    pub resume: bool,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub api_key: Option<String>,
    /// Text, or a file path whose contents replace the system prompt.
    pub system_prompt: Option<String>,
    /// Texts or file paths appended to the system prompt, in order.
    pub append_system_prompt: Vec<String>,
    pub thinking: Option<ThinkingLevel>,
    /// `-n/--name`: session display name (session stream).
    pub name: Option<String>,
    pub no_session: bool,
    /// Session file path, or a full or partial session id (session stream resolves ids).
    pub session: Option<String>,
    /// `--session-id`: exact project session id to open or create (session stream).
    pub session_id: Option<String>,
    /// `--fork`: session file or id to fork into a new session (session stream).
    pub fork: Option<String>,
    pub session_dir: Option<String>,
    /// `--models`: patterns for the model cycling scope.
    pub models: Option<Vec<String>>,
    pub tools: Option<Vec<String>>,
    pub exclude_tools: Vec<String>,
    pub no_tools: bool,
    pub no_builtin_tools: bool,
    /// `-e/--extension` paths or `builtin:<name>` (extension stream).
    pub extensions: Vec<String>,
    /// `-ne/--no-extensions` (extension stream).
    pub no_extensions: bool,
    /// `--no-mcp` (MCP stream).
    pub no_mcp: bool,
    /// `--export <file>`; the first message is the output path (export stream).
    pub export: Option<String>,
    /// `--skill` paths (resources stream).
    pub skills: Vec<String>,
    /// `-ns/--no-skills` (resources stream).
    pub no_skills: bool,
    /// `--prompt-template` paths (resources stream).
    pub prompt_templates: Vec<String>,
    /// `-np/--no-prompt-templates` (resources stream).
    pub no_prompt_templates: bool,
    /// `--theme` paths (TUI and resources streams).
    pub themes: Vec<String>,
    /// `--use-theme <name>` for this run (TUI stream).
    pub use_theme: Option<String>,
    /// `--no-themes` (TUI and resources streams).
    pub no_themes: bool,
    pub no_context_files: bool,
    /// `Some(search)` lists models; an empty search lists all.
    pub list_models: Option<String>,
    pub offline: bool,
    /// `--tui-mode regular|fullscreen` (TUI stream).
    pub tui_mode: Option<TuiMode>,
    /// `--verbose`: force verbose startup over `quietStartup`.
    pub verbose: bool,
    /// `-a/--approve` is `Some(true)`, `-na/--no-approve` is `Some(false)` (trust stream).
    pub project_trust_override: Option<bool>,
    /// `--max-turns` and `--max-cost` (pi-rust).
    pub run_limits: RunLimits,
    pub messages: Vec<String>,
    /// Paths given as `@path`.
    pub file_args: Vec<String>,
    /// Unknown long flags in order, for extension-registered options.
    pub unknown_flags: Vec<(String, FlagValue)>,
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
}

fn split_list(value: &str) -> Vec<String> {
    value.split(',').map(str::trim).filter(|item| !item.is_empty()).map(String::from).collect()
}

/// `--max-turns` or `--max-cost` with its value, validated.
fn set_run_limit(args: &mut Args, flag: &str, value: &str) {
    let limits = &mut args.run_limits;
    let result = match flag {
        "--max-turns" => RunLimits::parse_max_turns(value).map(|turns| limits.max_turns = Some(turns)),
        _ => RunLimits::parse_max_cost(value).map(|cost| limits.max_cost = Some(cost)),
    };
    args.errors.extend(result.err());
}

/// A display name with surrounding whitespace removed, or `None` when blank.
pub fn normalize_session_name(value: &str) -> Option<String> {
    let name = value.trim();
    (!name.is_empty()).then(|| name.to_string())
}

pub fn parse_args(raw: &[String]) -> Args {
    let mut args = Args::default();
    let mut index = 0;
    let value = |index: &mut usize, flag: &str, errors: &mut Vec<String>| -> Option<String> {
        match raw.get(*index + 1) {
            Some(next) => {
                *index += 1;
                Some(next.clone())
            }
            None => {
                errors.push(format!("{flag} requires a value"));
                None
            }
        }
    };
    let option_value = |index: &mut usize| -> Option<String> {
        let next = raw.get(*index + 1).filter(|next| !next.starts_with('-'))?;
        *index += 1;
        Some(next.clone())
    };
    while index < raw.len() {
        let arg = raw[index].as_str();
        match arg {
            "--" => {
                for positional in &raw[index + 1..] {
                    match positional.strip_prefix('@') {
                        Some(path) => args.file_args.push(path.into()),
                        None => args.messages.push(positional.clone()),
                    }
                }
                break;
            }
            "-h" | "--help" => args.help = true,
            "-v" | "--version" => args.version = true,
            "--mode" => match option_value(&mut index).as_deref() {
                Some("text") => args.mode = Some(OutputMode::Text),
                Some("json") => args.mode = Some(OutputMode::Json),
                Some("rpc") => args.mode = Some(OutputMode::Rpc),
                Some("acp") => args.mode = Some(OutputMode::Acp),
                Some(other) => {
                    args.errors.push(format!("Invalid mode \"{other}\". Valid values: text, json, rpc, acp"))
                }
                None => args.errors.push("--mode requires text, json, rpc, or acp".into()),
            },
            "--terminal-login" => args.terminal_login = true,
            "-c" | "--continue" => args.continue_session = true,
            "-r" | "--resume" => args.resume = true,
            "--provider" => args.provider = value(&mut index, arg, &mut args.errors),
            "--model" => args.model = value(&mut index, arg, &mut args.errors),
            "--api-key" => args.api_key = value(&mut index, arg, &mut args.errors),
            "--system-prompt" => args.system_prompt = value(&mut index, arg, &mut args.errors),
            "--append-system-prompt" => args.append_system_prompt.extend(value(&mut index, arg, &mut args.errors)),
            "-n" | "--name" => args.name = value(&mut index, "--name", &mut args.errors),
            "--no-session" => args.no_session = true,
            "--session" => args.session = value(&mut index, arg, &mut args.errors),
            "--session-id" => args.session_id = value(&mut index, arg, &mut args.errors),
            "--fork" => args.fork = value(&mut index, arg, &mut args.errors),
            "--session-dir" => args.session_dir = value(&mut index, arg, &mut args.errors),
            "--models" => args.models = value(&mut index, arg, &mut args.errors).map(|list| split_list(&list)),
            "-nt" | "--no-tools" => args.no_tools = true,
            "-nbt" | "--no-builtin-tools" => args.no_builtin_tools = true,
            "-t" | "--tools" => {
                if let Some(list) = value(&mut index, arg, &mut args.errors) {
                    let tools = split_list(&list);
                    match tool_list_error(&tools) {
                        Some(error) => args.errors.push(format!("{arg}: {error}")),
                        None => args.tools = Some(tools),
                    }
                }
            }
            "-xt" | "--exclude-tools" => {
                if let Some(list) = value(&mut index, arg, &mut args.errors) {
                    args.exclude_tools.extend(split_list(&list));
                }
            }
            "--thinking" => {
                if let Some(level) = value(&mut index, arg, &mut args.errors) {
                    match level.parse() {
                        Ok(level) => args.thinking = Some(level),
                        Err(_) => args.warnings.push(format!(
                            "Invalid thinking level \"{level}\". Valid values: off, minimal, low, medium, high, xhigh, max"
                        )),
                    }
                }
            }
            "-p" | "--print" => {
                args.print = true;
                if let Some(next) = raw.get(index + 1)
                    && !next.starts_with('@')
                    && (!next.starts_with('-') || next.starts_with("---"))
                {
                    args.messages.push(next.clone());
                    index += 1;
                }
            }
            "--export" => args.export = value(&mut index, arg, &mut args.errors),
            "-e" | "--extension" => args.extensions.extend(value(&mut index, arg, &mut args.errors)),
            "-ne" | "--no-extensions" => args.no_extensions = true,
            "--no-mcp" => args.no_mcp = true,
            "--skill" => args.skills.extend(value(&mut index, arg, &mut args.errors)),
            "--prompt-template" => args.prompt_templates.extend(value(&mut index, arg, &mut args.errors)),
            "--theme" => args.themes.extend(value(&mut index, arg, &mut args.errors)),
            "--use-theme" => match option_value(&mut index) {
                Some(theme) => args.use_theme = Some(theme),
                None => args.errors.push("--use-theme requires a theme name".into()),
            },
            "-ns" | "--no-skills" => args.no_skills = true,
            "-np" | "--no-prompt-templates" => args.no_prompt_templates = true,
            "--no-themes" => args.no_themes = true,
            "-nc" | "--no-context-files" => args.no_context_files = true,
            "--list-models" => {
                let search = raw.get(index + 1).filter(|next| !next.starts_with('-') && !next.starts_with('@'));
                args.list_models = Some(search.cloned().unwrap_or_default());
                if search.is_some() {
                    index += 1;
                }
            }
            "--tui-mode" => match option_value(&mut index).as_deref() {
                Some("regular") => args.tui_mode = Some(TuiMode::Regular),
                Some("fullscreen") => args.tui_mode = Some(TuiMode::Fullscreen),
                Some(other) => {
                    args.errors.push(format!("Invalid TUI mode \"{other}\". Valid values: regular, fullscreen"))
                }
                None => args.errors.push("--tui-mode requires regular or fullscreen".into()),
            },
            "--verbose" => args.verbose = true,
            "-a" | "--approve" => args.project_trust_override = Some(true),
            "-na" | "--no-approve" => args.project_trust_override = Some(false),
            "--offline" => args.offline = true,
            "--max-turns" | "--max-cost" => {
                if let Some(limit) = value(&mut index, arg, &mut args.errors) {
                    set_run_limit(&mut args, arg, &limit);
                }
            }
            _ if arg.starts_with("--max-turns=") || arg.starts_with("--max-cost=") => {
                let (flag, limit) = arg.split_once('=').unwrap_or_default();
                set_run_limit(&mut args, flag, limit);
            }
            _ if arg.starts_with('@') => args.file_args.push(arg[1..].into()),
            _ if arg.starts_with("--") => {
                let flag = &arg[2..];
                let entry = match flag.split_once('=') {
                    Some((name, value)) => (name.to_string(), FlagValue::String(value.into())),
                    None => match raw.get(index + 1).filter(|next| !next.starts_with('-') && !next.starts_with('@')) {
                        Some(next) => {
                            index += 1;
                            (flag.to_string(), FlagValue::String(next.clone()))
                        }
                        None => (flag.to_string(), FlagValue::Bool),
                    },
                };
                args.unknown_flags.push(entry);
            }
            _ if arg.starts_with('-') => args.errors.push(format!("Unknown option: {arg}")),
            _ => args.messages.push(arg.into()),
        }
        index += 1;
    }
    args
}

/// The error upstream reports for long flags no extension registered.
pub fn unknown_flags_error(flags: &[(String, FlagValue)]) -> Option<String> {
    if flags.is_empty() {
        return None;
    }
    let names: Vec<String> = flags.iter().map(|(name, _)| format!("--{name}")).collect();
    let plural = if names.len() == 1 { "" } else { "s" };
    Some(format!("Unknown option{plural}: {}", names.join(", ")))
}

#[cfg(test)]
#[path = "cli_tests.rs"]
mod tests;
