//! `session/prompt` (upstream `PiAcpAgent.prompt`): the built-in slash commands ACP answers
//! itself, then the prompt turn.

use std::path::Path;

use agent_client_protocol_schema::{
    ContentBlock, ContentChunk, PromptRequest, PromptResponse, ResourceLink, SessionNotification, SessionUpdate,
    StopReason, TextContent,
};
use serde_json::Value;

use super::agent::AcpAgent;
use super::commands::parse_command_args;
use super::errors::{AcpResult, internal_failure};
use super::session::{AcpSession, TurnEnd};
use super::translate::{prompt_to_pi_message, prompt_to_session_title};
use crate::platform::changelog::EMBEDDED_CHANGELOG;

const CHANGELOG_MAX_CHARS: usize = 20_000;

/// The `/session` summary of pi's session stats.
pub fn format_session_stats(stats: &Value) -> String {
    let mut lines = Vec::new();
    let text = |key: &str| stats[key].as_str().filter(|text| !text.is_empty());
    if let Some(id) = text("sessionId") {
        lines.push(format!("Session: {id}"));
    }
    if let Some(file) = text("sessionFile") {
        lines.push(format!("Session file: {file}"));
    }
    if let Some(count) = stats["totalMessages"].as_f64() {
        lines.push(format!("Messages: {count}"));
    }
    if let Some(cost) = stats["cost"].as_f64() {
        lines.push(format!("Cost: {cost}"));
    }
    let tokens = &stats["tokens"];
    if tokens.is_object() {
        let parts: Vec<String> = [("input", "in"), ("output", "out"), ("cacheRead", "cache read")]
            .into_iter()
            .chain([("cacheWrite", "cache write"), ("total", "total")])
            .filter_map(|(key, label)| tokens[key].as_f64().map(|value| format!("{label} {value}")))
            .collect();
        if !parts.is_empty() {
            lines.push(format!("Tokens: {}", parts.join(", ")));
        }
    }
    if lines.is_empty() {
        format!("Session stats:\n{}", serde_json::to_string_pretty(stats).unwrap_or_default())
    } else {
        lines.join("\n")
    }
}

fn end_turn() -> AcpResult<PromptResponse> {
    Ok(PromptResponse::new(StopReason::EndTurn))
}

impl AcpAgent {
    fn say(&self, session: &AcpSession, text: impl Into<String>) {
        self.say_block(session, ContentBlock::Text(TextContent::new(text)));
    }

    fn say_block(&self, session: &AcpSession, block: ContentBlock) {
        let update = SessionUpdate::AgentMessageChunk(ContentChunk::new(block));
        self.client().session_update(SessionNotification::new(session.session_id.clone(), update));
    }

    pub async fn prompt(&self, params: PromptRequest) -> AcpResult<PromptResponse> {
        let session = self.restore_session(&params.session_id.0, None).await?;
        let (message, images) = prompt_to_pi_message(&params.prompt);
        if images.is_empty() && message.trim_start().starts_with('/') {
            let trimmed = message.trim();
            let (command, rest) = trimmed[1..].split_once(' ').unwrap_or((&trimmed[1..], ""));
            let args = parse_command_args(rest);
            if let Some(response) = self.builtin_command(&session, command, &args).await {
                return response;
            }
        }
        let title = prompt_to_session_title(&params.prompt);
        let (result, usage) = session.prompt_with_usage(message, images, title).await;
        let stop = match result? {
            TurnEnd::EndTurn => StopReason::EndTurn,
            TurnEnd::Cancelled => StopReason::Cancelled,
            TurnEnd::Error if session.was_cancel_requested() => StopReason::Cancelled,
            TurnEnd::Error => StopReason::EndTurn,
        };
        Ok(PromptResponse::new(stop).usage(usage))
    }

