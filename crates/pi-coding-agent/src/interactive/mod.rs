//! Line-based interactive mode. Responses stream to the terminal; input typed while the agent
//! works is queued as steering, or as a follow-up with `/follow`. `!cmd` runs a shell command
//! for the model to see and `!!cmd` runs one it does not see.

pub mod selector;

use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pi_ai::{ContentBlock, ThinkingLevel};
use tokio::io::AsyncBufReadExt;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::agent_session::{AgentSession, AgentSessionRuntime};
use crate::model_resolver::{ModelCatalog, resolve_cli_model};
use crate::render::{Renderer, Style};
use crate::session::{SessionInfo, list_sessions};

const HELP: &str = "Commands:
  /model [pattern]   Show or switch the model
  /thinking [level]  Show or set the thinking level (off, minimal, low, medium, high, xhigh)
  /compact [focus]   Summarize older history
  /name <name>       Name the session
  /new               Start a new session
  /session           Show session details
  /resume            Pick a previous session
  /export [file]     Export the session to HTML
  /reload            Reload settings, skills, prompts, and context files
  /follow <text>     Queue a follow-up while the agent works
  /abort             Abort the running response
  /quit              Exit
  !command           Run a shell command and add its output to the context (!! to keep it out)
While the agent works, plain input is queued as steering. Ctrl+C aborts; press it twice to exit.";

struct Repl {
    runtime: Arc<AgentSessionRuntime>,
    style: Style,
    run: Option<JoinHandle<anyhow::Result<()>>>,
    picking: Option<Vec<SessionInfo>>,
}

fn print_error(style: Style, error: impl std::fmt::Display) {
    println!("{}", style.red(&error.to_string()));
}

impl Repl {
    fn session(&self) -> Arc<AgentSession> {
        self.runtime.session()
    }

    fn prompt_marker(&self) {
        if self.run.is_none() {
            print!("{} ", self.style.bold(">"));
            let _ = std::io::stdout().flush();
        }
    }

    fn start(&mut self, text: String, images: Vec<ContentBlock>) {
        let session = self.session();
        self.run = Some(tokio::spawn(async move { session.prompt(&text, images).await.map(|_| ()) }));
    }

    fn start_bash(&mut self, command: String, exclude_from_context: bool) {
        let session = self.session();
        self.run = Some(tokio::spawn(async move {
            let mut print_chunk = |chunk: &str| {
                print!("{chunk}");
                let _ = std::io::stdout().flush();
            };
            let result = session.execute_bash(&command, exclude_from_context, None, &mut print_chunk).await?;
            match (result.cancelled, result.exit_code) {
                (true, _) => println!("\n(cancelled)"),
                (false, Some(code)) if code != 0 => println!("\n(exit code {code})"),
                _ => println!(),
            }
            Ok(())
        }));
    }

    async fn handle_line(&mut self, line: String) -> bool {
        let line = line.trim().to_string();
        if let Some(sessions) = self.picking.take() {
            self.finish_pick(&line, sessions).await;
            return true;
        }
        if line.is_empty() {
            return true;
        }
        let (command, argument) = match line.split_once(char::is_whitespace) {
            Some((command, rest)) => (command, rest.trim()),
            None => (line.as_str(), ""),
        };
        if self.run.is_some() {
            return self.handle_busy_line(&line, command, argument);
        }
        if let Some(bash) = line.strip_prefix('!') {
            let (command, exclude) = match bash.strip_prefix('!') {
                Some(command) => (command.trim(), true),
                None => (bash.trim(), false),
            };
            if !command.is_empty() {
                self.start_bash(command.to_string(), exclude);
            }
            return true;
        }
        match command {
            "/quit" | "/exit" => return false,
            "/help" => println!("{HELP}"),
            "/model" => self.model_command(argument).await,
            "/thinking" => self.thinking_command(argument),
            "/name" => match self.session().set_session_name(argument) {
                Ok(()) => println!("Session name: {argument}"),
                Err(error) => print_error(self.style, format!("{error:#}")),
            },
            "/new" => match self.runtime.new_session(None, None).await {
                Ok(()) => println!("Started a new session"),
                Err(error) => print_error(self.style, format!("{error:#}")),
            },
            "/session" => self.session_command(),
            "/resume" => self.resume_command(),
            "/export" => match self.session().export_to_html((!argument.is_empty()).then_some(argument)) {
                Ok(path) => println!("Exported to: {}", path.display()),
                Err(error) => print_error(self.style, format!("{error:#}")),
            },
            "/reload" => {
                self.session().reload();
                println!("Reloaded settings and resources");
            }
            "/compact" => {
                let focus = (!argument.is_empty()).then_some(argument);
                match self.session().compact(focus).await {
                    Ok(result) => println!(
                        "Compacted {} tokens of history.\n{}",
                        result.tokens_before,
                        self.style.dim(&result.summary)
                    ),
                    Err(error) => print_error(self.style, format!("{error:#}")),
                }
            }
            "/follow" | "/abort" => println!("{}", self.style.dim("Nothing is running.")),
            _ => self.start(line, Vec::new()),
        }
        true
    }

