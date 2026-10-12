//! Non-interactive mode: run the prompts, then print the final response (text)
//! or every agent event as one JSON object per line (json).

use std::io::Write;
use std::sync::Arc;

use pi_ai::{ContentBlock, Message, StopReason};

use crate::app::CodingSession;
use crate::cli::OutputMode;

/// Returns the process exit code.
pub async fn run_print_mode(
    session: Arc<CodingSession>,
    mode: OutputMode,
    messages: Vec<String>,
    first_images: Vec<ContentBlock>,
) -> anyhow::Result<i32> {
    if messages.is_empty() {
        anyhow::bail!("No message given. Pass a message after -p or on stdin.");
    }
    if mode == OutputMode::Json {
        session.agent.subscribe(|event| {
            if let Ok(line) = serde_json::to_string(event) {
                let mut stdout = std::io::stdout().lock();
                let _ = writeln!(stdout, "{line}");
                let _ = stdout.flush();
            }
        });
    }
    let mut images = first_images;
    for message in &messages {
        session.prompt(message, std::mem::take(&mut images)).await?;
        if let Some(Message::Assistant(last)) = session.agent.state().messages.last()
            && matches!(last.stop_reason, StopReason::Error | StopReason::Aborted)
        {
            if mode == OutputMode::Text {
                eprintln!("Error: {}", last.error_message.as_deref().unwrap_or("request failed"));
            }
            return Ok(1);
        }
    }
    if mode == OutputMode::Text {
        let state = session.agent.state();
        let text = state
            .messages
            .iter()
            .rev()
            .find_map(|message| match message {
                Message::Assistant(assistant) => Some(assistant.text()),
                _ => None,
            })
            .unwrap_or_default();
        println!("{text}");
    }
    Ok(0)
}
