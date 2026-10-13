//! Plain-terminal rendering of session events for interactive mode.

use std::io::{IsTerminal, Write};

use pi_agent::{AgentEvent, AgentToolResult};

use crate::agent_session::AgentSessionEvent;
use pi_ai::{AssistantMessageEvent, ContentBlock, Message, StopReason};
use serde_json::Value;

const TOOL_PREVIEW_LINES: usize = 8;

#[derive(Clone, Copy)]
pub struct Style {
    pub color: bool,
}

impl Style {
    pub fn detect() -> Self {
        Self { color: std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none() }
    }

    fn paint(self, code: &str, text: &str) -> String {
        if self.color { format!("\x1b[{code}m{text}\x1b[0m") } else { text.to_string() }
    }

    pub fn dim(self, text: &str) -> String {
        self.paint("2", text)
    }

    pub fn bold(self, text: &str) -> String {
        self.paint("1", text)
    }

    pub fn red(self, text: &str) -> String {
        self.paint("31", text)
    }

    pub fn cyan(self, text: &str) -> String {
        self.paint("36", text)
    }

    pub fn yellow(self, text: &str) -> String {
        self.paint("33", text)
    }
}

/// Streams events to stdout. Text deltas print as they arrive.
pub struct Renderer {
    style: Style,
    at_line_start: bool,
    in_thinking: bool,
}

impl Renderer {
    pub fn new(style: Style) -> Self {
        Self { style, at_line_start: true, in_thinking: false }
    }

    fn write(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let mut stdout = std::io::stdout().lock();
        let _ = stdout.write_all(text.as_bytes());
        let _ = stdout.flush();
        self.at_line_start = text.ends_with('\n');
    }

    fn line(&mut self, text: &str) {
        if !self.at_line_start {
            self.write("\n");
        }
        self.write(&format!("{text}\n"));
    }

    /// Render one session event: agent output, retries, and compaction.
    pub fn render(&mut self, event: &AgentSessionEvent<'_>) {
        match event {
            AgentSessionEvent::Agent(event) => self.render_agent(event),
            AgentSessionEvent::AutoRetryStart { attempt, max_attempts, delay_ms, error_message } => {
                let seconds = *delay_ms as f64 / 1000.0;
                let text = format!("Retrying ({attempt}/{max_attempts}) in {seconds:.1}s: {error_message}");
                let text = self.style.yellow(&text);
                self.line(&text);
            }
            AgentSessionEvent::CompactionStart { reason } => {
                let text = self.style.dim(&format!("Compacting context ({})...", reason.as_str()));
                self.line(&text);
            }
            AgentSessionEvent::CompactionEnd { result: Some(result), .. } => {
                let text = self.style.dim(&format!("Compacted {} tokens of history.", result.tokens_before));
                self.line(&text);
            }
            AgentSessionEvent::CompactionEnd { error_message: Some(error), .. } => {
                let text = self.style.red(error);
                self.line(&text);
            }
            _ => {}
        }
    }

    pub fn render_agent(&mut self, event: &AgentEvent) {
        match event {
            AgentEvent::MessageUpdate { assistant_message_event, .. } => match assistant_message_event {
                AssistantMessageEvent::TextStart { .. } if self.in_thinking => {
                    self.in_thinking = false;
                    self.line("");
                }
                AssistantMessageEvent::TextDelta { delta, .. } => self.write(delta),
                AssistantMessageEvent::ThinkingStart { .. } => {
                    self.in_thinking = true;
                    let label = self.style.dim("Thinking:");
                    self.line(&label);
                }
                AssistantMessageEvent::ThinkingDelta { delta, .. } => {
                    let text = self.style.dim(delta);
                    self.write(&text);
                }
                _ => {}
            },
            AgentEvent::MessageEnd { message: Message::Assistant(message) } => {
                self.in_thinking = false;
                if !self.at_line_start {
                    self.write("\n");
                }
                match message.stop_reason {
                    StopReason::Error => {
                        let text = self
                            .style
                            .red(&format!("Error: {}", message.error_message.as_deref().unwrap_or("unknown error")));
                        self.line(&text);
                    }
                    StopReason::Aborted => {
                        let text = self.style.yellow("Aborted");
                        self.line(&text);
                    }
                    _ => {}
                }
            }
            AgentEvent::ToolExecutionStart { tool_name, args, parent_tool_call_id, .. } => {
                let indent = if parent_tool_call_id.is_some() { "  " } else { "" };
                let text = format!(
                    "{indent}{} {}",
                    self.style.cyan(&format!("→ {tool_name}")),
                    self.style.dim(&summarize_args(tool_name, args))
                );
                self.line(&text);
            }
            AgentEvent::ToolExecutionEnd { parent_tool_call_id: Some(_), .. } => {}
            AgentEvent::ToolExecutionEnd { result, is_error, .. } => {
                let preview = tool_preview(result);
                if !preview.is_empty() {
                    let text = if *is_error { self.style.red(&preview) } else { self.style.dim(&preview) };
                    self.line(&text);
                }
            }
            _ => {}
        }
    }
}

/// A short argument summary, for example the path of `read` or the command of `bash`.
pub fn summarize_args(tool: &str, args: &Value) -> String {
    let field = |name: &str| args.get(name).and_then(Value::as_str).map(String::from);
    let summary = match tool {
        "bash" => field("command"),
        "grep" => field("pattern").map(|pattern| format!("{pattern} {}", field("path").unwrap_or_default())),
        "find" => field("pattern"),
        _ => field("path"),
    }
    .unwrap_or_else(|| args.to_string());
    let first_line = summary.lines().next().unwrap_or_default();
    let mut text: String = first_line.chars().take(160).collect();
    if text.len() < summary.len() {
        text.push_str(" …");
    }
    text
}

fn tool_preview(result: &AgentToolResult) -> String {
    let text: String = result
        .content
        .iter()
        .map(|block| match block {
            ContentBlock::Text { text, .. } => text.clone(),
            ContentBlock::Image { mime_type, .. } => format!("[image {mime_type}]"),
            _ => String::new(),
        })
        .collect::<Vec<_>>()
        .join("\n");
    let lines: Vec<&str> = text.trim_end().lines().collect();
    let mut preview: Vec<String> =
        lines.iter().take(TOOL_PREVIEW_LINES).map(|line| format!("  {}", truncate(line, 200))).collect();
    if lines.len() > TOOL_PREVIEW_LINES {
        preview.push(format!("  … {} more lines", lines.len() - TOOL_PREVIEW_LINES));
    }
    preview.join("\n")
}

fn truncate(line: &str, max: usize) -> String {
    if line.chars().count() <= max {
        line.to_string()
    } else {
        format!("{}…", line.chars().take(max).collect::<String>())
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn summarizes_tool_arguments() {
        assert_eq!(summarize_args("bash", &json!({"command": "ls -la\npwd"})), "ls -la …");
        assert_eq!(summarize_args("read", &json!({"path": "src/main.rs"})), "src/main.rs");
        let result = AgentToolResult::text((1..=10).map(|n| n.to_string()).collect::<Vec<_>>().join("\n"));
        assert!(tool_preview(&result).ends_with("… 2 more lines"));
    }
}
