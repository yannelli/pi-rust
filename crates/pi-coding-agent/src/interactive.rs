//! Line-based interactive mode. Responses stream to the terminal; input typed
//! while the agent works is queued as steering, or as a follow-up with `/follow`.

use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pi_ai::{ContentBlock, Message, ThinkingLevel};
use tokio::io::AsyncBufReadExt;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::app::CodingSession;
use crate::compaction::context_tokens;
use crate::render::{Renderer, Style};
use crate::session::{SessionInfo, list_sessions};

const HELP: &str = "Commands:
  /model [pattern]   Show or switch the model
  /thinking [level]  Show or set the thinking level (off, minimal, low, medium, high, xhigh)
  /compact [focus]   Summarize older history
  /new               Start a new session
  /session           Show session details
  /resume            Pick a previous session
  /follow <text>     Queue a follow-up while the agent works
  /abort             Abort the running response
  /quit              Exit
While the agent works, plain input is queued as steering. Ctrl+C aborts; press it twice to exit.";

struct Repl {
    session: Arc<CodingSession>,
    style: Style,
    run: Option<JoinHandle<anyhow::Result<()>>>,
    picking: Option<Vec<SessionInfo>>,
}

impl Repl {
    fn prompt_marker(&self) {
        if self.run.is_none() {
            print!("{} ", self.style.bold(">"));
            let _ = std::io::stdout().flush();
        }
    }

    fn start(&mut self, text: String, images: Vec<ContentBlock>) {
        let session = self.session.clone();
        self.run = Some(tokio::spawn(async move { session.prompt(&text, images).await }));
    }

    async fn handle_line(&mut self, line: String) -> bool {
        let line = line.trim().to_string();
        if let Some(sessions) = self.picking.take() {
            match line.parse::<usize>().ok().and_then(|choice| sessions.get(choice.wrapping_sub(1))) {
                Some(info) => match self.session.open_session(&info.path) {
                    Ok(()) => println!("Resumed {} ({} messages)", info.path.display(), info.message_count),
                    Err(error) => println!("{}", self.style.red(&format!("{error:#}"))),
                },
                None => println!("Cancelled"),
            }
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
            match command {
                "/abort" => self.session.agent.abort(),
                "/follow" if !argument.is_empty() => {
                    self.session.agent.follow_up(Message::user(argument));
                    println!("{}", self.style.dim("(queued as follow-up)"));
                }
                "/quit" | "/exit" => {
                    self.session.agent.abort();
                    return false;
                }
                _ if line.starts_with('/') => {
                    println!("{}", self.style.dim("Wait for the response to finish, or /abort it."))
                }
                _ => {
                    self.session.agent.steer(Message::user(line.as_str()));
                    println!("{}", self.style.dim("(queued as steering)"));
                }
            }
            return true;
        }
        match command {
            "/quit" | "/exit" => return false,
            "/help" => println!("{HELP}"),
            "/model" => self.model_command(argument),
            "/thinking" => self.thinking_command(argument),
            "/new" => match self.session.new_session() {
                Ok(()) => println!("Started a new session"),
                Err(error) => println!("{}", self.style.red(&format!("{error:#}"))),
            },
            "/session" => self.session_command(),
            "/resume" => self.resume_command(),
            "/compact" => {
                let focus = (!argument.is_empty()).then_some(argument);
                match self.session.compact(focus).await {
                    Ok(result) => println!(
                        "Compacted {} tokens of history.\n{}",
                        result.tokens_before,
                        self.style.dim(&result.summary)
                    ),
                    Err(error) => println!("{}", self.style.red(&format!("Compaction failed: {error:#}"))),
                }
            }
            "/follow" | "/abort" => println!("{}", self.style.dim("Nothing is running.")),
            _ if command.starts_with('/') => println!("Unknown command: {command}. Type /help for commands."),
            _ => self.start(line, Vec::new()),
        }
        true
    }

