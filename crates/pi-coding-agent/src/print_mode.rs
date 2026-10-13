//! Non-interactive mode (upstream `modes/print-mode.ts`): run the prompts, then print the
//! final response (text), or stream every session event as one JSON object per line (json).

use std::io::Write;
use std::sync::Arc;

use pi_ai::{ContentBlock, Message, StopReason};

use crate::app::CodingSession;
use crate::cli::OutputMode;

/// Run `initial_message` with `initial_images`, then each of `messages`. Returns the exit code:
/// 1 when a prompt fails or, in text mode, when the last response is an error or aborted.
pub async fn run_print_mode(
    session: Arc<CodingSession>,
    mode: OutputMode,
    initial_message: Option<String>,
    initial_images: Vec<ContentBlock>,
    messages: Vec<String>,
) -> i32 {
    let json = mode == OutputMode::Json;
    let subscription = json.then(|| {
        session.subscribe(|event| {
            let mut stdout = std::io::stdout().lock();
            let _ = writeln!(stdout, "{}", event.to_json());
            let _ = stdout.flush();
        })
    });
    let result = async {
        if let Some(text) = initial_message {
            session.prompt(&text, initial_images).await?;
        }
        for message in &messages {
            session.prompt(message, Vec::new()).await?;
        }
        anyhow::Ok(())
    }
    .await;
    if let Some(id) = subscription {
        session.unsubscribe(id);
    }
    if let Err(error) = result {
        eprintln!("{error:#}");
        return 1;
    }
    if json {
        return 0;
    }
    let state = session.agent.state();
    let Some(Message::Assistant(last)) = state.messages.last() else { return 0 };
    if matches!(last.stop_reason, StopReason::Error | StopReason::Aborted) {
        let reason = if last.stop_reason == StopReason::Error { "error" } else { "aborted" };
        let message = last.error_message.clone().filter(|message| !message.is_empty());
        eprintln!("{}", message.unwrap_or_else(|| format!("Request {reason}")));
        return 1;
    }
    let mut stdout = std::io::stdout().lock();
    for block in &last.content {
        if let ContentBlock::Text { text, .. } = block {
            let _ = writeln!(stdout, "{text}");
        }
    }
    let _ = stdout.flush();
    0
}
