//! What the TUI asks of extensions (upstream `ExtensionRunner` getters the interactive mode uses):
//! shortcuts, raw terminal input, command argument completions, message and tool renderers, and
//! Markdown transformers. Render calls block on the JavaScript thread; call them off it.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use pi_js::HostValue;
use serde_json::{Value, json};

use super::component::JsComponent;
use super::registry::ShortcutInfo;
use super::runner::ExtensionRunner;
use crate::markdown_transform::{MarkdownMessageType, MarkdownTransformContext, MarkdownTransformer};

/// How long input and Markdown hooks may take before the TUI goes on without them.
const HOOK_LIMIT: Duration = Duration::from_secs(2);

/// Keybindings extensions cannot take over (upstream `RESERVED_KEYBINDINGS_FOR_EXTENSION_CONFLICTS`).
const RESERVED_KEYBINDINGS: &[&str] = &[
    "app.interrupt",
    "app.clear",
    "app.exit",
    "app.suspend",
    "app.thinking.cycle",
    "app.model.cycleForward",
    "app.model.cycleBackward",
    "app.model.select",
    "app.tools.expand",
    "app.thinking.toggle",
    "app.editor.external",
    "app.message.copy",
    "app.message.followUp",
    "tui.input.submit",
    "tui.select.confirm",
    "tui.select.cancel",
    "tui.input.copy",
    "tui.editor.deleteToLineEnd",
];

/// What `onTerminalInput` handlers decided about a chunk of input.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TerminalInputVerdict {
    Pass,
    Consume,
    Replace(String),
}

/// Which renderers a tool has (upstream `renderCall`, `renderResult`, `renderShell`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ToolRendererInfo {
    pub call: bool,
    pub result: bool,
    /// `renderShell: "self"`: the renderer draws its own frame.
    pub self_shell: bool,
}

/// The upstream `ToolRenderContext` fields a renderer receives, without its functions.
#[derive(Clone, Debug, Default)]
pub struct ToolRenderRequest {
    pub tool_call_id: String,
    pub args: Value,
    pub cwd: String,
    pub execution_started: bool,
    pub args_complete: bool,
    pub is_partial: bool,
    pub expanded: bool,
    pub show_images: bool,
    pub is_error: bool,
    pub duration_ms: Option<u64>,
    pub output_pad: usize,
}

impl ToolRenderRequest {
    fn context(&self) -> Value {
        json!({
            "args": self.args,
            "toolCallId": self.tool_call_id,
            "cwd": self.cwd,
            "executionStarted": self.execution_started,
            "argsComplete": self.args_complete,
            "isPartial": self.is_partial,
            "expanded": self.expanded,
            "showImages": self.show_images,
            "isError": self.is_error,
            "durationMs": self.duration_ms,
            "outputPad": self.output_pad,
        })
    }
}

fn values(args: Vec<Value>) -> Vec<HostValue> {
    args.into_iter().map(HostValue::from).collect()
}

impl ExtensionRunner {
    fn blocking_json(&self, method: &str, args: Vec<Value>) -> Option<Value> {
        let result = self.host().blocking_call_method_within(method, values(args), HOOK_LIMIT);
        result.ok().map(HostValue::into_json).filter(|value| !value.is_null())
    }

    fn blocking_component(&self, method: &str, args: Vec<Value>) -> Option<JsComponent> {
        let component = self.host().blocking_call_method_opt_ref(method, values(args));
        component.ok().flatten().map(JsComponent::new)
    }

