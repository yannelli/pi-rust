//! Prompting (upstream `prompt`, `steer`, `followUp`, `sendCustomMessage`, `clearQueue`):
//! skill and template expansion, queueing while streaming, auth checks, image normalization,
//! and the system prompt update sent with the next request.

use std::sync::Arc;

use pi_ai::{ContentBlock, Message, UserContent, UserMessage};

use super::builtins::mcp::set_servers_section;
use super::{AgentSession, lock};
use crate::app::{format_no_api_key_found_message, oauth_failed_message};
use crate::compaction::{find_message_entry_id, find_tool_result_entry_ids};
use crate::images::{ProcessImageOptions, decode_base64_lenient, process_image};
use crate::messages::{CustomMessage, CustomRole};
use crate::resources::expand_user_input;
use crate::session::EntryData;

/// How a prompt sent while streaming is queued.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamingBehavior {
    /// Delivered after the current turn's tool calls, before the next request.
    Steer,
    /// Delivered when the agent would otherwise stop.
    FollowUp,
}

/// What happened to an accepted prompt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PromptDisposition {
    /// Consumed without a run (an extension command or input handler).
    Handled,
    /// Queued as steering or a follow-up.
    Queued,
    /// A run started.
    Started,
}

impl PromptDisposition {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Handled => "handled",
            Self::Queued => "queued",
            Self::Started => "started",
        }
    }
}

/// Options of [`AgentSession::prompt_with`].
#[derive(Clone)]
pub struct PromptOptions {
    /// Expand `/skill:name` and prompt templates (default true).
    pub expand_prompt_templates: bool,
    pub images: Vec<ContentBlock>,
    /// Required while streaming.
    pub streaming_behavior: Option<StreamingBehavior>,
    /// Called once the prompt is accepted, before the run, with its disposition.
    pub preflight_result: Option<Arc<dyn Fn(PromptDisposition) + Send + Sync>>,
    /// Where the input came from for `input` handlers: `interactive` (default), `rpc`, or
    /// `extension`.
    pub source: Option<String>,
}

impl Default for PromptOptions {
    fn default() -> Self {
        Self {
            expand_prompt_templates: true,
            images: Vec::new(),
            streaming_behavior: None,
            preflight_result: None,
            source: None,
        }
    }
}

/// The arguments of a `/mcp` command line, or `None` for other input.
fn mcp_command_args(text: &str) -> Option<&str> {
    let rest = text.trim().strip_prefix("/mcp")?;
    (rest.is_empty() || rest.starts_with(char::is_whitespace)).then(|| rest.trim())
}

/// A user message on the current branch with its entry (see [`AgentSession::last_user_prompt`]).
#[derive(Clone, Debug, PartialEq)]
pub struct BranchPrompt {
    pub entry_id: String,
    pub parent_id: Option<String>,
    pub message: UserMessage,
}

/// The message that starts a run, as `before_agent_start` handlers see it.
pub(crate) enum RunTrigger {
    /// A prompt, with its expanded text and images.
    User { prompt: String, images: Vec<ContentBlock>, from_extension: bool },
    /// A custom message sent with `triggerTurn` while idle.
    Custom(CustomMessage),
}

impl RunTrigger {
    /// The `messageType` fields of the `before_agent_start` event: `prompt` and `images` for a
    /// user message, `message` for a custom one.
    pub(crate) fn event_fields(&self) -> serde_json::Value {
        match self {
            Self::User { prompt, images, .. } => {
                let mut fields = serde_json::json!({"messageType": "user", "prompt": prompt});
                if !images.is_empty() {
                    fields["images"] = serde_json::json!(images);
                }
                fields
            }
            Self::Custom(message) => {
                let mut message = serde_json::to_value(message).unwrap_or_default();
                message["role"] = CustomMessage::ROLE.into();
                serde_json::json!({"messageType": "custom", "message": message})
            }
        }
    }
}

fn user_message(text: &str, images: Vec<ContentBlock>) -> Message {
    let mut blocks = vec![ContentBlock::text(text)];
    blocks.extend(images);
    Message::User(UserMessage { content: UserContent::Blocks(blocks), timestamp: pi_ai::now_ms() })
}

