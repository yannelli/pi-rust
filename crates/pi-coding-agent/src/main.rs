use std::io::{IsTerminal, Read};
use std::path::PathBuf;
use std::sync::Arc;

use pi_coding_agent::app::{CodingSession, SessionOptions};
use pi_coding_agent::cli::{Args, help_text, parse_args};
use pi_coding_agent::config::{agent_dir, expand_tilde, models_path};
use pi_coding_agent::files::process_file_args;
use pi_coding_agent::interactive::run_interactive;
use pi_coding_agent::print_mode::run_print_mode;
use pi_coding_agent::session::{list_sessions, session_dir};

fn list_models(search: &str, agent_dir: &std::path::Path) -> anyhow::Result<()> {
    let registry = pi_ai::ModelRegistry::load(&models_path(agent_dir))?;
    let needle = search.to_lowercase();
    let mut rows: Vec<_> = registry
        .models()
        .iter()
        .filter(|model| {
            needle.is_empty()
                || format!("{}/{} {}", model.provider, model.id, model.name).to_lowercase().contains(&needle)
        })
        .collect();
    rows.sort_by(|a, b| (&a.provider, &a.id).cmp(&(&b.provider, &b.id)));
    for model in rows {
        let key = if registry.has_api_key(&model.provider) { "key" } else { "   " };
        println!(
            "{key}  {:<48} ctx {:>8}  out {:>7}  ${}/{} per M",
            format!("{}/{}", model.provider, model.id),
            model.context_window,
            model.max_tokens,
            model.cost.input,
            model.cost.output
        );
    }
    Ok(())
}

/// Ask on the terminal which saved session to resume.
fn pick_session(dir: &std::path::Path) -> anyhow::Result<Option<PathBuf>> {
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

async fn run(mut args: Args) -> anyhow::Result<i32> {
    if args.help {
        println!("{}", help_text());
        return Ok(0);
    }
    if args.version {
        println!("{}", env!("CARGO_PKG_VERSION"));
        return Ok(0);
    }
    if !args.errors.is_empty() {
        for error in &args.errors {
            eprintln!("Error: {error}");
        }
        eprintln!("Run pi --help for usage.");
        return Ok(2);
    }
    let agent_dir = agent_dir();
    if let Some(search) = &args.list_models {
        list_models(search, &agent_dir)?;
        return Ok(0);
    }
    let cwd = std::env::current_dir()?;
    if !std::io::stdin().is_terminal() {
        let mut piped = String::new();
        std::io::stdin().read_to_string(&mut piped)?;
        if !piped.trim().is_empty() {
            args.print = true;
            match args.messages.first_mut() {
                Some(first) => *first = format!("{}\n\n{first}", piped.trim_end()),
                None => args.messages.push(piped.trim_end().to_string()),
            }
        }
    }
    let files = process_file_args(&args.file_args, &cwd)?;
    if !files.text.is_empty() {
        match args.messages.first_mut() {
            Some(first) => *first = format!("{}{first}", files.text),
            None => args.messages.push(files.text.trim_end().to_string()),
        }
    }

    let persist = !args.no_session;
    let sessions_dir = args.session_dir.as_deref().map(expand_tilde).unwrap_or_else(|| session_dir(&agent_dir, &cwd));
    let mut session_file = args.session.as_deref().map(expand_tilde);
    if args.resume && session_file.is_none() && persist {
        session_file = pick_session(&sessions_dir)?;
    }
    let tools = if args.no_tools { Some(Vec::new()) } else { args.tools.clone() };
    let session = Arc::new(CodingSession::new(SessionOptions {
        cwd,
        agent_dir,
        provider: args.provider.clone(),
        model: args.model.clone(),
        api_key: args.api_key.clone(),
        thinking: args.thinking,
        system_prompt: args.system_prompt.clone(),
        append_system_prompt: args.append_system_prompt.clone(),
        tools,
        exclude_tools: args.exclude_tools.clone(),
        no_context_files: args.no_context_files,
        session_file,
        continue_recent: args.continue_session,
        persist,
        session_dir: Some(sessions_dir),
    })?);

    if args.print {
        return run_print_mode(session, args.mode, args.messages, files.images).await;
    }
    let mut messages = args.messages.into_iter();
    let initial = messages.next().map(|first| (first, files.images));
    for extra in messages {
        session.agent.follow_up(pi_ai::Message::user(extra));
    }
    run_interactive(session, initial).await?;
    Ok(0)
}

#[tokio::main]
async fn main() {
    let args = parse_args(&std::env::args().skip(1).collect::<Vec<_>>());
    let code = match run(args).await {
        Ok(code) => code,
        Err(error) => {
            eprintln!("Error: {error:#}");
            1
        }
    };
    std::process::exit(code);
}
