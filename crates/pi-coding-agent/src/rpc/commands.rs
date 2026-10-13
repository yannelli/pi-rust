//! Command handlers (upstream `handleCommand` in `rpc-mode.ts`), in the order of
//! `docs/rpc-commands.md`. Each returns the response, or `None` when it answers by itself.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use pi_ai::ContentBlock;
use serde_json::{Value, json};

use super::server::{Reply, RpcServer, run_eagerly};
use super::tree::tree_data_json;
use super::types::{
    RpcBashResult, RpcCommand, RpcCommandSource, RpcSessionState, RpcSlashCommand, RpcStreamingBehavior, error,
    success, success_line_with_raw_data,
};
use crate::agent_session::{AgentSession, ForkPosition, PromptOptions, compaction_result_json};

fn ok(id: Option<&Value>, command: &str, data: Option<Value>) -> anyhow::Result<Option<Reply>> {
    Ok(Some(Reply::Json(success(id, command, data))))
}

fn failed(id: Option<&Value>, command: &str, message: &str) -> anyhow::Result<Option<Reply>> {
    Ok(Some(Reply::Json(error(id, Some(&json!(command)), message))))
}

fn to_json(value: impl serde::Serialize) -> Value {
    serde_json::to_value(value).unwrap_or(Value::Null)
}

/// `get_state` data for `session`.
pub fn session_state(session: &AgentSession) -> RpcSessionState {
    RpcSessionState {
        model: session.model(),
        thinking_level: session.thinking_level(),
        is_streaming: session.is_streaming(),
        is_compacting: session.is_compacting(),
        steering_mode: session.steering_mode(),
        follow_up_mode: session.follow_up_mode(),
        session_file: session.session_file().map(|path| path.display().to_string()),
        session_id: session.session_id(),
        session_name: session.session_name(),
        auto_compaction_enabled: session.auto_compaction_enabled(),
        message_count: session.with_state(|state| state.messages.len()),
        pending_message_count: session.pending_message_count(),
    }
}

/// Prompt templates and skills as `get_commands` entries (extension commands come first).
pub fn resource_commands(session: &AgentSession) -> Vec<RpcSlashCommand> {
    let services = session.services();
    let loader = services.resources();
    let resources = loader.resources();
    let templates = resources.prompts.iter().map(|template| RpcSlashCommand {
        name: template.name.clone(),
        description: Some(template.description.clone()),
        source: RpcCommandSource::Prompt,
        source_info: template.source_info.clone(),
    });
    let skills = resources.skills.iter().map(|skill| RpcSlashCommand {
        name: format!("skill:{}", skill.name),
        description: Some(skill.description.clone()),
        source: RpcCommandSource::Skill,
        source_info: skill.source_info.clone(),
    });
    templates.chain(skills).collect()
}