impl AgentSession {
    /// Send a prompt and wait until the run settles (retries and queued work included).
    pub async fn prompt(&self, text: &str, images: Vec<ContentBlock>) -> anyhow::Result<PromptDisposition> {
        self.prompt_with(text, PromptOptions { images, ..PromptOptions::default() }).await
    }

    pub async fn prompt_with(&self, text: &str, options: PromptOptions) -> anyhow::Result<PromptDisposition> {
        let source = options.source.clone().unwrap_or_else(|| "interactive".into());
        if source != "extension" {
            self.wait_extensions_bound().await;
        }
        // Extension commands run first, even while streaming; they send their own messages.
        if options.expand_prompt_templates && text.starts_with('/') && self.try_extension_command(text).await {
            if let Some(callback) = &options.preflight_result {
                callback(PromptDisposition::Handled);
            }
            return Ok(PromptDisposition::Handled);
        }
        if lock(&self.state).manual_compaction.is_some() {
            anyhow::bail!(
                "Cannot submit a prompt while compaction is in progress. Wait for compaction to finish and retry."
            );
        }
        let preflight = |disposition| {
            if let Some(callback) = &options.preflight_result {
                callback(disposition);
            }
        };
        if options.expand_prompt_templates
            && let Some(args) = mcp_command_args(text)
            && self.mcp_manager().is_some()
        {
            preflight(PromptDisposition::Handled);
            self.run_mcp_command(args).await;
            return Ok(PromptDisposition::Handled);
        }
        let from_extension = source == "extension";
        let behavior = if self.is_streaming() { options.streaming_behavior } else { None };
        let Some((text, input_images)) = self.run_input_handlers(text, options.images.clone(), &source, behavior).await
        else {
            preflight(PromptDisposition::Handled);
            return Ok(PromptDisposition::Handled);
        };
        let text = text.as_str();
        let expanded = if options.expand_prompt_templates { self.expand_input(text) } else { text.to_string() };
        if self.is_streaming() {
            let Some(behavior) = options.streaming_behavior else {
                anyhow::bail!(
                    "Agent is already processing. Specify streamingBehavior ('steer' or 'followUp') to queue the message."
                );
            };
            self.queue_message(&expanded, input_images, behavior, from_extension);
            preflight(PromptDisposition::Queued);
            return Ok(PromptDisposition::Queued);
        }
        if self.cost_limit_reached() {
            preflight(PromptDisposition::Handled);
            return Ok(PromptDisposition::Handled);
        }
        self.ensure_auth().await?;
        let trigger = RunTrigger::User { prompt: expanded, images: input_images, from_extension };
        self.start_run(trigger, || preflight(PromptDisposition::Started)).await?;
        Ok(PromptDisposition::Started)
    }

    /// Start a run from `trigger`. The session counts as busy from here on, so messages sent
    /// while `before_agent_start` handlers run join this run, and an abort requested meanwhile
    /// aborts the run as soon as it exists (upstream `_startRun`). `on_started` runs once the
    /// run's messages are prepared.
    pub(crate) async fn start_run(&self, trigger: RunTrigger, on_started: impl FnOnce()) -> anyhow::Result<()> {
        {
            let mut state = lock(&self.state);
            state.abort_requested = false;
            state.run_active = true;
        }
        self.file_checkpoints.begin_prompt(&self.session_id());
        self.notify_idle();
        let messages = self.prepare_run(&trigger).await;
        // Messages held while the run was starting go before its starting messages, as they would
        // have while idle. No run exists yet, so they cannot split a tool call from its result.
        self.flush_pending_bash_messages();
        self.flush_pending_custom_messages();
        on_started();
        self.run_agent_prompt(messages).await
    }

