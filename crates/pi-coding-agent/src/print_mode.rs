//! Non-interactive mode (upstream `modes/print-mode.ts`): run the prompts, then print the
//! final response (text), or the session header and every session event as JSON lines (json).

use std::sync::{Arc, Mutex};

use pi_ai::{AssistantMessage, ContentBlock, Message, StopReason};
use serde_json::Value;

use crate::agent_session::{AgentSession, AgentSessionRuntime, PromptOptions, RUN_LIMIT_EXIT_CODE};
use crate::cli::OutputMode;
use crate::extension_ui::ExtensionMode;
use crate::extensions::ExtensionBindings;
use crate::output_schema::{OUTPUT_SCHEMA_EXIT_CODE, OutputSchema, correction_prompt};

/// One JSON line on the real stdout, which the stdout guard keeps for protocol output.
fn write_line(line: &str) {
    let _ = crate::diagnostics::write_raw_stdout(&format!("{line}\n"));
}

/// The session that prints events, with its listener id.
type Subscribed = Option<(Arc<AgentSession>, u64)>;

/// Subscribe the active session to stdout; the runtime rebinds it after session switches.
fn bind_json_output(runtime: &AgentSessionRuntime) {
    let current: Arc<Mutex<Subscribed>> = Arc::new(Mutex::new(None));
    let bind = move |session: &Arc<AgentSession>| {
        let mut current = current.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some((previous, id)) = current.take() {
            previous.unsubscribe(id);
        }
        let id = session.subscribe(|event| {
            if event.is_json_event() {
                write_line(&event.to_json().to_string());
            }
        });
        *current = Some((session.clone(), id));
    };
    bind(&runtime.session());
    runtime.set_rebind_session(Some(Arc::new(bind)));
}

/// What `--output-schema` made of the final response.
enum Outcome {
    /// No schema, or a run limit stopped the run.
    Plain,
    Valid(Value),
    Invalid(Vec<String>),
    /// The last response is an error or aborted.
    Failed(String),
}

/// Run `initial_message` with `initial_images`, then each of `messages`. Returns the exit code:
/// 1 when a prompt fails or the last response is an error or aborted (text mode or a schema),
/// [`RUN_LIMIT_EXIT_CODE`] for a run limit, and [`OUTPUT_SCHEMA_EXIT_CODE`] for a schema mismatch.
pub async fn run_print_mode(
    runtime: &Arc<AgentSessionRuntime>,
    mode: OutputMode,
    initial_message: Option<String>,
    initial_images: Vec<ContentBlock>,
    messages: Vec<String>,
    output_schema: Option<OutputSchema>,
) -> i32 {
    let json = mode == OutputMode::Json;
    if json {
        let header = serde_json::Value::Object(runtime.session().session_manager().header_raw().clone());
        write_line(&header.to_string());
        bind_json_output(runtime);
    }
    let bindings = ExtensionBindings {
        mode: if json { ExtensionMode::Json } else { ExtensionMode::Print },
        runtime: Some(Arc::downgrade(runtime)),
        on_error: Some(Arc::new(|error: &crate::extensions::ExtensionError| {
            eprintln!("Extension error ({}): {}", error.extension_path, error.error);
        })),
        ..ExtensionBindings::default()
    };
    // Binding runs inside the signal guard below: upstream installs its listeners first, so a
    // signal during `session_start` handlers still disposes the runtime.
    let result = async {
        runtime.session().bind_extensions(bindings).await;
        if let Some(text) = initial_message {
            let options = PromptOptions { images: initial_images, ..PromptOptions::default() };
            runtime.session().prompt_with(&text, options).await?;
        }
        for message in &messages {
            if runtime.session().run_limit_stop().is_some() {
                break;
            }
            runtime.session().prompt(message, Vec::new()).await?;
        }
        // A command can start a turn with `pi.sendUserMessage`, which runs in the background.
        runtime.session().wait_for_idle().await;
        let session = runtime.session();
        let registered =
            |name: &str| session.extension_runner().is_some_and(|runner| runner.registry().flag(name).is_some());
        match output_schema.and_then(|schema| schema.unclaimed(registered)) {
            Some(schema) => structured_output(&session, &schema).await,
            None => anyhow::Ok(Outcome::Plain),
        }
    };
    // SIGTERM and SIGHUP (Ctrl+Break and console close on Windows) stop the run. The prompts
    // keep running while the runtime is disposed, so the abort can settle the agent loop.
    let mut work = std::pin::pin!(result);
    let result = tokio::select! {
        biased;
        code = crate::signals::termination_signal() => {
            crate::signals::kill_tracked_detached_children();
            runtime.set_rebind_session(None);
            let mut dispose = std::pin::pin!(runtime.dispose());
            let mut work_done = false;
            loop {
                tokio::select! {
                    () = &mut dispose => break,
                    _ = &mut work, if !work_done => work_done = true,
                }
            }
            let _ = crate::diagnostics::flush_raw_stdout();
            std::process::exit(code);
        }
        result = &mut work => result,
    };
    let app = crate::resources::brand::APP_NAME;
    let code = match (result, runtime.session().run_limit_stop()) {
        (_, Some(stop)) => {
            eprintln!("{app}: {}", stop.message());
            RUN_LIMIT_EXIT_CODE
        }
        (Err(error), None) => {
            eprintln!("{error:#}");
            1
        }
        (Ok(Outcome::Valid(value)), None) => {
            let line = if json { serde_json::json!({ "type": "output", "value": value }) } else { value };
            write_line(&line.to_string());
            0
        }
        (Ok(Outcome::Invalid(errors)), None) => {
            let list: String = errors.iter().map(|error| format!("\n  {error}")).collect();
            eprintln!("{app}: the final response does not match --output-schema:{list}");
            OUTPUT_SCHEMA_EXIT_CODE
        }
        (Ok(Outcome::Failed(message)), None) => {
            eprintln!("{message}");
            1
        }
        (Ok(Outcome::Plain), None) if json => 0,
        (Ok(Outcome::Plain), None) => print_final_response(&runtime.session()),
    };
    runtime.set_rebind_session(None);
    runtime.dispose().await;
    let _ = crate::diagnostics::flush_raw_stdout();
    code
}

