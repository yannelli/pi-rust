use std::io::{IsTerminal, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use pi_ai::auth::OAuthLookup;
use pi_ai::env::{process_env_value, set_child_process_env};
use pi_coding_agent::acp::{AcpModeOptions, run_acp_mode};
use pi_coding_agent::agent_session::auth::configure_http_from_settings;
use pi_coding_agent::agent_session::{
    BUILTIN_EXTENSIONS, CreateServicesOptions, DiagnosticLevel, RuntimeDiagnostic, create_agent_session_services,
};
use pi_coding_agent::app::{AppOptions, create_app_runtime, format_no_models_available_message};
use pi_coding_agent::auth_cli::run_auth_command;
use pi_coding_agent::cli::{Args, OutputMode, help_text, normalize_session_name, parse_args, unknown_flags_error};
use pi_coding_agent::config::{agent_dir, env_flag, expand_tilde};
use pi_coding_agent::diagnostics::{print_timings, record_fatal_error, take_over_stdout, time};
use pi_coding_agent::export::{ExportTheme, export_from_file};
use pi_coding_agent::files::process_cli_file_args;
use pi_coding_agent::interactive::startup_ui::prompt_project_trust;
use pi_coding_agent::interactive::{InteractiveOptions, run_interactive};
use pi_coding_agent::interactive_components::first_time_setup::should_run_first_time_setup;
use pi_coding_agent::mcp_cli::run_mcp_cli;
use pi_coding_agent::packages::{PackageCommandOptions, run_config_command, run_package_command};
use pi_coding_agent::print_mode::run_print_mode;
use pi_coding_agent::rpc::{RpcModeOptions, run_rpc_mode};
use pi_coding_agent::session::branching::missing_session_cwd;
use pi_coding_agent::session::ids::assert_valid_session_id;
use pi_coding_agent::session::{
    NewSessionOptions, ResolvedSession, SessionManager, listing::resolve_session_arg, session_dir,
};
use pi_coding_agent::settings::{SettingsManager, SettingsOptions};
use pi_coding_agent::startup::{
    auto_trust_on_reload_cwd, install_update_commands, prepare_process, run_startup_benchmark,
    startup_benchmark_enabled, startup_changelog_once,
};
use pi_coding_agent::startup_dialogs::{self as dialogs, SessionPick};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AppMode {
    Interactive,
    Print,
    Json,
    Rpc,
}

/// Upstream `resolveAppMode`: print mode when stdin or stdout is not a terminal.
fn app_mode(args: &Args, stdin_is_tty: bool, stdout_is_tty: bool) -> AppMode {
    match args.mode {
        Some(OutputMode::Rpc) => AppMode::Rpc,
        Some(OutputMode::Json) => AppMode::Json,
        _ if args.print || !stdin_is_tty || !stdout_is_tty => AppMode::Print,
        _ => AppMode::Interactive,
    }
}

/// OAuth providers for token refresh; the OAuth stream supplies the built-in table.
fn oauth_lookup() -> OAuthLookup {
    Arc::new(pi_ai::auth::oauth::builtin_oauth)
}

/// `200000` as `200K`, `1000000` as `1M`, `1500000` as `1.5M`.
fn format_token_count(count: u64) -> String {
    let scaled = |value: f64, unit: &str| {
        if value.fract() == 0.0 { format!("{value}{unit}") } else { format!("{value:.1}{unit}") }
    };
    match count {
        1_000_000.. => scaled(count as f64 / 1_000_000.0, "M"),
        1_000.. => scaled(count as f64 / 1_000.0, "K"),
        _ => count.to_string(),
    }
}

/// Whether every word of `query` appears in `text` as an ordered run of characters.
fn fuzzy_matches(text: &str, query: &str) -> bool {
    let text = text.to_lowercase();
    query.to_lowercase().split_whitespace().all(|word| {
        let mut chars = text.chars();
        word.chars().all(|wanted| chars.any(|c| c == wanted))
    })
}

/// `--list-models [search]`: models with configured auth, as an aligned table. The session
/// services supply them, so cached catalogs and llama.cpp models appear too.
async fn list_models(search: &str, cwd: &Path, agent_dir: &Path) {
    let services = create_agent_session_services(CreateServicesOptions::new(cwd, agent_dir)).await;
    if let Some(error) = services.model_runtime.load_error() {
        eprintln!("Warning: errors loading models.json:\n{error}");
    }
    let mut models = services.model_runtime.available_models();
    if models.is_empty() {
        println!("{}", format_no_models_available_message());
        return;
    }
    if !search.is_empty() {
        models.retain(|model| fuzzy_matches(&format!("{} {}", model.provider, model.id), search));
        if models.is_empty() {
            println!("No models matching \"{search}\"");
            return;
        }
    }
    models.sort_by(|a, b| (&a.provider, &a.id).cmp(&(&b.provider, &b.id)));
    let yes_no = |flag: bool| if flag { "yes" } else { "no" }.to_string();
    let mut rows = vec![["provider", "model", "context", "max-out", "thinking", "images"].map(String::from)];
    rows.extend(models.iter().map(|model| {
        [
            model.provider.clone(),
            model.id.clone(),
            format_token_count(model.context_window),
            format_token_count(model.max_tokens),
            yes_no(model.reasoning),
            yes_no(model.input.iter().any(|input| input == "image")),
        ]
    }));
    let widths: Vec<usize> =
        (0..6).map(|column| rows.iter().map(|row| row[column].chars().count()).max().unwrap_or(0)).collect();
    for row in rows {
        let cells: Vec<String> = row.iter().zip(&widths).map(|(cell, width)| format!("{cell:<width$}")).collect();
        println!("{}", cells.join("  "));
    }
}

/// Print `question` and read one trimmed line from the terminal.
fn ask(question: &str) -> anyhow::Result<String> {
    print!("{question}");
    std::io::Write::flush(&mut std::io::stdout())?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    Ok(answer.trim().to_string())
}

/// A startup failure already reported to the user, with the exit code.
#[derive(Debug)]
struct Exit(i32);

impl std::fmt::Display for Exit {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "exit {}", self.0)
    }
}

