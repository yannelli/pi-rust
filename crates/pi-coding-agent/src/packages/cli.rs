//! `pi install`, `pi remove` (`pi uninstall`), `pi update`, and `pi list`, ported from
//! upstream `package-manager-cli.ts`.

use std::path::PathBuf;
use std::sync::Arc;

use super::cli_parse::{Command, Parsed, Problem, UpdateTarget, help_text, parse_package_command};
use super::cli_support::{
    CommandOutput, Io, ModelsUpdateFn, SelfUpdateFn, SelfUpdateRequest, Style, TrustExtensionsFn, TrustInputs,
    TrustPromptFn, command_settings, prompt_trust_on_terminal, report_settings_errors,
};
use super::runner::{CommandRunner, ProcessRunner};
use super::{PackageManager, PackageManagerOptions, ProgressKind};
use crate::resources::SourceScope;
use crate::resources::brand::APP_NAME;

/// Environment and callbacks of the package commands. The hub wires `models_update`,
/// `self_update`, and the trust callbacks; tests pass a mock `runner`.
#[derive(Clone)]
pub struct PackageCommandOptions {
    pub cwd: PathBuf,
    pub agent_dir: PathBuf,
    pub home: PathBuf,
    pub runner: Arc<dyn CommandRunner>,
    pub http_client: Option<reqwest::Client>,
    pub offline: Option<bool>,
    pub builtin_extensions: Vec<String>,
    /// Interactive terminal: the trust prompt may be shown.
    pub has_ui: bool,
    pub color: bool,
    pub models_update: Option<ModelsUpdateFn>,
    pub self_update: Option<SelfUpdateFn>,
    pub trust_prompt: Option<TrustPromptFn>,
    pub trust_extensions: Option<TrustExtensionsFn>,
    pub stdout: CommandOutput,
    pub stderr: CommandOutput,
}

impl PackageCommandOptions {
    pub fn new(cwd: PathBuf, agent_dir: PathBuf) -> Self {
        Self {
            cwd,
            agent_dir,
            home: crate::resources::paths::home_dir(),
            runner: Arc::new(ProcessRunner::default()),
            http_client: None,
            offline: None,
            builtin_extensions: Vec::new(),
            has_ui: false,
            color: false,
            models_update: None,
            self_update: None,
            trust_prompt: None,
            trust_extensions: None,
            stdout: CommandOutput::stdout(),
            stderr: CommandOutput::stderr(),
        }
    }

    /// Options for the real terminal: color and the trust prompt when stdin and stdout are
    /// terminals (`NO_COLOR` turns color off).
    pub fn from_env(cwd: PathBuf, agent_dir: PathBuf) -> Self {
        use std::io::IsTerminal;
        let mut options = Self::new(cwd, agent_dir);
        let stdout_tty = std::io::stdout().is_terminal();
        options.has_ui = stdout_tty && std::io::stdin().is_terminal();
        options.color = stdout_tty && std::env::var_os("NO_COLOR").is_none();
        if options.has_ui {
            options.trust_prompt = Some(Arc::new(|prompt| {
                Box::pin(async move {
                    let choice = tokio::task::spawn_blocking(move || prompt_trust_on_terminal(&prompt)).await;
                    choice.ok().flatten()
                })
            }));
        }
        options
    }
}

fn usage_error(io: &Io, message: &str, hint: String) -> i32 {
    let (message, hint) = (io.style.red(message), io.style.dim(&hint));
    io.error(&message);
    io.error(&hint);
    1
}

/// Run a package subcommand; `None` when `args[0]` is not one. Returns the exit code.
pub async fn run_package_command(args: &[String], options: &PackageCommandOptions) -> Option<i32> {
    let parsed = parse_package_command(args)?;
    let io = Io { out: options.stdout.clone(), err: options.stderr.clone(), style: Style { enabled: options.color } };
    Some(run_parsed(parsed, options, &io).await)
}