    /// Run a built-in command; `None` when `command` is not one.
    async fn builtin_command(
        &self,
        session: &AcpSession,
        command: &str,
        args: &[String],
    ) -> Option<AcpResult<PromptResponse>> {
        let backend = session.backend.as_ref();
        let first = args.first().map(|arg| arg.to_lowercase()).unwrap_or_default();
        let result = match command {
            "compact" => {
                let instructions = Some(args.join(" ").trim().to_string()).filter(|text| !text.is_empty());
                let custom = instructions.is_some();
                match backend.compact(instructions).await {
                    Ok(result) => {
                        let mut text = format!(
                            "Compaction completed.{}",
                            if custom { " (custom instructions applied)" } else { "" }
                        );
                        if let Some(before) = result["tokensBefore"].as_f64() {
                            text.push_str(&format!("\nTokens before: {before}"));
                        }
                        if let Some(summary) = result["summary"].as_str() {
                            text.push_str(&format!("\n\n{summary}"));
                        }
                        self.say(session, text);
                        end_turn()
                    }
                    Err(error) => Err(internal_failure(&error)),
                }
            }
            "session" => match backend.get_session_stats().await {
                Ok(stats) => {
                    self.say(session, format_session_stats(&stats));
                    end_turn()
                }
                Err(error) => Err(internal_failure(&error)),
            },
            "name" => {
                let name = args.join(" ").trim().to_string();
                if name.is_empty() {
                    self.say(session, "Usage: /name <name>");
                } else if let Err(error) = session.set_session_name(&name).await {
                    let message = format!("{error:#}");
                    let hint = if message.to_lowercase().contains("set_session_name") {
                        " This requires a newer pi version that supports `set_session_name` in RPC mode."
                    } else {
                        ""
                    };
                    self.say(session, format!("Failed to set session name: {message}{hint}"));
                } else {
                    self.say(session, format!("Session name set: {name}"));
                }
                end_turn()
            }
            "steering" | "follow-up" => self.queue_mode_command(session, command, &first).await,
            "changelog" => {
                let mut text = EMBEDDED_CHANGELOG.to_string();
                if text.chars().count() > CHANGELOG_MAX_CHARS {
                    text = text.chars().take(CHANGELOG_MAX_CHARS).collect::<String>() + "\n\n...(truncated)...";
                }
                self.say(session, text);
                end_turn()
            }
            "export" => self.export_command(session).await,
            "autocompact" => {
                let mode = if first.is_empty() { "toggle".to_string() } else { first };
                let enabled = match mode.as_str() {
                    "on" | "true" | "enable" | "enabled" => Some(true),
                    "off" | "false" | "disable" | "disabled" => Some(false),
                    _ => None,
                };
                let enabled = match enabled {
                    Some(enabled) => enabled,
                    None => match backend.get_state().await {
                        Ok(state) => !state["autoCompactionEnabled"].as_bool().unwrap_or(false),
                        Err(error) => return Some(Err(internal_failure(&error))),
                    },
                };
                match backend.set_auto_compaction(enabled).await {
                    Ok(()) => {
                        let word = if enabled { "enabled" } else { "disabled" };
                        self.say(session, format!("Auto-compaction {word}."));
                        end_turn()
                    }
                    Err(error) => Err(internal_failure(&error)),
                }
            }
            _ => return None,
        };
        Some(result)
    }

    /// `/steering` and `/follow-up`: show or set pi's queue delivery mode.
    async fn queue_mode_command(&self, session: &AcpSession, command: &str, mode: &str) -> AcpResult<PromptResponse> {
        let backend = session.backend.as_ref();
        let (label, key) =
            if command == "steering" { ("Steering", "steeringMode") } else { ("Follow-up", "followUpMode") };
        let state = backend.get_state().await.map_err(|error| internal_failure(&error))?;
        if mode.is_empty() {
            let current = state[key].as_str().filter(|mode| !mode.is_empty()).unwrap_or("unknown");
            self.say(session, format!("{label} mode: {current}"));
            return end_turn();
        }
        if mode != "all" && mode != "one-at-a-time" {
            self.say(session, format!("Usage: /{command} all | /{command} one-at-a-time"));
            return end_turn();
        }
        let set = if command == "steering" {
            backend.set_steering_mode(mode).await
        } else {
            backend.set_follow_up_mode(mode).await
        };
        set.map_err(|error| internal_failure(&error))?;
        self.say(session, format!("{label} mode set to: {mode}"));
        end_turn()
    }

    /// `/export`: write the session as HTML into its cwd and link it.
    async fn export_command(&self, session: &AcpSession) -> AcpResult<PromptResponse> {
        let backend = session.backend.as_ref();
        let state = backend.get_state().await.map_err(|error| internal_failure(&error))?;
        let file = state["sessionFile"].as_str().map(Path::new);
        let count = state["messageCount"].as_u64().unwrap_or(0);
        let Some(file) = file.filter(|file| count > 0 && file.exists()) else {
            self.say(session, "Nothing to export yet (no session messages). Send a prompt first.");
            return end_turn();
        };
        match std::fs::read_to_string(file) {
            Ok(raw) if raw.trim().is_empty() => {
                self.say(session, "Nothing to export yet (empty session file). Send a prompt first.");
                return end_turn();
            }
            Ok(_) => {}
            Err(_) => {
                self.say(session, "Couldn't read session file for export. Try sending a prompt first.");
                return end_turn();
            }
        }
        let safe_id: String = session
            .session_id
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
            .collect();
        let name = format!("pi-session-{safe_id}.html");
        let path = match backend.export_html(&session.cwd.join(&name)).await {
            Ok(path) => path.display().to_string(),
            Err(error) => {
                self.say(session, format!("Export failed: {error:#}"));
                return end_turn();
            }
        };
        if path.is_empty() {
            self.say(session, "Export failed: no output path returned by pi.");
            return end_turn();
        }
        self.say(session, "Session exported: ");
        let link = ResourceLink::new(name, format!("file://{path}")).mime_type("text/html").title("Session exported");
        self.say_block(session, ContentBlock::ResourceLink(link));
        end_turn()
    }
}