impl RpcServer {
    pub(super) async fn handle_command(
        &self,
        id: Option<&Value>,
        command: RpcCommand,
    ) -> anyhow::Result<Option<Reply>> {
        let session = self.runtime.session();
        match command {
            RpcCommand::Prompt { message, images, streaming_behavior } => {
                self.start_prompt(session, id, message, images.unwrap_or_default(), streaming_behavior).await;
                Ok(None)
            }
            RpcCommand::Steer { message, images } => {
                let disposition = session.steer(&message, images.unwrap_or_default());
                ok(id, "steer", Some(json!({"disposition": disposition.as_str()})))
            }
            RpcCommand::FollowUp { message, images } => {
                let disposition = session.follow_up(&message, images.unwrap_or_default());
                ok(id, "follow_up", Some(json!({"disposition": disposition.as_str()})))
            }
            RpcCommand::Abort => {
                session.abort().await;
                ok(id, "abort", None)
            }
            RpcCommand::ClearQueue => {
                let (steering, follow_up) = session.clear_queue();
                ok(id, "clear_queue", Some(json!({"steering": steering, "followUp": follow_up})))
            }
            RpcCommand::NewSession { parent_session } => {
                self.runtime.new_session(parent_session, None).await?;
                ok(id, "new_session", Some(json!({"cancelled": false})))
            }
            RpcCommand::GetState => ok(id, "get_state", Some(to_json(session_state(&session)))),
            RpcCommand::GetMessages => ok(id, "get_messages", Some(json!({"messages": session.messages()}))),
            RpcCommand::SetModel { provider, model_id } => {
                let available = session.model_runtime().available_models();
                let Some(model) =
                    available.into_iter().find(|model| model.provider == provider && model.id == model_id)
                else {
                    return failed(id, "set_model", &format!("Model not found: {provider}/{model_id}"));
                };
                session.set_model(model.clone(), false).await?;
                ok(id, "set_model", Some(to_json(model)))
            }
            RpcCommand::CycleModel => {
                let data = session.cycle_model(false, false).map_or(Value::Null, |result| {
                    json!({"model": result.model, "thinkingLevel": result.thinking_level, "isScoped": result.is_scoped})
                });
                ok(id, "cycle_model", Some(data))
            }
            RpcCommand::GetAvailableModels => {
                let models = session.model_runtime().available_models();
                ok(id, "get_available_models", Some(json!({"models": models})))
            }
            RpcCommand::SetThinkingLevel { level } => {
                session.set_thinking_level(level, false);
                ok(id, "set_thinking_level", None)
            }
            RpcCommand::CycleThinkingLevel => {
                let data = session.cycle_thinking_level(false).map_or(Value::Null, |level| json!({"level": level}));
                ok(id, "cycle_thinking_level", Some(data))
            }
            RpcCommand::GetAvailableThinkingLevels => {
                let levels = session.available_thinking_levels();
                ok(id, "get_available_thinking_levels", Some(json!({"levels": levels})))
            }
            RpcCommand::SetSteeringMode { mode } => {
                session.set_steering_mode(mode);
                ok(id, "set_steering_mode", None)
            }
            RpcCommand::SetFollowUpMode { mode } => {
                session.set_follow_up_mode(mode);
                ok(id, "set_follow_up_mode", None)
            }
            RpcCommand::Compact { custom_instructions } => {
                let result = session.compact(custom_instructions.as_deref()).await?;
                ok(id, "compact", Some(compaction_result_json(&result)))
            }
            RpcCommand::SetAutoCompaction { enabled } => {
                session.set_auto_compaction_enabled(enabled);
                ok(id, "set_auto_compaction", None)
            }
            RpcCommand::SetAutoRetry { enabled } => {
                session.set_auto_retry_enabled(enabled);
                ok(id, "set_auto_retry", None)
            }
            RpcCommand::AbortRetry => {
                session.abort_retry();
                ok(id, "abort_retry", None)
            }
            RpcCommand::Bash { command, exclude_from_context } => {
                let result = self.bash(&session, id, &command, exclude_from_context.unwrap_or(false)).await?;
                ok(id, "bash", Some(to_json(result)))
            }
            RpcCommand::AbortBash => {
                session.abort_bash();
                ok(id, "abort_bash", None)
            }
            command => self.handle_session_command(&session, id, command).await,
        }
    }