    /// The starting messages of a run: compact first when needed, emit `before_agent_start`,
    /// apply the run's prompt options and tool loadout, and build the starting message followed
    /// by pending `nextTurn` messages (consumed here) and handler messages, with a system patch
    /// first when the prompt changed (upstream `_prepareRun`).
    async fn prepare_run(&self, trigger: &RunTrigger) -> Vec<Message> {
        let last_assistant = self.with_state(|state| {
            state.messages.iter().rev().find_map(|message| match message {
                Message::Assistant(assistant) => Some(assistant.clone()),
                _ => None,
            })
        });
        if let Some(last) = last_assistant {
            // Recovery omits the response with its tool results, as after a run.
            let (entry_id, tool_results) = {
                let session = self.session_manager();
                let entry_id = find_message_entry_id(&session, &Message::Assistant(last.clone()));
                let results = entry_id.as_deref().map(|id| find_tool_result_entry_ids(&session, id, &last));
                (entry_id, results.unwrap_or_default())
            };
            self.check_compaction(&last, entry_id.as_deref(), false, &tool_results).await;
        }
        let servers_section = self.prepare_mcp_for_prompt().await;
        let mut prompt_options = self.effective_prompt_options();
        set_servers_section(&mut prompt_options, servers_section);
        // Before image normalization, so a model chosen by a handler sets the resize profile.
        let (prompt_options, extra) = self.run_before_agent_start(trigger, prompt_options).await;
        let mut messages = match trigger {
            RunTrigger::User { prompt, images, from_extension } => {
                let (images, hints) = self.normalize_prompt_images(images);
                let text = if hints.is_empty() { prompt.clone() } else { format!("{prompt}\n\n{}", hints.join("\n")) };
                let message = user_message(&text, images);
                if *from_extension {
                    self.note_extension_prompt(&message);
                }
                vec![message]
            }
            RunTrigger::Custom(message) => vec![message.clone().into_message()],
        };
        messages.append(&mut lock(&self.state).pending_next_turn);
        messages.extend(extra);
        let update = self.prepare_prompt_and_tool_loadout(&prompt_options, &self.messages());
        lock(&self.state).run_prompt = Some(prompt_options);
        if let Some(update) = update {
            messages.insert(0, Message::System(update));
        }
        messages
    }

    /// `/skill:name args` and `/template args` expansion; on an error the text is sent as is.
    pub fn expand_input(&self, text: &str) -> String {
        let services = self.services();
        let loader = services.resources();
        let resources = loader.resources();
        match expand_user_input(text, &resources.skills, &resources.prompts) {
            Ok(expanded) => expanded,
            Err(error) => {
                eprintln!("Warning: {error:#}");
                text.to_string()
            }
        }
    }

    async fn ensure_auth(&self) -> anyhow::Result<()> {
        let model = self.model().ok_or_else(|| anyhow::anyhow!(super::compaction::no_model_selected_message()))?;
        let runtime = self.model_runtime();
        if runtime.has_configured_auth(&model.provider) || runtime.check_auth(&model.provider).await {
            return Ok(());
        }
        if runtime.is_using_oauth(&model.provider).await {
            anyhow::bail!(oauth_failed_message(&model.provider));
        }
        anyhow::bail!(format_no_api_key_found_message(&model.provider))
    }

    /// Resize prompt images per `images.autoResize`; unusable images become hints.
    fn normalize_prompt_images(&self, images: &[ContentBlock]) -> (Vec<ContentBlock>, Vec<String>) {
        let auto_resize = self.settings().image_auto_resize();
        let options = match self.limits_model() {
            Some(model) => ProcessImageOptions::for_model(&model, auto_resize),
            None => ProcessImageOptions { auto_resize, ..ProcessImageOptions::default() },
        };
        let mut normalized = Vec::new();
        let mut hints = Vec::new();
        for image in images {
            let ContentBlock::Image { data, mime_type } = image else {
                normalized.push(image.clone());
                continue;
            };
            match process_image(&decode_base64_lenient(data), mime_type, &options) {
                Ok(processed) => {
                    normalized.push(ContentBlock::Image { data: processed.data, mime_type: processed.mime_type });
                    hints.extend(processed.hints);
                }
                Err(hint) => hints.push(hint),
            }
        }
        (normalized, hints)
    }

    /// Queue a steering message, delivered after the current turn's tool calls. Errors on an
    /// extension command; `input` handlers may handle it instead.
    pub async fn steer(&self, text: &str, images: Vec<ContentBlock>) -> anyhow::Result<PromptDisposition> {
        self.queue_user_input(text, images, StreamingBehavior::Steer, "interactive").await
    }

    /// Queue a follow-up, delivered when the agent has no more tool calls or steering.
    pub async fn follow_up(&self, text: &str, images: Vec<ContentBlock>) -> anyhow::Result<PromptDisposition> {
        self.queue_user_input(text, images, StreamingBehavior::FollowUp, "interactive").await
    }

