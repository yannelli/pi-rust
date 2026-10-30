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

fn extensions_help() -> &'static str {
    "Usage:\n  pi extensions list [-l]\n  pi extensions search [query]\n  pi extensions info <name>\n  pi extensions install <marketplace:name|npm:package|git:url|path> [-l]\n  pi extensions remove <source> [-l]\n  pi extensions enable <name-or-path> [-l]\n  pi extensions disable <name-or-path> [-l]\n\nLists built-in and installed extensions, including disabled ones.\nSearch the marketplace for native Rust extensions.\nUse pi config for the resource configuration TUI. Restart Pi or run /reload after changes.\n"
}

fn render_extensions(editor: &ConfigEditor) -> String {
    let mut lines = vec!["Extensions ([x] enabled, [ ] disabled):".to_string()];
    for item in editor
        .groups()
        .iter()
        .flat_map(|group| &group.subgroups)
        .flat_map(|group| &group.items)
        .filter(|item| item.kind == crate::resources::ResourceType::Extensions)
    {
        lines.push(format!(
            "{} {}  ({})",
            if item.enabled { "[x]" } else { "[ ]" },
            item.path.display(),
            item.metadata.source
        ));
    }
    if lines.len() == 1 {
        lines.push("No extensions found. Install one with pi extensions install <source>.".into());
    }
    lines.join("\n")
}

/// Run `pi config` or `pi extensions`; returns the exit code.
pub async fn run_config_command(
    args: &[String],
    options: &PackageCommandOptions,
    selector: Option<&ConfigSelectorFn>,
) -> Option<i32> {
    let (command, rest) = args.split_first()?;
    if command != "config" && command != "extensions" {
        return None;
    }
    let io = Io { out: options.stdout.clone(), err: options.stderr.clone(), style: Style { enabled: options.color } };
    if command == "extensions" {
        if rest.iter().any(|arg| arg == "--help" || arg == "-h") {
            io.line(extensions_help());
            return Some(0);
        }
        let (action, args) = rest.split_first().map_or(("list", &[][..]), |(action, args)| (action.as_str(), args));
        if matches!(action, "search" | "info") {
            return Some(super::cli_marketplace::run(action, args, options, &io).await);
        }
        if matches!(action, "install" | "remove") {
            let args = std::iter::once(action.to_string()).chain(args.iter().cloned()).collect::<Vec<_>>();
            return super::cli::run_package_command(&args, options).await;
        }
        let mut translated = vec!["--extensions".to_string()];
        match action {
            "list" => {}
            "enable" | "disable" => translated.push(format!("--{action}")),
            _ => {
                io.error(extensions_help());
                return Some(1);
            }
        }
        translated.extend_from_slice(args);
        return Some(run(&translated, options, None, &io).await);
    }
    Some(run(rest, options, selector, &io).await)
}

async fn run(rest: &[String], options: &PackageCommandOptions, selector: Option<&ConfigSelectorFn>, io: &Io) -> i32 {
    if rest.iter().any(|arg| arg == "-h" || arg == "--help") {
        io.line(&config_help());
        return 0;
    }
    let (mut local, mut trust_override) = (false, None);
    let mut extensions_only = false;
    let mut change = None;
    let mut args = rest.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-l" | "--local" => local = true,
            "-a" | "--approve" => trust_override = Some(true),
            "-na" | "--no-approve" => trust_override = Some(false),
            "--extensions" => extensions_only = true,
            "--enable" | "--disable" if extensions_only && change.is_none() => {
                let Some(reference) = args.next().filter(|value| !value.starts_with('-')) else {
                    io.error("Expected an extension name or path after enable/disable.");
                    return 1;
                };
                change = Some((reference.clone(), arg == "--enable"));
            }
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
    let (global_manager, project_manager) = (manager(global_settings), manager(settings.clone()));
    let resolved = async {
        let global = global_manager.resolve(None).await?;
        let project = if trusted { project_manager.resolve(None).await? } else { global.clone() };
        anyhow::Ok(ScopedResolvedPaths { global, project })
    }
    .await;
    let mut failures = global_manager.take_install_failures();
    for failure in project_manager.take_install_failures() {
        if !failures.contains(&failure) {
            failures.push(failure);
        }
    }
    for failure in failures {
        io.error(&io.style.yellow(&format!("Warning: {}", failure.warning())));
    }
    let resolved = match resolved {
        Ok(resolved) => resolved,
        Err(error) => {
            io.error(&io.style.red(&format!("Error: {error}")));
            return 1;
        }
    };
    let scope = if local { ConfigWriteScope::Project } else { ConfigWriteScope::Global };
    let mut editor = ConfigEditor::new(&resolved, settings, &options.cwd, &options.agent_dir, &options.home, scope);
    if let Some((reference, enabled)) = change {
        if let Err(error) = editor.set_extension_enabled(&reference, enabled) {
            io.error(&error);
            return 1;
        }
        let errors = editor.settings().drain_errors();
        if !errors.is_empty() {
            for error in errors {
                io.error(&error.diagnostic());
            }
            return 1;
        }
        io.line(&format!(
            "{} {reference}. Restart Pi or run /reload to apply.",
            if enabled { "Enabled" } else { "Disabled" }
        ));
        return 0;
    }
    if extensions_only {
        io.line(&render_extensions(&editor));
        return 0;
    }
    match selector {
        Some(selector) => selector(ConfigSelectorRequest { editor, project_mode_available: trusted }).await,
        None => io.line(&render_config_text(&editor)),
    }
    0
}
