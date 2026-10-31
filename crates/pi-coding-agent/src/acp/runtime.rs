//! [`PiBackend`] over an in-process [`AgentSessionRuntime`], using the same session APIs and
//! JSON shapes as RPC mode, and the factory that opens one runtime per ACP session.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use agent_client_protocol_schema::Error;
use async_trait::async_trait;
use futures::FutureExt;
use pi_agent::{AgentEvent, QueueMode};
use pi_ai::{AssistantMessageEvent, ContentBlock, ThinkingLevel};
use serde_json::{Value, json};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use super::backend::{AcpClient, BackendFactory, PiBackend};
use super::errors::internal_error;
use super::permissions::{PermissionMode, ToolPermissions};
use crate::agent_session::{
    AgentSession, AgentSessionEvent, AgentSessionRuntime, DiagnosticLevel, PromptOptions, compaction_result_json, lock,
};
use crate::app::{AppOptions, create_app_runtime};
use crate::rpc::{
    RpcExtensionBinding, RpcExtensionHost, RpcExtensionUi, RpcOutput, RpcShutdownHandle, builtin_commands,
    resource_commands, session_state,
};
use crate::session::{NewSessionOptions, SessionManager, session_dir};

/// A message update with the delta and, for tool call events, the call itself.
fn message_update_json(event: &AssistantMessageEvent) -> Value {
    let tool_call = |kind: &str, index: usize, partial: &pi_ai::AssistantMessage| {
        let call = partial.content.get(index).map(|block| serde_json::to_value(block).unwrap_or(Value::Null));
        json!({"type": kind, "contentIndex": index, "toolCall": call})
    };
    let inner = match event {
        AssistantMessageEvent::TextDelta { content_index, delta, .. } => {
            json!({"type": "text_delta", "contentIndex": content_index, "delta": delta})
        }
        AssistantMessageEvent::ThinkingDelta { content_index, delta, .. } => {
            json!({"type": "thinking_delta", "contentIndex": content_index, "delta": delta})
        }
        AssistantMessageEvent::ToolcallStart { content_index, partial } => {
            tool_call("toolcall_start", *content_index, partial)
        }
        AssistantMessageEvent::ToolcallDelta { content_index, partial, .. } => {
            tool_call("toolcall_delta", *content_index, partial)
        }
        AssistantMessageEvent::ToolcallEnd { content_index, partial, .. } => {
            tool_call("toolcall_end", *content_index, partial)
        }
        _ => return Value::Null,
    };
    json!({"type": "message_update", "assistantMessageEvent": inner})
}

/// Session events as RPC records. Edits and writes also carry the file before the tool runs
/// (`fileSnapshot`) and after it ends (`fileAfter`), read while the agent waits for the event.
pub struct EventEncoder {
    cwd: PathBuf,
    mutations: HashMap<String, PathBuf>,
}

impl EventEncoder {
    pub fn new(cwd: &Path) -> Self {
        Self { cwd: cwd.to_path_buf(), mutations: HashMap::new() }
    }

    /// The record for `event`; `None` for events ACP mode ignores.
    pub fn encode(&mut self, event: &AgentSessionEvent<'_>) -> Option<Value> {
        if !event.is_upstream_event() {
            return None;
        }
        let AgentSessionEvent::Agent(agent_event) = event else { return Some(event.to_json()) };
        match agent_event {
            AgentEvent::MessageUpdate { assistant_message_event, .. } => {
                Some(message_update_json(assistant_message_event)).filter(|value| !value.is_null())
            }
            AgentEvent::ToolExecutionStart { tool_call_id, tool_name, args, .. }
                if tool_name == "edit" || tool_name == "write" =>
            {
                let mut value = event.to_json();
                let path = args.get("path").or_else(|| args.get("file_path")).and_then(Value::as_str);
                if let Some(path) = path.map(|path| self.cwd.join(path)) {
                    value["fileSnapshot"] = json!({"oldText": std::fs::read_to_string(&path).ok()});
                    self.mutations.insert(tool_call_id.clone(), path);
                }
                Some(value)
            }
            AgentEvent::ToolExecutionEnd { tool_call_id, .. } => {
                let mut value = event.to_json();
                if let Some(path) = self.mutations.remove(tool_call_id) {
                    value["fileAfter"] = json!({"newText": std::fs::read_to_string(path).ok()});
                }
                Some(value)
            }
            _ => Some(event.to_json()),
        }
    }
}

/// Lines written by the extension UI, parsed and forwarded as events.
struct EventWriter {
    sender: UnboundedSender<Value>,
    buffer: Vec<u8>,
}

impl Write for EventWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.buffer.extend_from_slice(bytes);
        while let Some(end) = self.buffer.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = self.buffer.drain(..=end).collect();
            if let Ok(value) = serde_json::from_slice::<Value>(&line) {
                let _ = self.sender.send(value);
            }
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub struct RuntimeBackend {
    runtime: Arc<AgentSessionRuntime>,
    session: Arc<AgentSession>,
    ui: Arc<RpcExtensionUi>,
    host: Option<Arc<dyn RpcExtensionHost>>,
    subscription: u64,
    permissions: Arc<ToolPermissions>,
    events: Mutex<Option<UnboundedReceiver<Value>>>,
}

