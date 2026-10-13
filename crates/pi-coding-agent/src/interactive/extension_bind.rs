//! The session's extensions in the interactive mode (upstream `bindCurrentSessionExtensions`,
//! `setupExtensionShortcuts`, `showExtensionError`, and the extension renderers and commands the
//! chat and autocomplete use).

use std::rc::{Rc, Weak};
use std::sync::Arc;

use pi_tui::autocomplete::CommandEntry;
use pi_tui::{AutocompleteItem, ComponentRef, SlashCommand, component};
use serde_json::{Value, json};

use super::autocomplete::{BUILTIN_SLASH_COMMANDS, prefixed_description};
use super::components::{CustomMessageRenderer, ThemedText};
use super::extension_ui::TuiExtensionUi;
use super::mode::InteractiveMode;
use super::renderers::{
    ToolRenderContext, ToolRenderer, ToolResultView, format_tool_call_with_args, get_text_output, text,
};
use crate::extension_ui::ExtensionMode;
use crate::extensions::{ExtensionBindings, ExtensionError, ExtensionRunner, ToolRenderRequest, ToolRendererInfo};
use crate::markdown_transform::MarkdownTransformer;
use crate::messages::CustomMessage;
use crate::resources::SourceInfo;
use crate::theme::{ThemeColor, theme};

/// An extension tool renderer; parts it lacks come from the built-in renderer or the generic look.
struct JsToolRenderer {
    mode: Weak<InteractiveMode>,
    runner: std::sync::Weak<ExtensionRunner>,
    tool_name: String,
    info: ToolRendererInfo,
    builtin: Option<Rc<dyn ToolRenderer>>,
}

impl JsToolRenderer {
    fn request(&self, context: &ToolRenderContext<'_>) -> ToolRenderRequest {
        if let Some(mode) = self.mode.upgrade()
            && let Ok(mut state) = mode.state.try_borrow_mut()
        {
            let invalidators = &mut state.extensions.tool_invalidators;
            invalidators.insert(context.tool_call_id.to_string(), context.invalidate.clone());
        }
        ToolRenderRequest {
            tool_call_id: context.tool_call_id.to_string(),
            args: context.args.clone(),
            cwd: context.cwd.display().to_string(),
            execution_started: context.execution_started,
            args_complete: context.args_complete,
            is_partial: context.is_partial,
            expanded: context.expanded,
            show_images: context.show_images,
            is_error: context.is_error,
            duration_ms: context.duration_ms,
            output_pad: context.output_pad,
        }
    }
}

impl ToolRenderer for JsToolRenderer {
    fn render_call(&self, context: &mut ToolRenderContext<'_>) -> ComponentRef {
        let runner = self.runner.upgrade().filter(|_| self.info.call);
        let request = self.request(context);
        if let Some(js) = runner.and_then(|runner| runner.render_tool_call(&self.tool_name, &request)) {
            return component(js);
        }
        match &self.builtin {
            Some(builtin) => builtin.render_call(context),
            None => text(&format_tool_call_with_args(&self.tool_name, context.args, context.expanded)),
        }
    }

    fn render_result(&self, result: &ToolResultView, context: &mut ToolRenderContext<'_>) -> Option<ComponentRef> {
        let runner = self.runner.upgrade().filter(|_| self.info.result);
        let request = self.request(context);
        let value = json!({"content": result.content, "details": result.details, "isError": result.is_error});
        if let Some(js) = runner.and_then(|runner| runner.render_tool_result(&self.tool_name, &value, &request)) {
            return Some(component(js));
        }
        if let Some(builtin) = &self.builtin {
            return builtin.render_result(result, context);
        }
        let output = get_text_output(result, context.show_images);
        (!output.is_empty()).then(|| text(&theme().fg(ThemeColor::ToolOutput, &output)))
    }

    fn refresh_call_after_result(&self) -> bool {
        self.info.call || self.builtin.as_ref().is_some_and(|builtin| builtin.refresh_call_after_result())
    }
}

impl InteractiveMode {
    pub(super) fn extension_ui(&self) -> Arc<TuiExtensionUi> {
        Arc::new(TuiExtensionUi::new(&self.bridge, self.state.borrow().extensions.mirror.clone()))
    }

    /// What the extensions of every session of this mode bind to.
    fn extension_bindings(&self) -> ExtensionBindings {
        let bridge = Arc::downgrade(&self.bridge);
        let (shutdown, abort, errors) = (bridge.clone(), bridge.clone(), bridge);
        ExtensionBindings {
            ui: self.extension_ui(),
            mode: ExtensionMode::Tui,
            runtime: Some(Arc::downgrade(&self.runtime)),
            shutdown: Some(Arc::new(move || {
                if let Some(bridge) = shutdown.upgrade() {
                    bridge.call(|mode| mode.request_extension_shutdown());
                }
            })),
            abort: Some(Arc::new(move || {
                if let Some(bridge) = abort.upgrade() {
                    bridge.call(|mode| {
                        mode.restore_queued_messages_to_editor(true, None);
                    });
                }
            })),
            on_error: Some(Arc::new(move |error: &ExtensionError| {
                let error = error.clone();
                if let Some(bridge) = errors.upgrade() {
                    bridge.call(move |mode| mode.show_extension_error(&error));
                }
            })),
        }
    }