    /// Registered shortcuts the keybindings allow, last registration first, with a warning for
    /// each conflict (upstream `getShortcuts`).
    pub fn shortcuts(&self) -> (Vec<ShortcutInfo>, Vec<String>) {
        let bindings = pi_tui::get_keybindings().get_resolved_bindings();
        let mut builtin: Vec<(String, String, bool)> = Vec::new();
        for (id, value) in bindings.iter() {
            let reserved = RESERVED_KEYBINDINGS.contains(&id);
            for key in value.keys() {
                let key = key.to_lowercase();
                match builtin.iter_mut().find(|(existing, _, _)| *existing == key) {
                    Some(entry) if entry.2 && !reserved => {}
                    Some(entry) => *entry = (key, id.to_string(), reserved),
                    None => builtin.push((key, id.to_string(), reserved)),
                }
            }
        }
        let mut shortcuts: Vec<ShortcutInfo> = Vec::new();
        let mut warnings = Vec::new();
        for shortcut in self.registry().shortcuts.clone() {
            let key = shortcut.shortcut.to_lowercase();
            let path = &shortcut.extension_path;
            match builtin.iter().find(|(existing, _, _)| *existing == key) {
                Some((_, _, true)) => {
                    let message = format!(
                        "Extension shortcut '{}' from {path} conflicts with built-in shortcut. Skipping.",
                        shortcut.shortcut
                    );
                    warnings.push(message);
                    continue;
                }
                Some((_, action, false)) => warnings.push(format!(
                    "Extension shortcut conflict: '{}' is built-in shortcut for {action} and {path}. Using {path}.",
                    shortcut.shortcut
                )),
                None => {}
            }
            if let Some(index) = shortcuts.iter().position(|existing| existing.shortcut.to_lowercase() == key) {
                let existing = shortcuts.remove(index);
                warnings.push(format!(
                    "Extension shortcut conflict: '{}' registered by both {} and {path}. Using {path}.",
                    shortcut.shortcut, existing.extension_path
                ));
            }
            shortcuts.push(ShortcutInfo { shortcut: key, ..shortcut });
        }
        (shortcuts, warnings)
    }