async fn run_parsed(parsed: Parsed, options: &PackageCommandOptions, io: &Io) -> i32 {
    let command = parsed.command;
    if parsed.help {
        io.line(&help_text(command).replacen("Usage:", &io.style.bold("Usage:"), 1));
        return 0;
    }
    let usage = command.usage();
    match &parsed.problem {
        Some(Problem::InvalidOption(option)) => {
            let message = format!("Unknown option {option} for \"{}\".", command.name());
            return usage_error(io, &message, format!("Use \"{APP_NAME} --help\" or \"{usage}\"."));
        }
        Some(Problem::MissingValue(option)) => {
            return usage_error(io, &format!("Missing value for {option}."), format!("Usage: {usage}"));
        }
        Some(Problem::InvalidArgument(arg)) => {
            return usage_error(io, &format!("Unexpected argument {arg}."), format!("Usage: {usage}"));
        }
        Some(Problem::Conflict(message)) => return usage_error(io, message, format!("Usage: {usage}")),
        None => {}
    }
    if matches!(command, Command::Install | Command::Remove) && parsed.source.is_none() {
        return usage_error(io, &format!("Missing {} source.", command.name()), format!("Usage: {usage}"));
    }
    if parsed.update_target == Some(UpdateTarget::Models) {
        return run_models_update(options, io).await;
    }
    let writes_project = matches!(command, Command::Install | Command::Remove) && parsed.local;
    let (settings, warnings) = command_settings(TrustInputs {
        cwd: &options.cwd,
        agent_dir: &options.agent_dir,
        home: &options.home,
        trust_override: parsed.trust_override,
        saved_only: command == Command::Update,
        has_ui: options.has_ui,
        prompt: options.trust_prompt.as_ref(),
        extensions: options.trust_extensions.as_ref(),
    })
    .await;
    for warning in warnings {
        let text = io.style.yellow(&format!("Warning: {warning}"));
        io.error(&text);
    }
    if writes_project && !settings.is_project_trusted() {
        let text = io.style.red("Project is not trusted. Use --approve to modify local package config.");
        io.error(&text);
        return 1;
    }
    report_settings_errors(&settings, "package command", io);
    let manager = PackageManager::new(PackageManagerOptions {
        cwd: options.cwd.clone(),
        agent_dir: options.agent_dir.clone(),
        home: options.home.clone(),
        settings: settings.clone(),
        runner: options.runner.clone(),
        builtin_extensions: options.builtin_extensions.clone(),
        http_client: options.http_client.clone(),
        offline: options.offline,
    });
    let (out, style) = (io.out.clone(), io.style);
    manager.set_progress_callback(Some(Arc::new(move |event| {
        if event.kind == ProgressKind::Start
            && let Some(message) = &event.message
        {
            out.line(&style.dim(message));
        }
    })));
    match run_command(&parsed, &manager, options, io).await {
        Ok(code) => code,
        Err(error) => {
            let text = io.style.red(&format!("Error: {error}"));
            io.error(&text);
            1
        }
    }
}

async fn run_models_update(options: &PackageCommandOptions, io: &Io) -> i32 {
    let Some(update) = &options.models_update else {
        let text = io.style.red("Error: Model catalog refresh is not available in this build.");
        io.error(&text);
        return 1;
    };
    match update(options.agent_dir.clone()).await {
        Ok(()) => {
            let text = io.style.green("Model catalogs refreshed");
            io.line(&text);
            0
        }
        Err(error) => {
            let text = io.style.red(&format!("Error: {error}"));
            io.error(&text);
            1
        }
    }
}

async fn run_command(
    parsed: &Parsed,
    manager: &PackageManager,
    options: &PackageCommandOptions,
    io: &Io,
) -> anyhow::Result<i32> {
    let source = parsed.source.clone().unwrap_or_default();
    match parsed.command {
        Command::Install => {
            manager.install_and_persist(&source, parsed.local).await?;
            let text = io.style.green(&format!("Installed {source}"));
            io.line(&text);
        }
        Command::Remove => {
            let removed = manager.remove_and_persist(&source, parsed.local).await?;
            if !removed {
                let text = io.style.red(&format!("No matching package found for {source}"));
                io.error(&text);
                return Ok(1);
            }
            let text = io.style.green(&format!("Removed {source}"));
            io.line(&text);
        }
        Command::List => print_list(manager, io).await,
        Command::Update => return run_update(parsed, manager, options, io).await,
    }
    Ok(0)
}

async fn print_list(manager: &PackageManager, io: &Io) {
    let packages = manager.list_configured_packages().await;
    if packages.is_empty() {
        let text = io.style.dim("No packages installed.");
        io.line(&text);
        return;
    }
    let mut printed_user = false;
    for (scope, heading) in [(SourceScope::User, "User packages:"), (SourceScope::Project, "Project packages:")] {
        let group: Vec<_> = packages.iter().filter(|package| package.scope == scope).collect();
        if group.is_empty() {
            continue;
        }
        if scope == SourceScope::Project && printed_user {
            io.line("");
        }
        printed_user = true;
        let heading = io.style.bold(heading);
        io.line(&heading);
        for package in group {
            let display =
                if package.filtered { format!("{} (filtered)", package.source) } else { package.source.clone() };
            io.line(&format!("  {display}"));
            if let Some(path) = &package.installed_path {
                let text = io.style.dim(&format!("    {}", path.display()));
                io.line(&text);
            }
        }
    }
}

async fn run_update(
    parsed: &Parsed,
    manager: &PackageManager,
    options: &PackageCommandOptions,
    io: &Io,
) -> anyhow::Result<i32> {
    let target = parsed.update_target.clone().unwrap_or(UpdateTarget::SelfOnly);
    if parsed.show_extensions_skipped_note {
        let text =
            io.style.dim(&format!("Extensions are skipped. Run {APP_NAME} update --extensions to update extensions."));
        io.line(&text);
    }
    if let UpdateTarget::All | UpdateTarget::Extensions(_) = &target {
        let source = match &target {
            UpdateTarget::Extensions(source) => source.clone(),
            _ => None,
        };
        manager.update(source.as_deref()).await?;
        let text = match &source {
            Some(source) => io.style.green(&format!("Updated {source}")),
            None => io.style.green("Updated packages"),
        };
        io.line(&text);
    }
    if !matches!(target, UpdateTarget::All | UpdateTarget::SelfOnly) {
        return Ok(0);
    }
    let Some(self_update) = &options.self_update else {
        io.error(&format!("error: {APP_NAME} cannot self-update this installation."));
        return Ok(1);
    };
    let npm_command = manager.settings().global_settings().npm_command;
    let request = SelfUpdateRequest {
        force: parsed.force,
        agent_dir: options.agent_dir.clone(),
        package_dir: super::package_dir(),
        npm_command,
    };
    Ok(self_update(request).await)
}
