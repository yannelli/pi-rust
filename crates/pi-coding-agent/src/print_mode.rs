//! Non-interactive mode (upstream `modes/print-mode.ts`): run the prompts, then print the
//! final response (text), or the session header and every session event as JSON lines (json).

use std::sync::{Arc, Mutex};

use pi_ai::{ContentBlock, Message, StopReason};

use crate::agent_session::{AgentSession, AgentSessionRuntime, PromptOptions};
use crate::cli::OutputMode;

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
            if event.is_upstream_event() {
                write_line(&event.to_json().to_string());
            }
        });
        *current = Some((session.clone(), id));
    };
    bind(&runtime.session());
    runtime.set_rebind_session(Some(Arc::new(bind)));
}

/// Run `initial_message` with `initial_images`, then each of `messages`. Returns the exit code:
/// 1 when a prompt fails or, in text mode, when the last response is an error or aborted.
pub async fn run_print_mode(
    runtime: &AgentSessionRuntime,
    mode: OutputMode,
    initial_message: Option<String>,
    initial_images: Vec<ContentBlock>,
    messages: Vec<String>,
) -> i32 {
    let json = mode == OutputMode::Json;
    if json {
        let header = serde_json::Value::Object(runtime.session().session_manager().header_raw().clone());
        write_line(&header.to_string());
        bind_json_output(runtime);
    }
    let result = async {
        if let Some(text) = initial_message {
            let options = PromptOptions { images: initial_images, ..PromptOptions::default() };
            runtime.session().prompt_with(&text, options).await?;
        }
        for message in &messages {
            runtime.session().prompt(message, Vec::new()).await?;
        }
        anyhow::Ok(())
    };
    // SIGTERM and SIGHUP (Ctrl+Break and console close on Windows) stop the run. The prompts
    // keep running while the runtime is disposed, so the abort can settle the agent loop.
    let mut work = std::pin::pin!(result);
    let result = tokio::select! {
        result = &mut work => result,
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
    };
    let code = match result {
        Err(error) => {
            eprintln!("{error:#}");
            1
        }
        Ok(()) if json => 0,
        Ok(()) => print_final_response(&runtime.session()),
    };
    runtime.set_rebind_session(None);
    runtime.dispose().await;
    let _ = crate::diagnostics::flush_raw_stdout();
    code
}

fn print_final_response(session: &AgentSession) -> i32 {
    let Some(Message::Assistant(last)) = session.messages().pop() else { return 0 };
    if matches!(last.stop_reason, StopReason::Error | StopReason::Aborted) {
        let reason = if last.stop_reason == StopReason::Error { "error" } else { "aborted" };
        let message = last.error_message.clone().filter(|message| !message.is_empty());
        eprintln!("{}", message.unwrap_or_else(|| format!("Request {reason}")));
        return 1;
    }
    for block in &last.content {
        if let ContentBlock::Text { text, .. } = block {
            let _ = crate::diagnostics::write_raw_stdout(&format!("{text}\n"));
        }
    }
    let _ = crate::diagnostics::flush_raw_stdout();
    0
}
