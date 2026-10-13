use std::io::{IsTerminal, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use pi_ai::ModelRegistry;
use pi_ai::auth::{AuthResolver, FileCredentialStore, OAuthLookup};
use pi_ai::env::{process_env_value, set_child_process_env};
use pi_coding_agent::agent_session::auth::configure_http_from_settings;
use pi_coding_agent::agent_session::{BUILTIN_EXTENSIONS, DiagnosticLevel, RuntimeDiagnostic};
use pi_coding_agent::app::{AppOptions, configured_providers, create_app_runtime, format_no_models_available_message};
use pi_coding_agent::auth_cli::run_auth_command;
use pi_coding_agent::cli::{Args, OutputMode, help_text, normalize_session_name, parse_args, unknown_flags_error};
use pi_coding_agent::config::{ENV_SESSION_DIR, agent_dir, auth_path, env_flag, expand_tilde, models_path};
use pi_coding_agent::diagnostics::take_over_stdout;
use pi_coding_agent::export::{ExportTheme, export_from_file};
use pi_coding_agent::files::process_file_args;
use pi_coding_agent::interactive::run_interactive;
use pi_coding_agent::mcp_cli::run_mcp_cli;
use pi_coding_agent::packages::{PackageCommandOptions, run_config_command, run_package_command};
use pi_coding_agent::print_mode::run_print_mode;
use pi_coding_agent::rpc::{RpcModeOptions, run_rpc_mode};
use pi_coding_agent::session::branching::missing_session_cwd;
use pi_coding_agent::session::ids::assert_valid_session_id;
use pi_coding_agent::session::{
    NewSessionOptions, ResolvedSession, SessionManager, list_sessions, listing::resolve_session_arg, session_dir,
};
use pi_coding_agent::settings::{SettingsManager, SettingsOptions};

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

/// `--list-models [search]`: models with configured auth, as an aligned table.
async fn list_models(search: &str, agent_dir: &Path) {
    let (mut registry, error) = ModelRegistry::load_lenient(&models_path(agent_dir));
    if let Some(error) = error {
        eprintln!("Warning: errors loading models.json:\n{error}");
    }
    let resolver = AuthResolver::new(Arc::new(FileCredentialStore::new(auth_path(agent_dir))), oauth_lookup());
    resolver.set_provider_configs(registry.auth_configs());
    registry.set_configured_providers(configured_providers(&resolver, &registry).await);
    let mut models = registry.available_models();
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

/// Ask on the terminal which saved session to resume.
fn pick_session(dir: &Path) -> anyhow::Result<Option<PathBuf>> {
    let sessions: Vec<_> = list_sessions(dir).into_iter().take(20).collect();
    if sessions.is_empty() {
        eprintln!("No saved sessions for this directory; starting a new one.");
        return Ok(None);
    }
    for (index, info) in sessions.iter().enumerate() {
        let first: String = info.first_message.lines().next().unwrap_or_default().chars().take(70).collect();
        println!("{:>3}. {} ({} messages)", index + 1, first, info.message_count);
    }
    let answer = ask("Resume which session? ")?;
    Ok(answer
        .parse::<usize>()
        .ok()
        .and_then(|choice| sessions.get(choice.wrapping_sub(1)))
        .map(|info| info.path.clone()))
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
fn create_session_manager(args: &Args, place: &SessionPlace<'_>) -> Result<SessionManager, Exit> {
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
        if let Some(path) = pick_session(&dir).map_err(fail)? {
            return SessionManager::open_with(&path, place.dir_override, None).map_err(fail);
        }
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
fn check_session_cwd(session: SessionManager, place: &SessionPlace<'_>) -> Result<SessionManager, Exit> {
    let Some(missing) = missing_session_cwd(&session, place.cwd) else { return Ok(session) };
    if !place.interactive {
        return Err(fail(missing.error_message()));
    }
    let answer = ask(&format!("{}\nContinue in the current directory? [y/N] ", missing.prompt())).map_err(fail)?;
    if !(answer.eq_ignore_ascii_case("y") || answer.eq_ignore_ascii_case("yes")) {
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
        list_models(search, &agent_dir).await;
        return Ok(0);
    }
    if let Some(error) = unknown_flags_error(&args.unknown_flags) {
        eprintln!("Error: {error}");
        return Ok(1);
    }
    let mut mode = app_mode(&args, std::io::stdin().is_terminal(), std::io::stdout().is_terminal());
    if mode == AppMode::Rpc {
        take_over_stdout()?;
        if !args.file_args.is_empty() {
            eprintln!("Error: @file arguments are not supported in RPC mode");
            return Ok(1);
        }
    }
    validate_session_flags(&args)?;
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
    let files = process_file_args(&args.file_args, &cwd)?;
    let mut parts: Vec<String> = stdin_content.into_iter().collect();
    if !files.text.is_empty() {
        parts.push(files.text.clone());
    }
    if !args.messages.is_empty() {
        parts.push(args.messages.remove(0));
    }
    let initial_message = (!parts.is_empty()).then(|| parts.concat());

    let env_session_dir = process_env_value(ENV_SESSION_DIR).filter(|dir| !dir.is_empty());
    let dir_override = args
        .session_dir
        .as_deref()
        .or(env_session_dir.as_deref())
        .map(expand_tilde)
        .or_else(|| startup_settings.session_dir());
    let interactive = mode == AppMode::Interactive && std::io::stdin().is_terminal();
    let place = SessionPlace { cwd: &cwd, agent_dir: &agent_dir, dir_override: dir_override.as_deref(), interactive };
    let mut session_manager = check_session_cwd(create_session_manager(&args, &place)?, &place)?;
    if let Some(name) = &args.name {
        let Some(name) = normalize_session_name(name) else {
            return Err(fail("--name requires a non-empty value").into());
        };
        session_manager.append_session_info(&name)?;
    }

    let options = AppOptions { oauth: Some(oauth_lookup()), ..AppOptions::from_args(&args, &cwd, &agent_dir) };
    let runtime = create_app_runtime(options, session_manager).await?;
    let session = runtime.session();
    configure_http_from_settings(session.settings());
    let runtime_diagnostics: Vec<RuntimeDiagnostic> =
        runtime.diagnostics().into_iter().filter(|diagnostic| !startup_diagnostics.contains(diagnostic)).collect();
    let has_errors = runtime_diagnostics.iter().any(|diagnostic| diagnostic.level == DiagnosticLevel::Error);
    report(&startup_diagnostics);
    if mode != AppMode::Interactive || has_errors {
        report(&runtime_diagnostics);
    }
    if has_errors {
        runtime.dispose().await;
        return Ok(1);
    }
    if mode != AppMode::Interactive && session.model().is_none() {
        eprintln!("Error: {}", format_no_models_available_message());
        runtime.dispose().await;
        return Ok(1);
    }

    match mode {
        AppMode::Rpc => Ok(run_rpc_mode(Arc::new(runtime), RpcModeOptions::default()).await),
        AppMode::Print | AppMode::Json => {
            let output = if mode == AppMode::Json { OutputMode::Json } else { OutputMode::Text };
            Ok(run_print_mode(&runtime, output, initial_message, files.images, args.messages).await)
        }
        _ => {
            report(&runtime_diagnostics);
            if let Some(message) = runtime.model_fallback_message() {
                eprintln!("Warning: {message}");
            }
            for extra in args.messages {
                session.follow_up(&extra, Vec::new());
            }
            run_interactive(runtime, initial_message.map(|text| (text, files.images))).await?;
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
