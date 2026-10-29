//! `pi completion <shell>` (a pi-rust extension): completion scripts for bash, zsh, fish, and
//! PowerShell, rendered from the option table in `cli_table.rs`. The scripts call
//! `pi completion --values models|providers` for `--model` and `--provider` values.

mod posix;
mod shells;

use std::path::Path;

use pi_ai::ModelRegistry;

use crate::cli::CommandOutput;
use crate::cli::table::{CLI_COMMANDS, COMPLETION_SHELLS, CliOption, ValueKind, completable_options};
use crate::config::models_path;
use crate::resources::brand::APP_NAME;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shell {
    Bash,
    Zsh,
    Fish,
    PowerShell,
}

impl Shell {
    pub fn parse(name: &str) -> Option<Self> {
        match name.to_ascii_lowercase().as_str() {
            "bash" => Some(Self::Bash),
            "zsh" => Some(Self::Zsh),
            "fish" => Some(Self::Fish),
            "powershell" | "pwsh" => Some(Self::PowerShell),
            _ => None,
        }
    }
}

pub fn completion_help() -> String {
    let app = APP_NAME;
    format!(
        "Usage:
  {app} completion <bash|zsh|fish|powershell>

Print a shell completion script to stdout. It completes options, subcommands, and file paths.
For --model and --provider it asks {app} for the models of the built-in catalog and models.json.

Install:
  bash        Add to ~/.bashrc:
                eval \"$({app} completion bash)\"
  zsh         Add to ~/.zshrc after compinit:
                eval \"$({app} completion zsh)\"
              Or save it as a completion function on $fpath:
                {app} completion zsh > \"${{fpath[1]}}/_{app}\"
  fish        {app} completion fish > ~/.config/fish/completions/{app}.fish
  powershell  Add to $PROFILE:
                {app} completion powershell | Out-String | Invoke-Expression

Open a new shell afterwards.
"
    )
}

/// The completion script for `shell`.
pub fn completion_script(shell: Shell) -> String {
    match shell {
        Shell::Bash => posix::bash(),
        Shell::Zsh => posix::zsh(),
        Shell::Fish => shells::fish(),
        Shell::PowerShell => shells::powershell(),
    }
}

/// `providers` or `models` (as `provider/id`) from the built-in catalog and `models.json`.
pub fn completion_values(kind: &str, agent_dir: &Path) -> Option<Vec<String>> {
    let (mut registry, _) = ModelRegistry::load_lenient(&models_path(agent_dir));
    let disabled = crate::settings::disabled_providers(agent_dir);
    registry.retain_models(|model| !disabled.contains(&model.provider));
    let mut values: Vec<String> = match kind {
        "providers" => registry.providers(),
        "models" => registry.models().iter().map(|model| format!("{}/{}", model.provider, model.id)).collect(),
        _ => return None,
    };
    values.sort();
    values.dedup();
    Some(values)
}

fn error(message: &str) -> CommandOutput {
    let hint = format!("Use \"{APP_NAME} completion --help\" for usage.");
    CommandOutput { code: 1, stdout: String::new(), stderr: format!("Error: {message}\n{hint}\n") }
}

/// Run `pi completion ...`; `None` when `raw` is another command.
pub fn run_completion_command(raw: &[String], agent_dir: &Path) -> Option<CommandOutput> {
    if raw.first().map(String::as_str) != Some("completion") {
        return None;
    }
    let args: Vec<&str> = raw[1..].iter().map(String::as_str).collect();
    let output = match args.as_slice() {
        [] => error(&format!("Missing shell. Choose one of: {}.", COMPLETION_SHELLS.join(", "))),
        ["-h" | "--help" | "help", ..] => CommandOutput { stdout: completion_help(), ..CommandOutput::default() },
        ["--values", kind] => match completion_values(kind, agent_dir) {
            Some(values) => CommandOutput {
                stdout: values.iter().map(|value| format!("{value}\n")).collect(),
                ..Default::default()
            },
            None => error(&format!("Unknown value list \"{kind}\". Use models or providers.")),
        },
        [name] => match Shell::parse(name) {
            Some(shell) => CommandOutput { stdout: completion_script(shell), ..CommandOutput::default() },
            None => error(&format!("Unknown shell \"{name}\". Choose one of: {}.", COMPLETION_SHELLS.join(", "))),
        },
        [_, extra, ..] => error(&format!("Unexpected argument {extra}.")),
    };
    Some(output)
}

/// The name shell functions use: the app name with characters other than letters, digits, and `_` replaced.
fn ident() -> String {
    APP_NAME.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '_' }).collect()
}

/// Every spelling of `option`: the long flag, then the short one.
fn spellings(option: &CliOption) -> Vec<&'static str> {
    std::iter::once(option.long).chain(option.short).collect()
}

/// Options that take a value, with the value kind.
fn valued_options() -> impl Iterator<Item = &'static CliOption> {
    completable_options().filter(|option| option.takes_value())
}

/// The commands and their words, for scripts that branch on the subcommand.
fn commands() -> impl Iterator<Item = (&'static str, &'static [&'static str])> {
    CLI_COMMANDS.iter().map(|command| (command.name, command.words))
}

/// The list `pi completion --values` serves for a `Model` or `Provider` value.
fn value_list(kind: ValueKind) -> &'static str {
    if kind == ValueKind::Model { "models" } else { "providers" }
}

#[cfg(test)]
mod tests;