    /// Run the shortcut `data` matches, if any, without waiting for its handler.
    pub fn dispatch_shortcut(&self, data: &str) -> bool {
        let (shortcuts, _) = self.shortcuts();
        let Some(shortcut) = shortcuts.into_iter().find(|shortcut| pi_tui::matches_key(data, &shortcut.shortcut))
        else {
            return false;
        };
        let runner = self.this();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move { runner.run_shortcut(&shortcut.extension_path, &shortcut.shortcut).await });
        }
        true
    }

    /// Run a shortcut's handler; false when it is not registered.
    pub async fn run_shortcut(&self, extension_path: &str, shortcut: &str) -> bool {
        let result = self.call_json("runShortcut", vec![json!(extension_path), json!(shortcut)]).await;
        result.ok().and_then(|value| value.as_bool()).unwrap_or(false)
    }

    /// Whether an `onTerminalInput` handler is registered.
    pub fn listens_to_terminal_input(&self) -> bool {
        self.terminal_input.load(Ordering::SeqCst)
    }

    /// Pass one chunk of terminal input through the `onTerminalInput` handlers.
    pub fn terminal_input(&self, data: &str) -> TerminalInputVerdict {
        let Some(result) = self.blocking_json("terminalInput", vec![json!(data)]) else {
            return TerminalInputVerdict::Pass;
        };
        if result["consume"].as_bool() == Some(true) {
            return TerminalInputVerdict::Consume;
        }
        match result["data"].as_str() {
            Some(replaced) if replaced != data => TerminalInputVerdict::Replace(replaced.to_string()),
            _ => TerminalInputVerdict::Pass,
        }
    }

    /// Argument completions of command `name` (`getArgumentCompletions`), as `AutocompleteItem`s.
    pub async fn command_completions(&self, name: &str, prefix: &str) -> Option<Vec<Value>> {
        let result = self.call_json("commandCompletions", vec![json!(name), json!(prefix)]).await.ok()?;
        result.as_array().cloned()
    }

    pub fn has_message_renderer(&self, custom_type: &str) -> bool {
        self.registry().message_renderers.iter().any(|name| name == custom_type)
    }

    /// The component of a custom message renderer (`registerMessageRenderer`), if one draws it.
    pub fn render_message(
        &self,
        custom_type: &str,
        message: &Value,
        expanded: bool,
        output_pad: usize,
    ) -> Option<JsComponent> {
        if !self.has_message_renderer(custom_type) {
            return None;
        }
        let options = json!({"expanded": expanded, "outputPad": output_pad});
        self.blocking_component("renderMessage", vec![json!(custom_type), message.clone(), options])
    }

    /// Whether an extension draws `custom` entries of `custom_type` (`registerEntryRenderer`).
    pub fn has_entry_renderer(&self, custom_type: &str) -> bool {
        self.registry().entry_renderers.iter().any(|name| name == custom_type)
    }

    /// The component of an entry renderer; `Ok(None)` when it draws nothing, `Err` when it throws.
    pub fn render_entry(&self, entry: &Value, expanded: bool) -> Result<Option<JsComponent>, String> {
        let custom_type = entry["customType"].as_str().unwrap_or_default();
        if !self.has_entry_renderer(custom_type) {
            return Ok(None);
        }
        let args = values(vec![json!(custom_type), entry.clone(), json!({"expanded": expanded})]);
        let component = self.host().blocking_call_method_opt_ref("renderEntry", args);
        component.map(|component| component.map(JsComponent::new)).map_err(|error| error.message)
    }

    /// The renderers of `tool_name`: its own or those of `registerToolRenderer` resolvers.
    pub fn tool_renderer_info(&self, tool_name: &str) -> Option<ToolRendererInfo> {
        let registry = self.registry();
        let own = registry
            .tools
            .iter()
            .any(|tool| tool.name == tool_name && (tool.has_render_call || tool.has_render_result));
        if !own && registry.tool_renderers == 0 {
            return None;
        }
        drop(registry);
        let info = self.blocking_json("toolRendererInfo", vec![json!(tool_name)])?;
        Some(ToolRendererInfo {
            call: info["call"].as_bool() == Some(true),
            result: info["result"].as_bool() == Some(true),
            self_shell: info["shell"] == "self",
        })
    }

    pub fn render_tool_call(&self, tool_name: &str, request: &ToolRenderRequest) -> Option<JsComponent> {
        let args = vec![json!(tool_name), request.args.clone(), request.context()];
        self.blocking_component("renderToolCall", args)
    }

    /// `result` is the upstream `AgentToolResult` (`content`, `details`).
    pub fn render_tool_result(
        &self,
        tool_name: &str,
        result: &Value,
        request: &ToolRenderRequest,
    ) -> Option<JsComponent> {
        let options = json!({"expanded": request.expanded, "isPartial": request.is_partial});
        self.blocking_component("renderToolResult", vec![json!(tool_name), result.clone(), options, request.context()])
    }

    /// Forget the renderer state of a tool call whose row is gone.
    pub fn release_tool_render(&self, tool_call_id: &str) {
        let host = self.host().clone();
        let id = json!(tool_call_id);
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move { host.call_method("releaseToolRender", vec![id.into()]).await });
        }
    }

    /// The extensions' Markdown transformers as one, or `None` when none is registered.
    pub fn markdown_transformer(self: &Arc<Self>) -> Option<MarkdownTransformer> {
        if self.registry().markdown_transformers == 0 {
            return None;
        }
        let runner = Arc::downgrade(self);
        Some(Arc::new(move |markdown: &str, context: &MarkdownTransformContext| {
            let runner = runner.upgrade()?;
            let kind = match context.message_type {
                MarkdownMessageType::User => "user",
                MarkdownMessageType::Assistant => "assistant",
                MarkdownMessageType::AssistantThinking => "assistant-thinking",
            };
            let mut context = json!({
                "messageType": kind,
                "isStreaming": context.is_streaming,
                "availableWidth": context.available_width,
                "messageId": context.message_id,
                "timestamp": context.timestamp,
            });
            // Upstream leaves both undefined for live messages.
            context.as_object_mut()?.retain(|_, value| !value.is_null());
            let result = runner.blocking_json("transformMarkdown", vec![json!(markdown), context])?;
            result.as_str().map(str::to_string)
        }))
    }
}