impl std::error::Error for Exit {}

fn fail(message: impl std::fmt::Display) -> Exit {
    eprintln!("Error: {message}");
    Exit(1)
}

/// Upstream `validateForkFlags` and `validateSessionIdFlags`.
fn validate_session_flags(args: &Args) -> Result<(), Exit> {
    fn conflicts(flags: &[(bool, &'static str)]) -> Vec<&'static str> {
        flags.iter().filter(|(set, _)| *set).map(|(_, flag)| *flag).collect()
    }
    if args.fork.is_some() {
        let found = conflicts(&[
            (args.session.is_some(), "--session"),
            (args.continue_session, "--continue"),
            (args.resume, "--resume"),
            (args.no_session, "--no-session"),
        ]);
        if !found.is_empty() {
            return Err(fail(format!("--fork cannot be combined with {}", found.join(", "))));
        }
    }
    if let Some(id) = &args.session_id {
        let found = conflicts(&[
            (args.session.is_some(), "--session"),
            (args.continue_session, "--continue"),
            (args.resume, "--resume"),
        ]);
        if !found.is_empty() {
            return Err(fail(format!("--session-id cannot be combined with {}", found.join(", "))));
        }
        assert_valid_session_id(id).map_err(fail)?;
    }
    Ok(())
}

struct SessionPlace<'a> {
    cwd: &'a Path,
    agent_dir: &'a Path,
    /// `--session-dir`, `PI_CODING_AGENT_SESSION_DIR`, or the `sessionDir` setting.
    dir_override: Option<&'a Path>,
    interactive: bool,
    settings: &'a SettingsManager,
}

impl SessionPlace<'_> {
    fn dir(&self) -> PathBuf {
        self.dir_override.map(Path::to_path_buf).unwrap_or_else(|| session_dir(self.agent_dir, self.cwd))
    }

    fn fork(&self, source: &Path, id: Option<String>) -> Result<SessionManager, Exit> {
        let options = NewSessionOptions { id, parent_session: None };
        SessionManager::fork_from(source, self.cwd, self.dir_override, self.agent_dir, options).map_err(fail)
    }
}

/// Upstream `createSessionManager`: the session the run starts with.
async fn create_session_manager(args: &Args, place: &SessionPlace<'_>) -> Result<SessionManager, Exit> {
    let id = args.session_id.clone();
    if args.no_session {
        return SessionManager::create_with(place.cwd, None, NewSessionOptions { id, parent_session: None })
            .map_err(fail);
    }
    if let Some(fork) = &args.fork {
        if let Some(id) = &id
            && SessionManager::find_by_id(place.cwd, id, place.dir_override, place.agent_dir).is_some()
        {
            return Err(fail(format!("Session already exists with id '{id}'")));
        }
        return match resolve_session_arg(fork, place.cwd, place.dir_override, place.agent_dir) {
            ResolvedSession::Path(path) | ResolvedSession::Local(path) | ResolvedSession::Global { path, .. } => {
                place.fork(&path, id)
            }
            ResolvedSession::NotFound(arg) => Err(fail(format!("No session found matching '{arg}'"))),
        };
    }
    if let Some(session) = &args.session {
        return match resolve_session_arg(session, place.cwd, place.dir_override, place.agent_dir) {
            ResolvedSession::Path(path) | ResolvedSession::Local(path) => {
                SessionManager::open_with(&path, place.dir_override, None).map_err(fail)
            }
            ResolvedSession::Global { path, cwd } => {
                println!("Session found in different project: {cwd}");
                let answer = if place.interactive {
                    ask("Fork this session into current directory? [y/N] ")
                } else {
                    Ok(String::new())
                };
                if !answer.is_ok_and(|answer| answer.eq_ignore_ascii_case("y") || answer.eq_ignore_ascii_case("yes")) {
                    println!("Aborted.");
                    return Err(Exit(0));
                }
                place.fork(&path, None)
            }
            ResolvedSession::NotFound(arg) => Err(fail(format!("No session found matching '{arg}'"))),
        };
    }
    let dir = place.dir();
    if args.resume && place.interactive {
        let terminal = dialogs::process_terminal();
        let pick = dialogs::select_session_on(terminal, place.settings, place.cwd, place.dir_override, place.agent_dir);
        return match tokio::task::LocalSet::new().run_until(pick).await {
            SessionPick::Selected(path) => SessionManager::open_with(&path, place.dir_override, None).map_err(fail),
            SessionPick::Exit => Err(Exit(0)),
            SessionPick::Cancelled => {
                println!("\x1b[2mNo session selected\x1b[22m");
                Err(Exit(0))
            }
        };
    }
    if args.continue_session {
        return SessionManager::continue_recent(place.cwd, &dir, place.agent_dir).map_err(fail);
    }
    if let Some(id) = &id {
        if let Some(path) = SessionManager::find_by_id(place.cwd, id, place.dir_override, place.agent_dir) {
            return SessionManager::open_with(&path, place.dir_override, None).map_err(fail);
        }
        eprintln!("Warning: No project session found with id '{id}'; creating a new session with that id.");
    }
    SessionManager::create_with(place.cwd, Some(&dir), NewSessionOptions { id, parent_session: None }).map_err(fail)
}

/// A stored session whose cwd is gone continues in the current directory after confirmation.
async fn check_session_cwd(session: SessionManager, place: &SessionPlace<'_>) -> Result<SessionManager, Exit> {
    let Some(missing) = missing_session_cwd(&session, place.cwd) else { return Ok(session) };
    if !place.interactive {
        return Err(fail(missing.error_message()));
    }
    let prompt = missing.prompt();
    let confirm = dialogs::confirm_missing_session_cwd_on(dialogs::process_terminal(), place.settings, &prompt);
    if !tokio::task::LocalSet::new().run_until(confirm).await {
        return Err(Exit(0));
    }
    let file = missing.session_file.unwrap_or_default();
    SessionManager::open_with(&file, place.dir_override, Some(&missing.fallback_cwd)).map_err(fail)
}

fn report(diagnostics: &[RuntimeDiagnostic]) {
    for diagnostic in diagnostics {
        match diagnostic.level {
            DiagnosticLevel::Error => eprintln!("Error: {}", diagnostic.message),
            DiagnosticLevel::Warning => eprintln!("Warning: {}", diagnostic.message),
            DiagnosticLevel::Info => eprintln!("{}", diagnostic.message),
        }
    }
}

async fn run(raw: Vec<String>) -> anyhow::Result<i32> {
    start(raw).await.or_else(|error| error.downcast::<Exit>().map(|exit| exit.0))
}

async fn start(raw: Vec<String>) -> anyhow::Result<i32> {
    set_child_process_env("PI_CODING_AGENT", "true");
    set_child_process_env("AI_AGENT", "pi");
    if raw.iter().any(|arg| arg == "--offline") || env_flag("PI_OFFLINE") {
        set_child_process_env("PI_OFFLINE", "1");
        set_child_process_env("PI_SKIP_VERSION_CHECK", "1");
    }
    let agent_dir = agent_dir();
    let crash_context = prepare_process(&agent_dir);
    if let Some(output) = run_auth_command(&raw, &agent_dir, oauth_lookup()).await {
        print!("{}", output.stdout);
        eprint!("{}", output.stderr);
        return Ok(output.code);
    }
    let cwd = std::env::current_dir()?;
    let bootstrap_settings = SettingsManager::create(&cwd, &agent_dir, SettingsOptions { project_trusted: false });
    configure_http_from_settings(&bootstrap_settings);
    let mut package_options = PackageCommandOptions::from_env(cwd.clone(), agent_dir.clone());
    package_options.builtin_extensions = BUILTIN_EXTENSIONS.iter().map(|name| name.to_string()).collect();
    install_update_commands(&mut package_options);
    if let Some(code) = run_package_command(&raw, &package_options).await {
        return Ok(code);
    }
    let config_selector = (std::io::stdin().is_terminal() && std::io::stdout().is_terminal())
        .then(pi_coding_agent::interactive_components::config_selector::config_selector_fn);
    if let Some(code) = run_config_command(&raw, &package_options, config_selector.as_ref()).await {
        return Ok(code);
    }
    if let Some(code) = run_mcp_cli(&raw, &cwd, &agent_dir).await {
        return Ok(code);
    }
    let mut args = parse_args(&raw);
    time("parseArgs");
    for warning in &args.warnings {
        eprintln!("Warning: {warning}");
    }
    if !args.errors.is_empty() {
        for error in &args.errors {
            eprintln!("Error: {error}");
        }
        return Ok(1);
    }
    if args.version {
        println!("{}", env!("CARGO_PKG_VERSION"));
        return Ok(0);
    }
    let startup_settings = SettingsManager::create(
        &cwd,
        &agent_dir,
        SettingsOptions { project_trusted: args.project_trust_override.unwrap_or(true) },
    );
    configure_http_from_settings(&startup_settings);
    if let Some(input) = &args.export {
        let theme =
            startup_settings.theme().and_then(|name| ExportTheme::builtin(&name)).unwrap_or_else(ExportTheme::dark);
        return match export_from_file(input, args.messages.first().map(String::as_str), &theme) {
            Ok(path) => {
                println!("Exported to: {}", path.display());
                Ok(0)
            }
            Err(error) => Err(fail(format!("{error:#}")).into()),
        };
    }
    if args.help {
        println!("{}", help_text());
        return Ok(0);
    }
    if let Some(search) = &args.list_models {
        list_models(search, &cwd, &agent_dir).await;
        return Ok(0);
    }
    if let Some(error) = unknown_flags_error(&args.unknown_flags) {
        eprintln!("Error: {error}");
        return Ok(1);
    }
    if args.terminal_login {
        args.mode = None;
    } else if args.mode == Some(OutputMode::Acp) {
        return Ok(run_acp_mode(AcpModeOptions::from_args(&args, &cwd, &agent_dir, oauth_lookup())).await);
    }
    let mut mode = app_mode(&args, std::io::stdin().is_terminal(), std::io::stdout().is_terminal());
    if mode == AppMode::Rpc {
        take_over_stdout()?;
        if !args.file_args.is_empty() {
            eprintln!("Error: @file arguments are not supported in RPC mode");
            return Ok(1);
        }
    }
    if matches!(mode, AppMode::Json | AppMode::Print) {
        take_over_stdout()?;
    }
    validate_session_flags(&args)?;
    let migrations = pi_coding_agent::migrations::run_migrations(&cwd, &agent_dir);
    time("runMigrations");
    if mode == AppMode::Interactive && should_run_first_time_setup(&pi_coding_agent::config::settings_path(&agent_dir))
    {
        tokio::task::LocalSet::new().run_until(dialogs::show_first_time_setup(&startup_settings)).await;
        time("firstTimeSetup");
    }
    if let (AppMode::Interactive, Some(theme)) = (mode, &args.use_theme) {
        startup_settings.apply_overrides(serde_json::json!({ "theme": theme }));
    }
    if startup_benchmark_enabled() && mode != AppMode::Interactive {
        return Err(fail("PI_STARTUP_BENCHMARK only supports interactive mode").into());
    }
    let startup_diagnostics: Vec<RuntimeDiagnostic> =
        startup_settings.drain_errors().iter().map(|error| RuntimeDiagnostic::warning(error.diagnostic())).collect();

    let mut stdin_content = None;
    if mode != AppMode::Rpc && !std::io::stdin().is_terminal() {
        let mut piped = String::new();
        std::io::stdin().read_to_string(&mut piped)?;
        stdin_content = Some(piped.trim().to_string()).filter(|text| !text.is_empty());
        if stdin_content.is_some() && mode == AppMode::Interactive {
            mode = AppMode::Print;
        }
    }
    time("readPipedStdin");
    let files = process_cli_file_args(&args.file_args, &cwd)?;
    time("prepareInitialMessage");
    let mut parts: Vec<String> = stdin_content.into_iter().collect();
    if !files.text.is_empty() {
        parts.push(files.text.clone());
    }
    if !args.messages.is_empty() {
        parts.push(args.messages.remove(0));
    }
    let initial_message = (!parts.is_empty()).then(|| parts.concat());

    let env_session_dir =
        process_env_value(&pi_coding_agent::resources::brand::session_dir_env_var()).filter(|dir| !dir.is_empty());
    let dir_override = args
        .session_dir
        .as_deref()
        .or(env_session_dir.as_deref())
        .map(expand_tilde)
        .or_else(|| startup_settings.session_dir());
    let interactive = mode == AppMode::Interactive && std::io::stdin().is_terminal();
    let settings = &startup_settings;
    let place =
        SessionPlace { cwd: &cwd, agent_dir: &agent_dir, dir_override: dir_override.as_deref(), interactive, settings };
    let mut session_manager = check_session_cwd(create_session_manager(&args, &place).await?, &place).await?;
    if let Some(name) = &args.name {
        let Some(name) = normalize_session_name(name) else {
            return Err(fail("--name requires a non-empty value").into());
        };
        session_manager.append_session_info(&name)?;
    }

    let mut options = AppOptions { oauth: Some(oauth_lookup()), ..AppOptions::from_args(&args, &cwd, &agent_dir) };
    time("createSessionManager");
    let home = pi_coding_agent::config::home_dir();
    let auto_trust_cwd = auto_trust_on_reload_cwd(session_manager.cwd(), args.project_trust_override, &home);
    if interactive {
        let session_cwd = session_manager.cwd().to_path_buf();
        let local = tokio::task::LocalSet::new();
        let prompt = prompt_project_trust(&session_cwd, &agent_dir, args.project_trust_override);
        if let Some(trusted) = local.run_until(prompt).await {
            options.project_trust_decisions.push((session_cwd, trusted));
        }
    }
    time("createRuntime");
    let runtime = create_app_runtime(options, session_manager).await?;
    let session = runtime.session();
    time("createAgentSessionRuntime");
    crash_context.set_session_file(session.session_file().as_deref());
    configure_http_from_settings(session.settings());
    let runtime_diagnostics: Vec<RuntimeDiagnostic> =
        runtime.diagnostics().into_iter().filter(|diagnostic| !startup_diagnostics.contains(diagnostic)).collect();
    let has_errors = runtime_diagnostics.iter().any(|diagnostic| diagnostic.level == DiagnosticLevel::Error);
    if mode != AppMode::Interactive || has_errors {
        report(&startup_diagnostics);
        report(&runtime_diagnostics);
    }
    if has_errors {
        runtime.dispose().await;
        return Ok(1);
    }
    time("initTheme");
    if mode == AppMode::Interactive && !migrations.deprecation_warnings.is_empty() {
        print!("{}", pi_coding_agent::migrations::deprecation_warning_text(&migrations.deprecation_warnings));
        tokio::task::LocalSet::new().run_until(dialogs::wait_for_any_key_on(dialogs::process_terminal())).await;
        println!();
    }
    time("resolveModelScope");
    time("createAgentSession");
    if mode != AppMode::Interactive && session.model().is_none() {
        eprintln!("Error: {}", format_no_models_available_message());
        runtime.dispose().await;
        return Ok(1);
    }

    print_timings();
    match mode {
        AppMode::Rpc => {
            // Upstream refreshes catalogs in the background for RPC; interactive mode starts its own refresh.
            session.model_runtime().refresh_in_background();
            Ok(run_rpc_mode(Arc::new(runtime), RpcModeOptions::default()).await)
        }
        AppMode::Print | AppMode::Json => {
            let output = if mode == AppMode::Json { OutputMode::Json } else { OutputMode::Text };
            Ok(run_print_mode(&runtime, output, initial_message, files.images, args.messages).await)
        }
        _ => {
            session.model_runtime().refresh_in_background();
            let changelog = startup_changelog_once(session.settings(), !session.messages().is_empty());
            let mut diagnostics = startup_diagnostics;
            diagnostics.extend(runtime_diagnostics);
            if !migrations.migrated_auth_providers.is_empty() {
                let providers = migrations.migrated_auth_providers.join(", ");
                diagnostics.push(RuntimeDiagnostic::warning(format!("Migrated credentials to auth.json: {providers}")));
            }
            let options = InteractiveOptions {
                initial_message,
                initial_images: files.images,
                initial_messages: args.messages,
                verbose: args.verbose,
                tui_mode: args.tui_mode,
                initial_theme_setting: args.use_theme.clone(),
                startup_diagnostics: diagnostics,
                model_fallback_message: runtime.model_fallback_message(),
                changelog_markdown: changelog.markdown,
                agent_dir: Some(agent_dir.clone()),
                auto_trust_on_reload_cwd: auto_trust_cwd,
                ..InteractiveOptions::default()
            };
            if startup_benchmark_enabled() {
                run_startup_benchmark(Arc::new(runtime), options).await?;
                return Ok(0);
            }
            if let Err(error) = run_interactive(Arc::new(runtime), options).await {
                eprintln!("Error: {error:#}");
                let crashes = pi_coding_agent::diagnostics::crash_log_path(&agent_dir);
                if let Some(instructions) = record_fatal_error(&crashes, &crash_context, &error) {
                    eprintln!("\n{instructions}");
                }
                return Ok(1);
            }
            Ok(0)
        }
    }
}

#[tokio::main]
async fn main() {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let code = match run(raw).await {
        Ok(code) => code,
        Err(error) => {
            eprintln!("Error: {error:#}");
            1
        }
    };
    std::process::exit(code);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chooses_print_mode_when_a_stream_is_redirected() {
        let args = Args::default();
        assert_eq!(app_mode(&args, true, true), AppMode::Interactive);
        assert_eq!(app_mode(&args, false, true), AppMode::Print);
        assert_eq!(app_mode(&args, true, false), AppMode::Print);
        let json = Args { mode: Some(OutputMode::Json), ..Args::default() };
        assert_eq!(app_mode(&json, true, true), AppMode::Json);
    }

    #[test]
    fn formats_token_counts_and_fuzzy_search() {
        assert_eq!(
            [200_000, 1_000_000, 1_500_000, 999, 32_768].map(format_token_count),
            ["200K", "1M", "1.5M", "999", "32.8K"]
        );
        assert!(fuzzy_matches("anthropic claude-sonnet-4-5", "sonnet anth"));
        assert!(fuzzy_matches("openai gpt-5", "gpt5"));
        assert!(!fuzzy_matches("openai gpt-5", "claude"));
    }
}
