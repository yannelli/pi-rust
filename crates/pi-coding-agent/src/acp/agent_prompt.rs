//! `session/prompt` (upstream `PiAcpAgent.prompt`): the built-in slash commands ACP answers
//! itself, then the prompt turn.

use std::path::Path;

use agent_client_protocol_schema::{
    ContentBlock, ContentChunk, PromptRequest, PromptResponse, ResourceLink, SessionNotification, SessionUpdate,
    StopReason, TextContent,
};

use super::agent::AcpAgent;
use super::commands::parse_command_args;
use super::errors::{AcpResult, internal_failure};
use super::session::{AcpSession, TurnEnd};
use super::translate::{prompt_to_pi_message, prompt_to_session_title};
use crate::headless_commands::{
    HEADLESS_COMMANDS, changelog_text, export_blocker, export_file_name, format_session_stats,
};

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
            if HEADLESS_COMMANDS.iter().any(|builtin| builtin.name == command)
                && let Some(response) = self.headless_command(&session, command, rest.trim()).await
            {
                return response;
            }
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
                self.say(session, changelog_text());
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

    /// A command of [`HEADLESS_COMMANDS`]; after a reload the client gets the new command list.
    async fn headless_command(
        &self,
        session: &AcpSession,
        command: &str,
        args: &str,
    ) -> Option<AcpResult<PromptResponse>> {
        let output = session.backend.run_headless_command(command, args).await?;
        let _ = session.publish_config_options(false).await;
        self.say(session, output.text);
        if output.reloaded {
            self.advertise_commands(session, &session.cwd).await;
        }
        Some(end_turn())
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
        if let Some(blocker) = export_blocker(file, count) {
            self.say(session, blocker);
            return end_turn();
        }
        let name = export_file_name(&session.session_id);
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
