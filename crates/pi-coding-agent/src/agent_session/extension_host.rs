//! Extensions in a session (upstream `bindExtensions`, `_bindExtensionCore`, `sendCustomMessage`,
//! `sendUserMessage`, and the prompt hooks): the runner and its bindings, messages and commands
//! from extensions, and the context previews boundary handlers see.

use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex, RwLock};

use futures::FutureExt;
use futures::future::BoxFuture;
use pi_ai::{ContentBlock, Message, UserContent};
use serde_json::{Value, json};
use tokio::sync::watch;

use super::{AgentSession, BashResult, PromptOptions, StreamingBehavior, lock};
use crate::extensions::convert::{prompt_options_from_json, prompt_options_to_json, user_content};
use crate::extensions::{ExtensionBindings, ExtensionRunner};
use crate::messages::{CustomMessage, CustomRole};
use crate::resources::{ExtensionResourcePaths, PathMetadata, SourceOrigin, SourceScope};
use crate::system_prompt::BuildSystemPromptOptions;

/// Extension state of a session.
pub(crate) struct ExtensionState {
    runner: RwLock<Option<Arc<ExtensionRunner>>>,
    /// The `session_start` event the next bind emits.
    start_event: Mutex<Value>,
    /// False while a bind runs; prompts from modes wait for it.
    bound: watch::Sender<bool>,
    /// Whether `session_start` was emitted; later binds only replace the bindings.
    started: std::sync::atomic::AtomicBool,
    pub(crate) turn_index: AtomicU64,
    /// Timestamps of assistant messages whose `turn_end` ran from `finish_turn`.
    pub(crate) dispatched: Mutex<Vec<i64>>,
    pub(crate) outcome: Mutex<&'static str>,
    /// What the mode bound, kept for extensions loaded later (reload, replacement).
    bindings: Mutex<Option<ExtensionBindings>>,
}

impl Default for ExtensionState {
    fn default() -> Self {
        Self {
            runner: RwLock::default(),
            start_event: Mutex::new(json!({"type": "session_start", "reason": "startup"})),
            bound: watch::channel(true).0,
            started: std::sync::atomic::AtomicBool::new(false),
            turn_index: AtomicU64::new(0),
            dispatched: Mutex::default(),
            outcome: Mutex::new("completed"),
            bindings: Mutex::default(),
        }
    }
}

/// `extension:<file name>` (upstream `getExtensionSourceLabel`).
fn extension_source_label(extension_path: &str) -> String {
    if extension_path.starts_with('<') || extension_path.starts_with("builtin:") {
        return format!("extension:{}", extension_path.replace(['<', '>'], ""));
    }
    let name = std::path::Path::new(extension_path).file_name().map(|name| name.to_string_lossy().into_owned());
    let name = name.unwrap_or_default();
    let name = name.strip_suffix(".ts").or_else(|| name.strip_suffix(".js")).unwrap_or(&name).to_string();
    format!("extension:{name}")
}

fn resource_paths(found: &Value, key: &str) -> Vec<(PathBuf, PathMetadata)> {
    let entries = found[key].as_array().cloned().unwrap_or_default();
    entries
        .iter()
        .filter_map(|entry| {
            let path = entry["path"].as_str()?;
            let extension_path = entry["extensionPath"].as_str().unwrap_or_default();
            let mut metadata = PathMetadata::new(
                &extension_source_label(extension_path),
                SourceScope::Temporary,
                SourceOrigin::TopLevel,
            );
            if !extension_path.starts_with('<') {
                metadata.base_dir = std::path::Path::new(extension_path).parent().map(PathBuf::from);
            }
            Some((PathBuf::from(path), metadata))
        })
        .collect()
}

/// `cache_warming_decision` handlers: the action they settle on, when any is registered.
fn cache_warming_decider(runner: &Arc<ExtensionRunner>) -> super::CacheWarmingDecider {
    let runner = Arc::downgrade(runner);
    Arc::new(move |event: crate::cache_warmer::CacheWarmingDecisionEvent| {
        let runner = runner.upgrade().filter(|runner| runner.has_handlers("cache_warming_decision"));
        async move {
            let action = runner?.call_json("emitCacheWarmingDecision", vec![event.to_json()]).await.ok()?;
            match action.as_str()? {
                "warm" => Some(crate::cache_warmer::CacheWarmingAction::Warm),
                "stop" => Some(crate::cache_warmer::CacheWarmingAction::Stop),
                _ => None,
            }
        }
        .boxed()
    })
}