    /// Bind the current session's extensions to this UI; prompts wait until `session_start` ran.
    pub(super) fn bind_current_extensions(&self) {
        let session = self.session();
        let mirror = self.state.borrow().extensions.mirror.clone();
        mirror.set_editor_actions(self.editor.action_ids());
        self.sync_extension_mirror();
        session.mark_binding_extensions();
        let bindings = self.extension_bindings();
        let bridge = self.bridge.clone();
        tokio::spawn(async move {
            session.bind_extensions(bindings).await;
            bridge.call(|mode| mode.after_extensions_bound());
        });
    }

    /// Commands and shortcuts of freshly bound extensions.
    pub(super) fn after_extensions_bound(&self) {
        self.setup_autocomplete_provider();
        self.setup_extension_shortcuts();
        self.request_render();
    }

    fn setup_extension_shortcuts(&self) {
        let runner = self.session().extension_runner();
        let shortcuts = runner.map(|runner| runner.shortcuts().0).unwrap_or_default();
        if shortcuts.is_empty() {
            return self.editor.set_on_extension_shortcut(None);
        }
        let weak = self.weak.clone();
        self.editor.set_on_extension_shortcut(Some(Rc::new(move |data: &str| {
            let runner = weak.upgrade().and_then(|mode| mode.session().extension_runner());
            runner.is_some_and(|runner| runner.dispatch_shortcut(data))
        })));
    }

    /// `ctx.shutdown()`: quit now when idle, else once the agent settles.
    fn request_extension_shutdown(&self) {
        if self.session().is_streaming() {
            self.extensions_state().shutdown_requested = true;
        } else {
            self.request_shutdown();
        }
    }

    pub(super) fn check_shutdown_requested(&self) {
        if self.state.borrow().extensions.shutdown_requested {
            self.request_shutdown();
        }
    }

    pub(super) fn show_extension_error(&self, error: &ExtensionError) {
        let message = format!("Extension \"{}\" error: {}", error.extension_path, error.error);
        self.add_to_chat(component(ThemedText::new(move || theme().fg(ThemeColor::Error, &message), 1, 0)));
        let stack: Vec<String> =
            error.stack.iter().flat_map(|stack| stack.lines().skip(1)).map(str::to_string).collect();
        if !stack.is_empty() {
            let render = move || {
                let lines = stack.iter().map(|line| theme().fg(ThemeColor::Dim, &format!("  {}", line.trim())));
                lines.collect::<Vec<_>>().join("\n")
            };
            self.add_to_chat(component(ThemedText::new(render, 1, 0)));
        }
        self.request_render();
    }

    /// Commands extensions registered, except those named like a built-in one.
    pub(super) fn extension_slash_commands(&self) -> Vec<CommandEntry> {
        let Some(runner) = self.session().extension_runner() else { return Vec::new() };
        let commands = runner.registry().commands.clone();
        commands
            .into_iter()
            .filter(|command| !BUILTIN_SLASH_COMMANDS.iter().any(|(name, _, _)| *name == command.name))
            .map(|command| {
                let mut entry = SlashCommand::new(command.invocation_name.clone(), None);
                let description = command.description.clone().unwrap_or_default();
                entry.description = match serde_json::from_value::<SourceInfo>(command.source_info.clone()) {
                    Ok(source) => prefixed_description(&description, &source),
                    Err(_) => command.description.clone(),
                };
                if command.has_completions {
                    let (runner, name) = (Arc::downgrade(&runner), command.invocation_name.clone());
                    entry = entry.with_async_argument_completions(move |prefix| {
                        let (runner, name, prefix) = (runner.upgrade(), name.clone(), prefix.to_string());
                        Box::pin(async move {
                            let items = runner?.command_completions(&name, &prefix).await?;
                            Some(items.iter().filter_map(autocomplete_item).collect())
                        })
                    });
                }
                entry.into()
            })
            .collect()
    }

    /// The renderer of a custom message type an extension registered one for.
    pub(super) fn extension_message_renderer(&self, message: &CustomMessage) -> Option<CustomMessageRenderer> {
        let runner =
            self.session().extension_runner().filter(|runner| runner.has_message_renderer(&message.custom_type))?;
        let mut value = serde_json::to_value(message).ok()?;
        value["role"] = json!("custom");
        let (runner, custom_type) = (Arc::downgrade(&runner), message.custom_type.clone());
        Some(Rc::new(move |expanded: bool, output_pad: usize| {
            let js = runner.upgrade()?.render_message(&custom_type, &value, expanded, output_pad)?;
            Some(component(js) as ComponentRef)
        }))
    }

    /// An extension's renderer for `tool_name`, over the built-in one.
    pub(super) fn extension_tool_renderer(&self, tool_name: &str) -> Option<Rc<dyn ToolRenderer>> {
        let runner = self.session().extension_runner()?;
        let info = runner.tool_renderer_info(tool_name)?;
        Some(Rc::new(JsToolRenderer {
            mode: self.weak.clone(),
            runner: Arc::downgrade(&runner),
            tool_name: tool_name.to_string(),
            info,
            builtin: super::renderers::builtin_renderer(tool_name),
        }))
    }

    pub(super) fn extension_markdown_transformers(&self) -> Vec<MarkdownTransformer> {
        self.session().extension_runner().and_then(|runner| runner.markdown_transformer()).into_iter().collect()
    }
}

/// An `AutocompleteItem` from its JSON form.
fn autocomplete_item(value: &Value) -> Option<AutocompleteItem> {
    let item_value = value["value"].as_str()?;
    let label = value["label"].as_str().unwrap_or(item_value);
    let item = AutocompleteItem::new(item_value, label);
    Some(match value["description"].as_str() {
        Some(description) => item.with_description(description),
        None => item,
    })
}