impl RuntimeBackend {
    /// Stream the runtime's session and route its extension dialogs through events. The
    /// extension host binds to the session as in RPC mode, with dialogs mapped to ACP.
    pub fn start(runtime: Arc<AgentSessionRuntime>, host: Option<Arc<dyn RpcExtensionHost>>) -> Arc<Self> {
        let session = runtime.session();
        let permissions = Arc::new(ToolPermissions::default());
        let policy = permissions.clone();
        let weak = Arc::downgrade(&session);
        *lock(&session.tool_authorizer) = Some(Arc::new(move |call, cancel| {
            let policy = policy.clone();
            let read_only = weak
                .upgrade()
                .and_then(|session| session.tool(&call.tool_call.name))
                .and_then(|tool| tool.annotations().cloned())
                .is_some_and(|annotations| annotations.read_only_hint == Some(true));
            let tool_call = call.tool_call.clone();
            let args = call.args.clone();
            async move {
                (!policy.authorize(tool_call, args, read_only, cancel).await).then(|| pi_agent::BeforeToolCallResult {
                    block: true,
                    reason: Some("Tool execution denied by ACP permission policy".into()),
                    ..Default::default()
                })
            }
            .boxed()
        }));
        let (sender, receiver) = unbounded_channel();
        let events = sender.clone();
        let encoder = Mutex::new(EventEncoder::new(session.cwd()));
        let subscription = session.subscribe(move |event| {
            if let Some(value) = lock(&encoder).encode(event) {
                let _ = events.send(value);
            }
        });
        let output = RpcOutput::spawn(Box::new(EventWriter { sender, buffer: Vec::new() }));
        let ui = RpcExtensionUi::new(output.clone());
        crate::rpc::bind_builtins(&session, &ui);
        if let Some(host) = &host {
            let shutdown = RpcShutdownHandle::default();
            host.bind(RpcExtensionBinding::new(runtime.clone(), session.clone(), ui.clone(), shutdown, output));
        }
        session.model_runtime().refresh_in_background();
        Arc::new(Self { runtime, session, ui, host, subscription, permissions, events: Mutex::new(Some(receiver)) })
    }

    pub fn session(&self) -> &Arc<AgentSession> {
        &self.session
    }
}

fn parse_enum<T: serde::de::DeserializeOwned>(text: &str, what: &str) -> anyhow::Result<T> {
    serde_json::from_value(json!(text)).map_err(|_| anyhow::anyhow!("Invalid {what}: {text}"))
}

#[async_trait]
impl PiBackend for RuntimeBackend {
    fn bind_client(&self, session_id: &str, client: Arc<dyn AcpClient>) {
        self.permissions.bind(session_id, client);
    }

    fn permission_mode(&self) -> PermissionMode {
        self.permissions.mode()
    }

    fn set_permission_mode(&self, mode: PermissionMode) {
        self.permissions.set_mode(mode);
    }

    fn take_events(&self) -> Option<UnboundedReceiver<Value>> {
        lock(&self.events).take()
    }

    async fn prompt(&self, message: String, images: Vec<ContentBlock>) -> anyhow::Result<()> {
        let (accepted_sender, accepted) = tokio::sync::oneshot::channel();
        let accepted_sender = Arc::new(Mutex::new(Some(accepted_sender)));
        let flag = accepted_sender.clone();
        let options = PromptOptions {
            images,
            preflight_result: Some(Arc::new(move |_| {
                if let Some(sender) = lock(&flag).take() {
                    let _ = sender.send(Ok(()));
                }
            })),
            ..PromptOptions::default()
        };
        let session = self.session.clone();
        tokio::spawn(async move {
            let result = session.prompt_with(&message, options).await;
            if let Some(sender) = lock(&accepted_sender).take() {
                let _ = sender.send(result.map(|_| ()));
            }
        });
        accepted.await.unwrap_or_else(|_| Err(anyhow::anyhow!("pi stopped before accepting the prompt")))
    }

    async fn abort(&self) -> anyhow::Result<()> {
        self.session.abort().await;
        Ok(())
    }

    async fn get_state(&self) -> anyhow::Result<Value> {
        Ok(serde_json::to_value(session_state(&self.session))?)
    }

    async fn get_available_models(&self) -> anyhow::Result<Value> {
        let runtime = self.session.model_runtime();
        let models: Vec<Value> = runtime
            .available_models()
            .iter()
            .map(|model| {
                let mut value = serde_json::to_value(model).expect("model is serializable");
                if let Some(name) = runtime.registry().provider_name(&model.provider) {
                    value["providerName"] = json!(name);
                }
                value
            })
            .collect();
        Ok(json!({"models": models}))
    }

    async fn set_model(&self, provider: &str, model_id: &str) -> anyhow::Result<()> {
        let available = self.session.model_runtime().available_models();
        let Some(model) = available.into_iter().find(|model| model.provider == provider && model.id == model_id) else {
            anyhow::bail!("Model not found: {provider}/{model_id}");
        };
        self.session.set_model(model, false).await?;
        Ok(())
    }