    fn handle_busy_line(&mut self, line: &str, command: &str, argument: &str) -> bool {
        let session = self.session();
        match command {
            "/abort" => session.request_abort(),
            "/follow" if !argument.is_empty() => {
                session.follow_up(argument, Vec::new());
                println!("{}", self.style.dim("(queued as follow-up)"));
            }
            "/quit" | "/exit" => {
                session.request_abort();
                session.abort_bash();
                return false;
            }
            _ if line.starts_with('/') || line.starts_with('!') => {
                println!("{}", self.style.dim("Wait for the response to finish, or /abort it."))
            }
            _ => {
                session.steer(line, Vec::new());
                println!("{}", self.style.dim("(queued as steering)"));
            }
        }
        true
    }

    async fn model_command(&self, pattern: &str) {
        let session = self.session();
        let runtime = session.model_runtime().clone();
        if pattern.is_empty() {
            match session.model() {
                Some(current) => println!("Current model: {}/{}", current.provider, current.id),
                None => println!("No model selected."),
            }
            let available: Vec<String> = runtime
                .available_chat_models()
                .iter()
                .map(|model| format!("  {}/{}", model.provider, model.id))
                .collect();
            println!("Models with configured keys ({}):", available.len());
            for line in available.iter().take(50) {
                println!("{line}");
            }
            if available.len() > 50 {
                println!("  … use /model <pattern> to search");
            }
            return;
        }
        let resolved = resolve_cli_model(None, Some(pattern), None, runtime.as_ref());
        if let Some(warning) = resolved.warning {
            println!("{}", self.style.yellow(&format!("Warning: {warning}")));
        }
        let Some(model) = resolved.model else {
            print_error(self.style, resolved.error.unwrap_or_else(|| format!("No model matches \"{pattern}\"")));
            return;
        };
        let name = format!("{}/{}", model.provider, model.id);
        match session.set_model(model, true).await {
            Ok(()) => {
                if let Some(level) = resolved.thinking_level {
                    session.set_thinking_level(level, true);
                }
                println!("Model: {name} (thinking {})", session.thinking_level().as_str());
            }
            Err(error) => print_error(self.style, format!("{error:#}")),
        }
    }

    fn thinking_command(&self, level: &str) {
        let session = self.session();
        if level.is_empty() {
            println!("Thinking level: {}", session.thinking_level().as_str());
            return;
        }
        match level.parse::<ThinkingLevel>() {
            Ok(level) => {
                session.set_thinking_level(level, true);
                println!("Thinking level: {}", session.thinking_level().as_str());
            }
            Err(error) => println!("{error}"),
        }
    }

    fn session_command(&self) {
        let session = self.session();
        let report = session.session_stats();
        let file =
            session.session_file().map(|path| path.display().to_string()).unwrap_or_else(|| "(not saved)".into());
        println!("Session: {}\nFile: {file}", session.session_id());
        if let Some(name) = session.session_name() {
            println!("Name: {name}");
        }
        if let Some(model) = session.model() {
            println!("Model: {}/{} (thinking {})", model.provider, model.id, session.thinking_level().as_str());
        }
        let stats = &report.stats;
        let context = report
            .context_usage
            .and_then(|usage| usage.tokens.map(|tokens| format!("~{tokens} / {} tokens", usage.context_window)))
            .unwrap_or_else(|| "unknown".into());
        println!("Messages: {}  Context: {context}  Cost: ${:.4}", stats.total_messages, stats.cost);
    }

    fn resume_command(&mut self) {
        let session = self.session();
        if session.session_file().is_none() {
            println!("Sessions are not saved in this run (--no-session).");
            return;
        }
        let dir = session.session_manager().session_dir().to_path_buf();
        let sessions: Vec<SessionInfo> = list_sessions(&dir).into_iter().take(20).collect();
        if sessions.is_empty() {
            println!("No saved sessions for this directory.");
            return;
        }
        for (index, info) in sessions.iter().enumerate() {
            let first: String = info.first_message.lines().next().unwrap_or_default().chars().take(70).collect();
            println!("{:>3}. {} ({} messages)", index + 1, first, info.message_count);
        }
        println!("Enter a number to resume, or anything else to cancel.");
        self.picking = Some(sessions);
    }