/// `--output-schema`: check the last response, sending corrective prompts with the errors while
/// retries remain.
async fn structured_output(session: &AgentSession, schema: &OutputSchema) -> anyhow::Result<Outcome> {
    let mut retries = schema.retries();
    loop {
        if session.run_limit_stop().is_some() {
            return Ok(Outcome::Plain);
        }
        let Some(Message::Assistant(last)) = session.messages().pop() else {
            return Ok(Outcome::Invalid(vec!["there is no response to check".into()]));
        };
        if let Some(message) = failure(&last) {
            return Ok(Outcome::Failed(message));
        }
        let text: Vec<&str> = last
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        match schema.check(&text.join("\n")) {
            Ok(value) => return Ok(Outcome::Valid(value)),
            Err(errors) if retries == 0 => return Ok(Outcome::Invalid(errors)),
            Err(errors) => {
                retries -= 1;
                session.prompt(&correction_prompt(&errors), Vec::new()).await?;
            }
        }
    }
}

/// The message to report when `last` is an error or aborted.
fn failure(last: &AssistantMessage) -> Option<String> {
    let reason = match last.stop_reason {
        StopReason::Error => "error",
        StopReason::Aborted => "aborted",
        _ => return None,
    };
    let message = last.error_message.clone().filter(|message| !message.is_empty());
    Some(message.unwrap_or_else(|| format!("Request {reason}")))
}

fn print_final_response(session: &AgentSession) -> i32 {
    let Some(Message::Assistant(last)) = session.messages().pop() else { return 0 };
    if let Some(message) = failure(&last) {
        eprintln!("{message}");
        return 1;
    }
    for block in &last.content {
        if let ContentBlock::Text { text, .. } = block {
            let _ = crate::diagnostics::write_raw_stdout(&format!("{text}\n"));
        }
    }
    let _ = crate::diagnostics::flush_raw_stdout();
    if last.stop_reason == StopReason::Length {
        eprintln!("Response was truncated: the model reached its output token limit.");
        return 1;
    }
    0
}