impl AgentSession {
    /// The extensions of this session, when it has any.
    pub fn extension_runner(&self) -> Option<Arc<ExtensionRunner>> {
        self.extensions.runner.read().unwrap_or_else(|poisoned| poisoned.into_inner()).clone()
    }

    /// Attach loaded extensions: their tools register and their actions reach this session.
    pub async fn attach_extensions(&self, runner: Arc<ExtensionRunner>) {
        let Some(this) = self.weak.upgrade() else { return };
        runner.set_session(&this);
        *self.extensions.runner.write().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(runner.clone());
        // Built-in tools an extension replaces leave before its tools register.
        self.install_builtin_tools();
        runner.sync_tools();
        let _ = runner.call_json("bind", Vec::new()).await;
        self.install_request_hooks(&runner);
        self.set_cache_warming_decider(Some(cache_warming_decider(&runner)));
    }

    /// The `session_start` event the next [`Self::bind_extensions`] emits.
    pub(crate) fn set_session_start_event(&self, event: Value) {
        *lock(&self.extensions.start_event) = event;
    }

    /// Bind the mode's UI and handlers, then emit `session_start`, report MCP servers nothing
    /// connects, and add resources from `resources_discover` (upstream `bindExtensions`).
    pub async fn bind_extensions(&self, bindings: ExtensionBindings) {
        *lock(&self.extensions.bindings) = Some(bindings.clone());
        let Some(runner) = self.extension_runner() else { return };
        runner.set_bindings(bindings);
        if self.extensions.started.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        self.extensions.bound.send_replace(false);
        let event = lock(&self.extensions.start_event).clone();
        let reason = if event["reason"] == "reload" { "reload" } else { "startup" };
        runner.emit(event).await;
        runner.report_unconnected_mcp();
        self.discover_extension_resources(&runner, reason).await;
        self.extensions.bound.send_replace(true);
    }

    /// What the mode last bound with [`Self::bind_extensions`].
    pub fn extension_bindings(&self) -> Option<ExtensionBindings> {
        lock(&self.extensions.bindings).clone()
    }

    /// Make prompts wait for a [`Self::bind_extensions`] that is about to start.
    pub fn mark_binding_extensions(&self) {
        if self.extension_runner().is_some() && !self.extensions.started.load(std::sync::atomic::Ordering::SeqCst) {
            self.extensions.bound.send_replace(false);
        }
    }

    /// Wait until a running [`Self::bind_extensions`] emitted `session_start`.
    pub(crate) async fn wait_extensions_bound(&self) {
        let mut receiver = self.extensions.bound.subscribe();
        let _ = receiver.wait_for(|bound| *bound).await;
    }

    async fn discover_extension_resources(&self, runner: &ExtensionRunner, reason: &str) {
        if !runner.has_handlers("resources_discover") {
            return;
        }
        let cwd = json!(self.cwd().display().to_string());
        let Ok(found) = runner.call_json("emitResourcesDiscover", vec![cwd, json!(reason)]).await else { return };
        let paths = ExtensionResourcePaths {
            skill_paths: resource_paths(&found, "skillPaths"),
            prompt_paths: resource_paths(&found, "promptPaths"),
            theme_paths: resource_paths(&found, "themePaths"),
        };
        if paths.skill_paths.is_empty() && paths.prompt_paths.is_empty() && paths.theme_paths.is_empty() {
            return;
        }
        self.services().resources().extend_resources(paths);
        self.rebuild_system_prompt();
    }

    /// Use the catalog entry of the current model after providers changed.
    pub(crate) fn refresh_model_from_registry(&self) {
        let Some(model) = self.model() else { return };
        if let Some(refreshed) = self.model_runtime().get_model(&model.provider, &model.id)
            && refreshed != model
        {
            self.agent.update_state(|state| state.model = refreshed);
        }
    }