    fn model_command(&self, pattern: &str) {
        let registry = self.session.registry();
        if pattern.is_empty() {
            let current = self.session.model();
            println!("Current model: {}/{}", current.provider, current.id);
            let available: Vec<String> = registry
                .models()
                .iter()
                .filter(|model| registry.has_api_key(&model.provider))
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
        match registry.resolve(pattern, None).cloned() {
            Some(model) => {
                let name = format!("{}/{}", model.provider, model.id);
                match self.session.set_model(model) {
                    Ok(()) => println!("Model: {name}"),
                    Err(error) => println!("{}", self.style.red(&format!("{error:#}"))),
                }
            }
            None => println!("No model matches \"{pattern}\""),
        }
    }

    fn thinking_command(&self, level: &str) {
        if level.is_empty() {
            println!("Thinking level: {}", self.session.agent.state().thinking_level.as_str());
            return;
        }
        match level.parse::<ThinkingLevel>() {
            Ok(level) => match self.session.set_thinking_level(level) {
                Ok(()) => println!("Thinking level: {}", self.session.agent.state().thinking_level.as_str()),
                Err(error) => println!("{}", self.style.red(&format!("{error:#}"))),
            },
            Err(error) => println!("{error}"),
        }
    }

    fn session_command(&self) {
        let state = self.session.agent.state();
        let cost: f64 = state
            .messages
            .iter()
            .filter_map(|message| match message {
                Message::Assistant(assistant) => Some(assistant.usage.cost.total),
                _ => None,
            })
            .sum();
        let file =
            self.session.session_file().map(|path| path.display().to_string()).unwrap_or_else(|| "(not saved)".into());
        println!("Session: {}\nFile: {file}", self.session.session_id());
        println!("Model: {}/{} (thinking {})", state.model.provider, state.model.id, state.thinking_level.as_str());
        println!(
            "Messages: {}  Context: ~{} / {} tokens  Cost: ${cost:.4}",
            state.messages.len(),
            context_tokens(&state.messages),
            state.model.context_window
        );
    }

    fn resume_command(&mut self) {
        let Some(dir) = self.session.sessions_dir() else {
            println!("Sessions are not saved in this run (--no-session).");
            return;
        };
        let sessions: Vec<SessionInfo> = list_sessions(dir).into_iter().take(20).collect();
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
}

pub async fn run_interactive(
    session: Arc<CodingSession>,
    initial: Option<(String, Vec<ContentBlock>)>,
) -> anyhow::Result<()> {
    let style = Style::detect();
    let model = session.model();
    println!("{} {}", style.bold("pi"), style.dim(&format!("v{} (Rust)", env!("CARGO_PKG_VERSION"))));
    println!(
        "{}",
        style.dim(&format!(
            "Model: {}/{}  Thinking: {}",
            model.provider,
            model.id,
            session.agent.state().thinking_level.as_str()
        ))
    );
    for file in &session.context_files {
        println!("{}", style.dim(&format!("Loaded {}", file.path.display())));
    }
    let restored = session.agent.state().messages.len();
    if restored > 0 {
        println!("{}", style.dim(&format!("Resumed session with {restored} messages")));
    }
    println!("{}", style.dim("Type /help for commands."));

    let renderer = Arc::new(Mutex::new(Renderer::new(style)));
    session
        .agent
        .subscribe(move |event| renderer.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).render(event));

    let (lines_tx, mut lines) = mpsc::unbounded_channel::<String>();
    tokio::spawn(async move {
        let mut reader = tokio::io::BufReader::new(tokio::io::stdin()).lines();
        while let Ok(Some(line)) = reader.next_line().await {
            if lines_tx.send(line).is_err() {
                break;
            }
        }
    });

    let mut repl = Repl { session, style, run: None, picking: None };
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
                    repl.session.agent.abort();
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
                    Ok(Err(error)) => println!("{}", style.red(&format!("{error:#}"))),
                    Err(error) => println!("{}", style.red(&format!("Run failed: {error}"))),
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
        repl.session.agent.abort();
        let _ = run.await;
    }
    Ok(())
}
