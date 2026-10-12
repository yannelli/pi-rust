use std::io::{IsTerminal, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use pi_ai::ModelRegistry;
use pi_ai::auth::{AuthResolver, FileCredentialStore, OAuthLookup};
use pi_ai::env::{process_env_value, set_child_process_env};
use pi_coding_agent::app::{
    CodingSession, SessionOptions, StartupError, configured_providers, format_no_models_available_message,
};
use pi_coding_agent::auth_cli::run_auth_command;
use pi_coding_agent::cli::{Args, OutputMode, help_text, parse_args, unknown_flags_error};
use pi_coding_agent::config::{ENV_SESSION_DIR, agent_dir, auth_path, env_flag, expand_tilde, models_path};
use pi_coding_agent::files::process_file_args;
use pi_coding_agent::interactive::run_interactive;
use pi_coding_agent::print_mode::run_print_mode;
use pi_coding_agent::session::{list_sessions, session_dir};
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
    Arc::new(|_: &str| None)
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
    print!("Resume which session? ");
    std::io::Write::flush(&mut std::io::stdout())?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    Ok(answer
        .trim()
        .parse::<usize>()
        .ok()
        .and_then(|choice| sessions.get(choice.wrapping_sub(1)))
        .map(|info| info.path.clone()))
}

async fn run(raw: Vec<String>) -> anyhow::Result<i32> {
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
        eprintln!("Error: RPC mode is not supported yet");
        return Ok(1);
    }
    let cwd = std::env::current_dir()?;
    let trusted = args.project_trust_override.unwrap_or(true);
    let settings = Arc::new(SettingsManager::create(&cwd, &agent_dir, SettingsOptions { project_trusted: trusted }));
    for error in settings.drain_errors() {
        eprintln!("Warning: {}", error.diagnostic());
    }

    let mut stdin_content = None;
    if !std::io::stdin().is_terminal() {
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

    let persist = !args.no_session;
    let env_session_dir = process_env_value(ENV_SESSION_DIR).filter(|dir| !dir.is_empty());
    let sessions_dir = args
        .session_dir
        .as_deref()
        .or(env_session_dir.as_deref())
        .map(expand_tilde)
        .or_else(|| settings.session_dir())
        .unwrap_or_else(|| session_dir(&agent_dir, &cwd));
    let mut session_file = args.session.as_deref().map(expand_tilde);
    if args.resume && session_file.is_none() && persist && mode == AppMode::Interactive {
        session_file = pick_session(&sessions_dir)?;
    }
    let created = CodingSession::new(SessionOptions {
        cwd,
        agent_dir,
        provider: args.provider.clone(),
        model: args.model.clone(),
        api_key: args.api_key.clone(),
        thinking: args.thinking,
        models: args.models.clone(),
        system_prompt: args.system_prompt.clone(),
        append_system_prompt: args.append_system_prompt.clone(),
        tools: args.tools.clone(),
        exclude_tools: args.exclude_tools.clone(),
        no_tools: args.no_tools,
        no_builtin_tools: args.no_builtin_tools,
        no_context_files: args.no_context_files,
        session_file,
        continue_recent: args.continue_session,
        persist,
        session_dir: Some(sessions_dir),
        settings: Some(settings),
        oauth: Some(oauth_lookup()),
        ..SessionOptions::default()
    })
    .await;
    let session = match created {
        Ok(session) => Arc::new(session),
        Err(error) => {
            if let Some(startup) = error.downcast_ref::<StartupError>() {
                startup.warnings.iter().for_each(|warning| eprintln!("Warning: {warning}"));
                startup.errors.iter().for_each(|error| eprintln!("Error: {error}"));
                return Ok(1);
            }
            return Err(error);
        }
    };
    for warning in &session.warnings {
        eprintln!("Warning: {warning}");
    }

    match mode {
        AppMode::Print | AppMode::Json => {
            let output = if mode == AppMode::Json { OutputMode::Json } else { OutputMode::Text };
            Ok(run_print_mode(session, output, initial_message, files.images, args.messages).await)
        }
        _ => {
            if let Some(message) = &session.model_fallback_message {
                eprintln!("Warning: {message}");
            }
            for extra in args.messages {
                session.agent.follow_up(pi_ai::Message::user(extra));
            }
            run_interactive(session, initial_message.map(|text| (text, files.images))).await?;
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