    /// `pi.sendMessage`: delivery happens now; a returned run starts a turn.
    pub(crate) fn send_extension_message(
        self: &Arc<Self>,
        message: Value,
        options: &Value,
    ) -> Result<Option<BoxFuture<'static, anyhow::Result<()>>>, String> {
        let mut custom: CustomMessage =
            serde_json::from_value(message).map_err(|error| format!("Invalid message: {error}"))?;
        custom.timestamp = pi_ai::now_ms();
        let message = custom.into_message();
        let deliver_as = options["deliverAs"].as_str();
        let trigger = options["triggerTurn"].as_bool();
        if deliver_as == Some("nextTurn") {
            lock(&self.state).pending_next_turn.push(message);
        } else if self.is_streaming() && trigger != Some(false) {
            match deliver_as {
                Some("followUp") => self.agent.follow_up(message),
                _ => self.agent.steer(message),
            }
        } else if trigger == Some(true) {
            let session = self.clone();
            return Ok(Some(async move { session.run_agent_prompt(vec![message]).await }.boxed()));
        } else if self.is_streaming() {
            lock(&self.state).pending_custom.push(message);
        } else {
            self.append_custom_message(message);
        }
        Ok(None)
    }

    /// `pi.sendUserMessage`: a prompt from an extension (upstream `sendUserMessage`).
    pub(crate) async fn send_extension_user_message(&self, content: Value, options: &Value) -> anyhow::Result<()> {
        let (text, images) = user_content(&content);
        let streaming_behavior = match options["deliverAs"].as_str() {
            Some("followUp") => Some(StreamingBehavior::FollowUp),
            Some("steer") => Some(StreamingBehavior::Steer),
            _ => None,
        };
        let options = PromptOptions {
            expand_prompt_templates: options["expandPromptTemplates"].as_bool().unwrap_or(false),
            images,
            streaming_behavior,
            source: Some("extension".into()),
            ..PromptOptions::default()
        };
        self.prompt_with(&text, options).await.map(|_| ())
    }

    /// Slash commands as `pi.getCommands()` lists them: extension commands, then prompt
    /// templates, then skills.
    pub fn extension_command_infos(&self) -> Value {
        let mut commands: Vec<Value> = Vec::new();
        if let Some(runner) = self.extension_runner() {
            for command in &runner.registry().commands {
                commands.push(json!({
                    "name": command.invocation_name,
                    "description": command.description,
                    "source": "extension",
                    "sourceInfo": command.source_info,
                }));
            }
        }
        let loader = self.services().resources();
        let resources = loader.resources();
        for template in &resources.prompts {
            commands.push(json!({
                "name": template.name,
                "description": template.description,
                "source": "prompt",
                "sourceInfo": template.source_info,
            }));
        }
        for skill in &resources.skills {
            commands.push(json!({
                "name": format!("skill:{}", skill.name),
                "description": skill.description,
                "source": "skill",
                "sourceInfo": skill.source_info,
            }));
        }
        Value::Array(commands)
    }

    /// `ctx.getSystemPromptOptions()`.
    pub(crate) fn extension_prompt_options(&self) -> Value {
        prompt_options_to_json(&self.effective_prompt_options())
    }

    /// Run `/name args` when an extension registered `name`; true when it did.
    pub(crate) async fn try_extension_command(&self, text: &str) -> bool {
        let Some(runner) = self.extension_runner() else { return false };
        let body = &text[1..];
        let (name, args) = body.split_once(' ').unwrap_or((body, ""));
        if runner.registry().command(name).is_none() {
            return false;
        }
        matches!(runner.call_json("runCommand", vec![json!(name), json!(args)]).await, Ok(Value::Bool(true)))
    }

    /// Upstream `_throwIfExtensionCommand`: extension commands run, they are never queued.
    pub(crate) fn ensure_not_extension_command(&self, text: &str) -> anyhow::Result<()> {
        let Some(runner) = self.extension_runner() else { return Ok(()) };
        let body = text.strip_prefix('/').unwrap_or(text);
        let name = body.split_once(' ').map_or(body, |(name, _)| name);
        if runner.registry().command(name).is_some() {
            anyhow::bail!(
                "Extension command \"/{name}\" cannot be queued. Use prompt() or execute the command when not streaming."
            );
        }
        Ok(())
    }

    /// `input` handlers: `None` when one handled the input, else the text and images to use.
    pub(crate) async fn run_input_handlers(
        &self,
        text: &str,
        images: Vec<ContentBlock>,
        source: &str,
        behavior: Option<StreamingBehavior>,
    ) -> Option<(String, Vec<ContentBlock>)> {
        let Some(runner) = self.extension_runner().filter(|runner| runner.has_handlers("input")) else {
            return Some((text.to_string(), images));
        };
        let behavior = behavior.map(|behavior| match behavior {
            StreamingBehavior::Steer => "steer",
            StreamingBehavior::FollowUp => "followUp",
        });
        let images_json = if images.is_empty() { Value::Null } else { json!(images) };
        let args = vec![json!(text), images_json, json!(source), json!(behavior)];
        let Ok(result) = runner.call_json("emitInput", args).await else { return Some((text.to_string(), images)) };
        match result["action"].as_str() {
            Some("handled") => None,
            Some("transform") => {
                let text = result["text"].as_str().unwrap_or(text).to_string();
                let images = serde_json::from_value(result["images"].clone()).unwrap_or(images);
                Some((text, images))
            }
            _ => Some((text.to_string(), images)),
        }
    }

    /// `before_agent_start`: prompt options to run with and custom messages to send along.
    pub(crate) async fn run_before_agent_start(
        &self,
        prompt: &str,
        images: &[ContentBlock],
        options: BuildSystemPromptOptions,
    ) -> (BuildSystemPromptOptions, Vec<Message>) {
        let Some(runner) = self.extension_runner().filter(|runner| runner.has_handlers("before_agent_start")) else {
            return (options, Vec::new());
        };
        let images_json = if images.is_empty() { Value::Null } else { json!(images) };
        let args = vec![json!(prompt), images_json, prompt_options_to_json(&options)];
        let Ok(result) = runner.call_json("emitBeforeAgentStart", args).await else { return (options, Vec::new()) };
        let mut edited = prompt_options_from_json(&result["systemPromptOptions"], self.cwd());
        // An explicit edit of `selectedTools` wins; otherwise the live loadout stays.
        if edited.selected_tools == options.selected_tools {
            edited.selected_tools = self.active_tool_names();
        }
        let messages = result["messages"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|message| {
                let content =
                    serde_json::from_value(message["content"].clone()).unwrap_or(UserContent::Blocks(Vec::new()));
                CustomMessage {
                    custom_type: message["customType"].as_str().unwrap_or_default().to_string(),
                    content,
                    display: message["display"].as_bool().unwrap_or(false),
                    details: Some(message["details"].clone()).filter(|details| !details.is_null()),
                    timestamp: pi_ai::now_ms(),
                }
                .into_message()
            })
            .collect();
        (edited, messages)
    }

    /// `user_bash` handlers (`!cmd`): a result one of them produced, or the command run through
    /// the operations one supplied; `None` runs it locally. The result is not recorded yet.
    pub async fn extension_user_bash(&self, command: &str, exclude_from_context: bool) -> Option<BashResult> {
        self.extension_user_bash_with(command, exclude_from_context, &|_| {}).await
    }

    /// [`Self::extension_user_bash`] that also passes output to `on_chunk` as it arrives.
    pub async fn extension_user_bash_with(
        &self,
        command: &str,
        exclude_from_context: bool,
        on_chunk: &(dyn Fn(&str) + Send + Sync),
    ) -> Option<BashResult> {
        let runner = self.extension_runner().filter(|runner| runner.has_handlers("user_bash"))?;
        let cwd = self.session_manager().cwd().to_path_buf();
        let event = json!({
            "type": "user_bash",
            "command": command,
            "excludeFromContext": exclude_from_context,
            "cwd": cwd.display().to_string(),
        });
        let failed = |output: String| BashResult {
            output,
            exit_code: Some(1),
            cancelled: false,
            truncated: false,
            full_output_path: None,
        };
        let value = match runner.call_json("emitUserBash", vec![event]).await {
            Ok(value) => value,
            Err(error) => return Some(failed(error.message)),
        };
        if let Some(result) = value.get("result").filter(|result| result.is_object()) {
            return Some(BashResult {
                output: result["output"].as_str().unwrap_or_default().to_string(),
                exit_code: result["exitCode"].as_i64().map(|code| code as i32),
                cancelled: result["cancelled"].as_bool().unwrap_or(false),
                truncated: result["truncated"].as_bool().unwrap_or(false),
                full_output_path: result["fullOutputPath"].as_str().map(PathBuf::from),
            });
        }
        let key = value["operations"].as_u64()?;
        let cancel = tokio_util::sync::CancellationToken::new();
        let slot = {
            let mut state = lock(&self.state);
            state.next_bash += 1;
            let slot = state.next_bash;
            state.bash.push((slot, cancel.clone()));
            slot
        };
        let forward = |delta: &str| {
            on_chunk(delta);
            self.emit(&super::AgentSessionEvent::BashExecutionUpdate { id: None, delta: delta.to_string() });
        };
        let result = crate::extensions::shell::run_user_bash(&runner, key, command, &cwd, cancel, &forward).await;
        lock(&self.state).bash.retain(|(running, _)| *running != slot);
        Some(result)
    }

    /// Unload extensions after `session_shutdown` (reload, replacement, or quit).
    pub(crate) async fn shutdown_extensions(&self, reason: &str, target: Option<&str>) {
        self.emit_session_shutdown(reason, target).await;
        self.invalidate_extensions().await;
    }

    /// `session_shutdown` to the extensions (upstream `emitSessionShutdownEvent`).
    pub(crate) async fn emit_session_shutdown(&self, reason: &str, target: Option<&str>) {
        let Some(runner) = self.extension_runner() else { return };
        if runner.has_handlers("session_shutdown") {
            let mut event = json!({"type": "session_shutdown", "reason": reason});
            if let Some(target) = target {
                event["targetSessionFile"] = json!(target);
            }
            runner.emit(event).await;
        }
    }

    /// Make the extension contexts of this session stale.
    pub(crate) async fn invalidate_extensions(&self) {
        if let Some(runner) = self.extension_runner() {
            runner.invalidate(None).await;
        }
    }

    /// Unload and load the extensions again (upstream `reload`).
    pub async fn reload_extensions(&self) -> anyhow::Result<()> {
        let old = self.extension_runner();
        let bindings = self.extension_bindings();
        let inline = old.as_ref().map(|runner| lock(&runner.inline).clone()).unwrap_or_default();
        let flags = match &old {
            Some(runner) => runner.call_json("flagValues", Vec::new()).await.unwrap_or(Value::Null),
            None => Value::Null,
        };
        self.shutdown_extensions("reload", None).await;
        if let Some(old) = &old {
            old.unregister_models();
            old.remove_tools();
            old.shutdown();
        }
        *self.extensions.runner.write().unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
        let package_error = self.reload_resources().await;
        let flags = flags.as_object().map(|flags| flags.clone().into_iter().collect()).unwrap_or_default();
        let options = crate::extensions::LoadOptions { flag_values: flags, inline, ..Default::default() };
        let loaded = crate::extensions::load_extensions(self.services(), &options).await;
        for diagnostic in package_error.iter().chain(&loaded.diagnostics) {
            if let Some(bindings) = &bindings
                && let Some(on_error) = &bindings.on_error
            {
                on_error(&crate::extensions::ExtensionError {
                    extension_path: "<reload>".into(),
                    event: "load".into(),
                    error: diagnostic.message.clone(),
                    stack: None,
                });
            }
        }
        if let Some(runner) = loaded.runner {
            self.attach_extensions(runner).await;
            self.set_session_start_event(json!({"type": "session_start", "reason": "reload"}));
            self.extensions.started.store(false, std::sync::atomic::Ordering::SeqCst);
            if let Some(bindings) = bindings {
                self.bind_extensions(bindings).await;
            }
        }
        Ok(())
    }
}