    /// Session file and tree commands.
    async fn handle_session_command(
        &self,
        session: &Arc<AgentSession>,
        id: Option<&Value>,
        command: RpcCommand,
    ) -> anyhow::Result<Option<Reply>> {
        match command {
            RpcCommand::GetSessionStats => ok(id, "get_session_stats", Some(session.session_stats().to_json())),
            RpcCommand::ExportHtml { output_path } => {
                let path = session.export_to_html(output_path.as_deref())?;
                ok(id, "export_html", Some(json!({"path": path.display().to_string()})))
            }
            RpcCommand::SwitchSession { session_path } => {
                self.runtime.switch_session(Path::new(&session_path), None).await?;
                ok(id, "switch_session", Some(json!({"cancelled": false})))
            }
            RpcCommand::Fork { entry_id } => {
                let result = self.runtime.fork(&entry_id, ForkPosition::Before).await?;
                let mut data = json!({"cancelled": result.cancelled});
                if let Some(text) = result.selected_text {
                    data = json!({"text": text, "cancelled": result.cancelled});
                }
                ok(id, "fork", Some(data))
            }
            RpcCommand::CloneSession => {
                let result = self.runtime.clone_session().await?;
                ok(id, "clone", Some(json!({"cancelled": result.cancelled})))
            }
            RpcCommand::GetForkMessages => {
                let messages: Vec<Value> = session
                    .user_messages_for_forking()
                    .into_iter()
                    .map(|(entry_id, text)| json!({"entryId": entry_id, "text": text}))
                    .collect();
                ok(id, "get_fork_messages", Some(json!({"messages": messages})))
            }
            RpcCommand::GetEntries { since } => {
                let manager = session.session_manager();
                let entries = manager.entries();
                let start = match &since {
                    None => 0,
                    Some(since) => match entries.iter().position(|entry| entry.id() == since) {
                        Some(index) => index + 1,
                        None => return failed(id, "get_entries", &format!("Entry not found: {since}")),
                    },
                };
                let entries: Vec<Value> =
                    entries[start..].iter().map(|entry| Value::Object(entry.raw().clone())).collect();
                ok(id, "get_entries", Some(json!({"entries": entries, "leafId": manager.leaf_id()})))
            }
            RpcCommand::GetTree => {
                let data = tree_data_json(&session.session_manager());
                Ok(Some(Reply::Line(success_line_with_raw_data(id, "get_tree", &data))))
            }
            RpcCommand::GetLastAssistantText => {
                ok(id, "get_last_assistant_text", Some(json!({"text": session.last_assistant_text()})))
            }
            RpcCommand::SetSessionName { name } => {
                let name = name.trim();
                if name.is_empty() {
                    return failed(id, "set_session_name", "Session name cannot be empty");
                }
                session.set_session_name(name)?;
                ok(id, "set_session_name", None)
            }
            RpcCommand::GetCommands => {
                let mut commands = super::builtins::builtin_commands(session);
                if let Some(host) = &self.host {
                    commands.extend(host.commands(session));
                }
                commands.extend(resource_commands(session));
                ok(id, "get_commands", Some(json!({"commands": commands})))
            }
            _ => unreachable!("handle_command answers every other command"),
        }
    }

    /// Start a prompt; its response goes out once the session accepts or rejects it, and the
    /// run continues in the background.
    async fn start_prompt(
        &self,
        session: Arc<AgentSession>,
        id: Option<&Value>,
        message: String,
        images: Vec<ContentBlock>,
        streaming_behavior: Option<RpcStreamingBehavior>,
    ) {
        let accepted = Arc::new(AtomicBool::new(false));
        let (flag, output, response_id) = (accepted.clone(), self.output.clone(), id.cloned());
        let options = PromptOptions {
            images,
            streaming_behavior: streaming_behavior.map(Into::into),
            preflight_result: Some(Arc::new(move |disposition: crate::agent_session::PromptDisposition| {
                flag.store(true, Ordering::SeqCst);
                let data = json!({"disposition": disposition.as_str()});
                output.send(&success(response_id.as_ref(), "prompt", Some(data)));
            })),
            ..PromptOptions::default()
        };
        let (output, id) = (self.output.clone(), id.cloned());
        let run = async move {
            if let Err(failure) = session.prompt_with(&message, options).await
                && !accepted.load(Ordering::SeqCst)
            {
                output.send(&error(id.as_ref(), Some(&json!("prompt")), &format!("{failure:#}")));
            }
        };
        run_eagerly(Box::pin(run)).await;
    }

    async fn bash(
        &self,
        session: &AgentSession,
        id: Option<&Value>,
        command: &str,
        exclude_from_context: bool,
    ) -> anyhow::Result<RpcBashResult> {
        if let Some(host) = &self.host
            && let Some(result) = host.user_bash(session, command, exclude_from_context).await
        {
            session.record_bash_result(command, &result, exclude_from_context);
            return Ok(RpcBashResult::from(&result));
        }
        let event_id = id.map(|id| id.as_str().map_or_else(|| id.to_string(), String::from));
        let result = session.execute_bash(command, exclude_from_context, event_id.as_deref(), &mut |_| {}).await?;
        Ok(RpcBashResult::from(&result))
    }
}
