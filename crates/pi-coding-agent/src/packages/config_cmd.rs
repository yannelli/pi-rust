//! `pi config [-l]`, ported from upstream `handleConfigCommand`. It resolves resources for
//! global and project mode and hands a [`ConfigEditor`] to the TUI selector; without a
//! selector (no terminal) it prints the resource list with its enabled state.

use std::sync::Arc;

use futures::future::BoxFuture;

use super::cli::PackageCommandOptions;
use super::cli_support::{Io, Style, TrustInputs, command_settings, report_settings_errors};
use super::config::{ConfigWriteScope, ScopedResolvedPaths};
use super::config_editor::ConfigEditor;
use super::{PackageManager, PackageManagerOptions};
use crate::resources::brand::{APP_NAME, CONFIG_DIR_NAME};
use crate::settings::{SettingsManager, SettingsOptions};

/// What the `pi config` selector edits; `project_mode_available` enables Tab switching.
pub struct ConfigSelectorRequest {
    pub editor: ConfigEditor,
    pub project_mode_available: bool,
}

/// Runs the interactive selector until it closes.
pub type ConfigSelectorFn = Arc<dyn Fn(ConfigSelectorRequest) -> BoxFuture<'static, ()> + Send + Sync>;

fn usage() -> String {
    format!("{APP_NAME} config [-l] [--approve|--no-approve]")
}

pub(crate) fn config_help() -> String {
    format!(
        "Usage:\n  {}\n\nOpen the resource configuration TUI to enable or disable package resources.\nWithout -l, starts in global settings (~/{CONFIG_DIR_NAME}/agent/settings.json).\nPress Tab in the TUI to switch between global and project-local modes.\n\nOptions:\n  -l, --local       Edit project overrides ({CONFIG_DIR_NAME}/settings.json)\n  -a, --approve     Trust project-local files for this command with -l\n  -na, --no-approve Ignore project-local files for this command with -l\n",
        usage()
    )
}

/// The resource list as text: one block per group with `[x]`, `[ ]`, `[+]`, or `[-]` items.
pub fn render_config_text(editor: &ConfigEditor) -> String {
    let mut lines = Vec::new();
    match editor.write_scope() {
        ConfigWriteScope::Global => {
            lines.push("Global Resources".to_string());
            lines.push(format!("~/{CONFIG_DIR_NAME}/agent/settings.json"));
        }
        ConfigWriteScope::Project => {
            lines.push("Project Local Resources".to_string());
            lines.push(format!("{CONFIG_DIR_NAME}/settings.json"));
        }
    }
    if editor.groups().is_empty() {
        lines.push(String::new());
        lines.push("No resources found.".into());
    }
    for group in editor.groups() {
        lines.push(String::new());
        lines.push(group.label.clone());
        for subgroup in &group.subgroups {
            lines.push(format!("  {}", subgroup.label));
            for item in &subgroup.items {
                let suffix = editor.item_suffix(item);
                let suffix = if suffix.is_empty() { String::new() } else { format!("  {suffix}") };
                lines.push(format!("    {} {}{suffix}", editor.checkbox(item), item.display_name));
            }
        }
    }
    lines.join("\n")
}

/// Run `pi config`; `None` when `args[0]` is not `config`. Returns the exit code.
pub async fn run_config_command(
    args: &[String],
    options: &PackageCommandOptions,
    selector: Option<&ConfigSelectorFn>,
) -> Option<i32> {
    let (command, rest) = args.split_first()?;
    if command != "config" {
        return None;
    }
    let io = Io { out: options.stdout.clone(), err: options.stderr.clone(), style: Style { enabled: options.color } };
    Some(run(rest, options, selector, &io).await)
}

async fn run(rest: &[String], options: &PackageCommandOptions, selector: Option<&ConfigSelectorFn>, io: &Io) -> i32 {
    if rest.iter().any(|arg| arg == "-h" || arg == "--help") {
        io.line(&config_help());
        return 0;
    }
    let (mut local, mut trust_override) = (false, None);
    for arg in rest {
        match arg.as_str() {
            "-l" | "--local" => local = true,
            "-a" | "--approve" => trust_override = Some(true),
            "-na" | "--no-approve" => trust_override = Some(false),
            other => {
                let (message, hint) = if other.starts_with('-') {
                    (
                        format!("Unknown option {other} for \"config\"."),
                        format!("Use \"{APP_NAME} --help\" or \"{}\".", usage()),
                    )
                } else {
                    (format!("Unexpected argument {other}."), format!("Usage: {}", usage()))
                };
                io.error(&io.style.red(&message));
                io.error(&io.style.dim(&hint));
                return 1;
            }
        }
    }
    let (settings, warnings) = command_settings(TrustInputs {
        cwd: &options.cwd,
        agent_dir: &options.agent_dir,
        home: &options.home,
        trust_override,
        saved_only: false,
        has_ui: options.has_ui,
        prompt: options.trust_prompt.as_ref(),
        extensions: options.trust_extensions.as_ref(),
    })
    .await;
    for warning in warnings {
        io.error(&io.style.yellow(&format!("Warning: {warning}")));
    }
    let trusted = settings.is_project_trusted();
    if local && !trusted {
        io.error(&io.style.red("Project is not trusted. Use --approve to modify local resource config."));
        return 1;
    }
    report_settings_errors(&settings, "config command", io);
    let global_settings =
        Arc::new(SettingsManager::create(&options.cwd, &options.agent_dir, SettingsOptions { project_trusted: false }));
    let manager = |settings: Arc<SettingsManager>| {
        let mut manager_options = PackageManagerOptions::new(&options.cwd, &options.agent_dir, settings);
        manager_options.runner = options.runner.clone();
        manager_options.home = options.home.clone();
        manager_options.builtin_extensions = options.builtin_extensions.clone();
        manager_options.http_client = options.http_client.clone();
        manager_options.offline = options.offline;
        PackageManager::new(manager_options)
    };
    let resolved = async {
        let global = manager(global_settings).resolve(None).await?;
        let project = if trusted { manager(settings.clone()).resolve(None).await? } else { global.clone() };
        anyhow::Ok(ScopedResolvedPaths { global, project })
    }
    .await;
    let resolved = match resolved {
        Ok(resolved) => resolved,
        Err(error) => {
            io.error(&io.style.red(&format!("Error: {error}")));
            return 1;
        }
    };
    let scope = if local { ConfigWriteScope::Project } else { ConfigWriteScope::Global };
    let editor = ConfigEditor::new(&resolved, settings, &options.cwd, &options.agent_dir, &options.home, scope);
    match selector {
        Some(selector) => selector(ConfigSelectorRequest { editor, project_mode_available: trusted }).await,
        None => io.line(&render_config_text(&editor)),
    }
    0
}