    /// Upstream `_queueUserInput`: refuse extension commands, run `input` handlers with
    /// `source`, expand skills and templates, then queue.
    pub async fn queue_user_input(
        &self,
        text: &str,
        images: Vec<ContentBlock>,
        behavior: StreamingBehavior,
        source: &str,
    ) -> anyhow::Result<PromptDisposition> {
        if text.starts_with('/') {
            self.ensure_not_extension_command(text)?;
        }
        let streaming = self.is_streaming().then_some(behavior);
        let Some((text, images)) = self.run_input_handlers(text, images, source, streaming).await else {
            return Ok(PromptDisposition::Handled);
        };
        let expanded = self.expand_input(&text);
        self.queue_message(&expanded, images, behavior, source == "extension");
        Ok(PromptDisposition::Queued)
    }

    fn queue_message(&self, text: &str, images: Vec<ContentBlock>, behavior: StreamingBehavior, from_extension: bool) {
        {
            let mut state = lock(&self.state);
            match behavior {
                StreamingBehavior::Steer => state.steering.push(text.to_string()),
                StreamingBehavior::FollowUp => state.follow_up.push(text.to_string()),
            }
        }
        self.emit_queue_update();
        let message = user_message(text, images);
        if from_extension {
            self.note_extension_prompt(&message);
        }
        match behavior {
            StreamingBehavior::Steer => self.agent.steer(message),
            StreamingBehavior::FollowUp => self.agent.follow_up(message),
        }
    }

    /// Clear both queues and return their texts (steering, follow-up).
    pub fn clear_queue(&self) -> (Vec<String>, Vec<String>) {
        let cleared = {
            let mut state = lock(&self.state);
            (std::mem::take(&mut state.steering), std::mem::take(&mut state.follow_up))
        };
        self.agent.clear_queues();
        self.emit_queue_update();
        cleared
    }

    /// Steering and follow-up messages not delivered yet.
    pub fn pending_message_count(&self) -> usize {
        let state = lock(&self.state);
        state.steering.len() + state.follow_up.len()
    }

    pub fn steering_messages(&self) -> Vec<String> {
        lock(&self.state).steering.clone()
    }

    pub fn follow_up_messages(&self) -> Vec<String> {
        lock(&self.state).follow_up.clone()
    }

    /// Add a `custom` message to the conversation. While streaming it waits for the end of the
    /// current turn (or is queued when `trigger_turn`); otherwise `trigger_turn` starts a run.
    pub async fn send_custom_message(&self, message: CustomMessage, trigger_turn: bool) -> anyhow::Result<()> {
        let custom = message.clone();
        let message = message.into_message();
        if self.is_streaming() {
            if trigger_turn {
                self.agent.steer(message);
            } else {
                lock(&self.state).pending_custom.push(message);
            }
            return Ok(());
        }
        if trigger_turn {
            return self.start_run(RunTrigger::Custom(custom), || {}).await;
        }
        self.append_custom_message(message);
        Ok(())
    }

    /// Context for the next user prompt, sent along with it.
    pub fn queue_next_turn_message(&self, message: CustomMessage) {
        lock(&self.state).pending_next_turn.push(message.into_message());
    }

    fn note_extension_prompt(&self, message: &Message) {
        if let Message::User(user) = message {
            lock(&self.state).extension_prompts.push((user.timestamp, user.content.text()));
        }
    }

    /// The newest user message of the current branch that no extension sent while this session
    /// ran (pi-rust, for `/retry`). Custom messages are not user messages, so they never match.
    pub fn last_user_prompt(&self) -> Option<BranchPrompt> {
        let from_extensions = lock(&self.state).extension_prompts.clone();
        let sent_by_extension = |user: &UserMessage| {
            let mut sent = from_extensions.iter().filter(|(timestamp, _)| *timestamp == user.timestamp);
            sent.any(|(_, text)| *text == user.content.text())
        };
        let session = self.session_manager();
        session.get_branch(None).into_iter().rev().find_map(|entry| match entry.data() {
            EntryData::Message(Message::User(user)) if !sent_by_extension(user) => Some(BranchPrompt {
                entry_id: entry.id().to_string(),
                parent_id: entry.parent_id().map(String::from),
                message: user.clone(),
            }),
            _ => None,
        })
    }
}