    async fn finish_pick(&self, line: &str, sessions: Vec<SessionInfo>) {
        let Some(info) = line.parse::<usize>().ok().and_then(|choice| sessions.get(choice.wrapping_sub(1))) else {
            println!("Cancelled");
            return;
        };
        match self.runtime.switch_session(&info.path, None).await {
            Ok(()) => println!("Resumed {} ({} messages)", info.path.display(), info.message_count),
            Err(error) => print_error(self.style, format!("{error:#}")),
        }
    }
}

/// Render session events of the active session, following runtime replacements.
fn bind_renderer(runtime: &AgentSessionRuntime, style: Style) {
    let renderer = Arc::new(Mutex::new(Renderer::new(style)));
    let bind = move |session: &Arc<AgentSession>| {
        let renderer = renderer.clone();
        session.subscribe(move |event| renderer.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).render(event));
    };
    bind(&runtime.session());
    runtime.set_rebind_session(Some(Arc::new(bind)));
}

fn print_banner(session: &AgentSession, style: Style) {
    println!("{} {}", style.bold("pi"), style.dim(&format!("v{} (Rust)", env!("CARGO_PKG_VERSION"))));
    let model = match session.model() {
        Some(model) => format!("Model: {}/{}", model.provider, model.id),
        None => "No model selected. Use /model or log in with `pi auth login`.".to_string(),
    };
    println!("{}", style.dim(&format!("{model}  Thinking: {}", session.thinking_level().as_str())));
    for file in &session.services().resources().resources().context_files {
        println!("{}", style.dim(&format!("Loaded {}", file.path.display())));
    }
    let restored = session.messages().iter().filter(|message| !matches!(message, pi_ai::Message::System(_))).count();
    if restored > 0 {
        println!("{}", style.dim(&format!("Resumed session with {restored} messages")));
    }
    println!("{}", style.dim("Type /help for commands."));
}

pub async fn run_interactive(
    runtime: AgentSessionRuntime,
    initial: Option<(String, Vec<ContentBlock>)>,
) -> anyhow::Result<()> {
    let style = Style::detect();
    print_banner(&runtime.session(), style);
    bind_renderer(&runtime, style);

    let (lines_tx, mut lines) = mpsc::unbounded_channel::<String>();
    tokio::spawn(async move {
        let mut reader = tokio::io::BufReader::new(tokio::io::stdin()).lines();
        while let Ok(Some(line)) = reader.next_line().await {
            if lines_tx.send(line).is_err() {
                break;
            }
        }
    });

    let mut repl = Repl { runtime: Arc::new(runtime), style, run: None, picking: None };
    if let Some((text, images)) = initial {
        repl.start(text, images);
    }
    repl.prompt_marker();
    let mut last_interrupt: Option<Instant> = None;
    let mut input_closed = false;
    loop {
        tokio::select! {
            line = lines.recv(), if !input_closed => match line {
                Some(line) => {
                    if !repl.handle_line(line).await {
                        break;
                    }
                    repl.prompt_marker();
                }
                None if repl.run.is_none() => break,
                None => input_closed = true,
            },
            _ = tokio::signal::ctrl_c() => {
                if repl.run.is_some() {
                    repl.session().request_abort();
                    repl.session().abort_bash();
                } else if last_interrupt.is_some_and(|time| time.elapsed() < Duration::from_secs(1)) {
                    println!();
                    break;
                } else {
                    println!("\n{}", style.dim("Press Ctrl+C again to exit."));
                    last_interrupt = Some(Instant::now());
                    repl.prompt_marker();
                }
            }
            result = async { repl.run.as_mut().expect("guarded by the select condition").await }, if repl.run.is_some() => {
                repl.run = None;
                match result {
                    Ok(Err(error)) => print_error(style, format!("{error:#}")),
                    Err(error) => print_error(style, format!("Run failed: {error}")),
                    Ok(Ok(())) => {}
                }
                if input_closed {
                    break;
                }
                repl.prompt_marker();
            }
        }
    }
    if let Some(run) = repl.run.take() {
        repl.session().request_abort();
        repl.session().abort_bash();
        let _ = run.await;
    }
    repl.runtime.set_rebind_session(None);
    repl.runtime.dispose().await;
    Ok(())
}