    async fn get_available_thinking_levels(&self) -> anyhow::Result<Vec<String>> {
        Ok(self.session.available_thinking_levels().into_iter().map(|level| level.as_str().to_string()).collect())
    }

    async fn set_thinking_level(&self, level: &str) -> anyhow::Result<()> {
        self.session.set_thinking_level(parse_enum::<ThinkingLevel>(level, "thinking level")?, false);
        Ok(())
    }

    async fn set_fast_mode(&self, enabled: bool) -> anyhow::Result<()> {
        self.session.set_fast_mode(enabled, true);
        Ok(())
    }

    async fn set_steering_mode(&self, mode: &str) -> anyhow::Result<()> {
        self.session.set_steering_mode(parse_enum::<QueueMode>(mode, "steering mode")?);
        Ok(())
    }

    async fn set_follow_up_mode(&self, mode: &str) -> anyhow::Result<()> {
        self.session.set_follow_up_mode(parse_enum::<QueueMode>(mode, "follow-up mode")?);
        Ok(())
    }

    async fn compact(&self, custom_instructions: Option<String>) -> anyhow::Result<Value> {
        let result = self.session.compact(custom_instructions.as_deref()).await?;
        Ok(compaction_result_json(&result))
    }

    async fn set_auto_compaction(&self, enabled: bool) -> anyhow::Result<()> {
        self.session.set_auto_compaction_enabled(enabled);
        Ok(())
    }

    async fn get_session_stats(&self) -> anyhow::Result<Value> {
        Ok(self.session.session_stats().to_json())
    }

    async fn set_session_name(&self, name: &str) -> anyhow::Result<()> {
        let name = name.trim();
        if name.is_empty() {
            anyhow::bail!("Session name cannot be empty");
        }
        self.session.set_session_name(name)
    }

    async fn export_html(&self, output_path: &Path) -> anyhow::Result<PathBuf> {
        self.session.export_to_html(Some(&output_path.display().to_string()))
    }

    async fn get_messages(&self) -> anyhow::Result<Value> {
        Ok(json!({"messages": self.session.messages()}))
    }

    async fn get_commands(&self) -> anyhow::Result<Value> {
        let mut commands = builtin_commands(&self.session);
        if let Some(host) = &self.host {
            commands.extend(host.commands(&self.session));
        }
        commands.extend(resource_commands(&self.session));
        Ok(json!({"commands": commands}))
    }

    async fn send_extension_ui_response(&self, response: Value) -> anyhow::Result<()> {
        let Value::Object(response) = response else { anyhow::bail!("extension UI responses are objects") };
        let id = response.get("id").and_then(Value::as_str).unwrap_or_default().to_string();
        self.ui.resolve(&id, response);
        Ok(())
    }

    async fn dispose(&self) {
        self.session.unsubscribe(self.subscription);
        crate::rpc::unbind_builtins(&self.session);
        self.session.abort().await;
        self.runtime.dispose().await;
    }
}

/// Opens an [`AgentSessionRuntime`] per ACP session with the command-line options.
pub struct RuntimeBackendFactory {
    pub app: AppOptions,
    /// `--session-dir`, `PI_CODING_AGENT_SESSION_DIR`, or the `sessionDir` setting.
    pub session_dir: Option<PathBuf>,
    /// Extensions, bound to every session as in RPC mode.
    pub extension_host: Option<Arc<dyn RpcExtensionHost>>,
}

impl RuntimeBackendFactory {
    async fn open(&self, cwd: &Path, session_file: Option<&Path>) -> anyhow::Result<AgentSessionRuntime> {
        let override_dir = self.session_dir.as_deref();
        let manager = match session_file {
            Some(file) => SessionManager::open_with(file, override_dir, None)?,
            None => {
                let dir = override_dir.map(Path::to_path_buf).unwrap_or_else(|| session_dir(&self.app.agent_dir, cwd));
                SessionManager::create_with(cwd, Some(&dir), NewSessionOptions::default())?
            }
        };
        create_app_runtime(self.app.clone(), manager).await
    }
}

#[async_trait]
impl BackendFactory for RuntimeBackendFactory {
    async fn create(&self, cwd: &Path, session_file: Option<&Path>) -> Result<Arc<dyn PiBackend>, Error> {
        let runtime =
            self.open(cwd, session_file).await.map_err(|error| internal_error(json!({}), &format!("{error:#}")))?;
        let mut errors = Vec::new();
        for diagnostic in runtime.diagnostics() {
            match diagnostic.level {
                DiagnosticLevel::Error => errors.push(diagnostic.message.clone()),
                DiagnosticLevel::Warning => eprintln!("Warning: {}", diagnostic.message),
                DiagnosticLevel::Info => eprintln!("{}", diagnostic.message),
            }
        }
        if !errors.is_empty() {
            runtime.dispose().await;
            return Err(internal_error(json!({}), &errors.join("\n")));
        }
        Ok(RuntimeBackend::start(Arc::new(runtime), self.extension_host.clone()))
    }
}
