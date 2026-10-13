//! `ctx.ui` operations: plain UI calls go straight to the session's `ExtensionUi`; component
//! factories become [`ComponentFactory`] handles; dialogs are async (see `ops_async`).

use std::time::Duration;

use pi_js::{HostValue, JsError};
use serde_json::{Value, json};

use super::component::{ComponentFactory, ComponentKind, CustomRequest};
use super::ops::{Args, error};
use super::runner::ExtensionRunner;
use crate::agent_session::lock;
use crate::extension_ui::{DialogOptions, NotifyType, WidgetPlacement, WorkingIndicatorOptions};

fn placement(value: &Value) -> Option<WidgetPlacement> {
    match value.as_str() {
        Some("belowEditor") => Some(WidgetPlacement::BelowEditor),
        Some("aboveEditor") => Some(WidgetPlacement::AboveEditor),
        _ => None,
    }
}

fn notify_type(value: &Value) -> Option<NotifyType> {
    match value.as_str() {
        Some("warning") => Some(NotifyType::Warning),
        Some("error") => Some(NotifyType::Error),
        Some("info") => Some(NotifyType::Info),
        _ => None,
    }
}

fn factory(runner: &ExtensionRunner, target: u64, id: &Value, kind: ComponentKind) -> Option<ComponentFactory> {
    id.as_u64().map(|id| ComponentFactory::new(&runner.this(), id, target, kind))
}

/// Dialog options from `{ timeout, signal }`.
pub(crate) fn dialog_options(runner: &ExtensionRunner, value: &Value) -> DialogOptions {
    let timeout = value["timeout"].as_f64().filter(|ms| *ms > 0.0).map(|ms| Duration::from_millis(ms as u64));
    let cancel = value["signal"].as_u64().map(|id| runner.token(Some(id)));
    DialogOptions { timeout, cancel }
}

pub(crate) fn sync(runner: &ExtensionRunner, name: &str, args: &Args) -> Result<HostValue, JsError> {
    if name == "keybindings" {
        return Ok(HostValue::Json(keybindings()));
    }
    if name == "customDone" {
        if let Some(sender) = lock(&runner.custom_done).remove(&args.u64(0)) {
            let _ = sender.send(args.json(1));
        }
        return Ok(HostValue::Undefined);
    }
    if name == "invalidateTool" {
        runner.bindings().ui.invalidate_tool(&args.str(0)?);
        return Ok(HostValue::Undefined);
    }
    let target = args.u64(0);
    let ui = runner.target_ui(target);
    let text = |index| args.opt_str(index);
    let value = match name {
        "editorEvent" => {
            ui.editor_event(&args.str(1)?, text(2).as_deref());
            Value::Null
        }
        "editorShortcut" => json!(runner.target_runner(target).dispatch_shortcut(&args.str(1)?)),
        "editorActions" => json!(ui.editor_actions()),
        "requestRender" => {
            ui.request_render(args.bool(1));
            Value::Null
        }
        "terminalSize" => {
            let (columns, rows) = ui.terminal_size();
            json!([columns, rows])
        }
        "footer" => ui.footer_data(&args.str(1)?),
        "notify" => {
            ui.notify(&args.str(1)?, notify_type(&args.json(2)));
            Value::Null
        }
        "terminalInput" => {
            let listening = args.bool(1);
            runner.target_runner(target).terminal_input.store(listening, std::sync::atomic::Ordering::SeqCst);
            ui.set_terminal_input_listening(listening);
            Value::Null
        }
        "setStatus" => {
            ui.set_status(&args.str(1)?, text(2).as_deref());
            Value::Null
        }
        "setWorkingMessage" => {
            ui.set_working_message(text(1).as_deref());
            Value::Null
        }
        "setWorkingVisible" => {
            ui.set_working_visible(args.bool(1));
            Value::Null
        }
        "setWorkingIndicator" => {
            let options = args.json(1);
            let indicator = (!options.is_null()).then(|| WorkingIndicatorOptions {
                frames: options["frames"]
                    .as_array()
                    .map(|frames| frames.iter().filter_map(|frame| frame.as_str().map(str::to_string)).collect()),
                interval_ms: options["intervalMs"].as_u64(),
            });
            ui.set_working_indicator(indicator);
            Value::Null
        }
        "setHiddenThinkingLabel" => {
            ui.set_hidden_thinking_label(text(1).as_deref());
            Value::Null
        }
        "setWidget" => {
            let lines: Option<Vec<String>> = serde_json::from_value(args.json(2)).ok();
            ui.set_widget(&args.str(1)?, lines.as_deref(), placement(&args.json(3)));
            Value::Null
        }
        "setWidgetComponent" => {
            let factory = factory(runner, target, &args.json(2), ComponentKind::Widget);
            ui.set_widget_component(&args.str(1)?, factory, placement(&args.json(3)));
            Value::Null
        }
        "setFooter" => {
            ui.set_footer(factory(runner, target, &args.json(1), ComponentKind::Footer));
            Value::Null
        }
        "setHeader" => {
            ui.set_header(factory(runner, target, &args.json(1), ComponentKind::Header));
            Value::Null
        }
        "setEditorComponent" => {
            ui.set_editor_component(factory(runner, target, &args.json(1), ComponentKind::Editor));
            Value::Null
        }
        "addAutocompleteProvider" => {
            if let Some(factory) = factory(runner, target, &args.json(1), ComponentKind::Widget) {
                ui.add_autocomplete_provider(factory);
            }
            Value::Null
        }
        "setTitle" => {
            ui.set_title(&args.str(1)?);
            Value::Null
        }
        "pasteToEditor" => {
            ui.paste_to_editor(&args.str(1)?);
            Value::Null
        }
        "setEditorText" => {
            ui.set_editor_text(&args.str(1)?);
            Value::Null
        }
        "getEditorText" => json!(ui.get_editor_text()),
        "getAllThemes" => ui
            .get_all_themes()
            .into_iter()
            .map(|theme| json!({"name": theme.name, "path": theme.path.map(|path| path.display().to_string())}))
            .collect(),
        "setTheme" => match ui.set_theme(&args.str(1)?) {
            Ok(()) => Value::Null,
            Err(message) => json!(message),
        },
        "getToolsExpanded" => json!(ui.get_tools_expanded()),
        "setToolsExpanded" => {
            ui.set_tools_expanded(args.bool(1));
            Value::Null
        }
        other => return Err(error(format!("Unknown UI operation: {other}"))),
    };
    Ok(HostValue::Json(value))
}

/// `ctx.ui.custom(factory, options)`: resolves with the value passed to `done`.
pub(crate) async fn custom(runner: &ExtensionRunner, target: u64, id: u64, options: Value) -> Value {
    let (sender, done) = tokio::sync::oneshot::channel();
    lock(&runner.custom_done).insert(id, sender);
    let request = CustomRequest {
        factory: ComponentFactory::new(&runner.this(), id, target, ComponentKind::Custom),
        overlay: options["overlay"].as_bool().unwrap_or(false),
        overlay_options: options.get("overlayOptions").filter(|value| !value.is_null()).cloned(),
        done,
    };
    let result = runner.target_ui(target).custom(request).await;
    lock(&runner.custom_done).remove(&id);
    result.unwrap_or(Value::Null)
}

/// Every keybinding id with its effective keys, for the JavaScript `KeybindingsManager`.
fn keybindings() -> Value {
    let installed = pi_tui::get_keybindings();
    let definitions: serde_json::Map<String, Value> = crate::keybindings::keybindings()
        .iter()
        .map(|(id, definition)| {
            let keys = if installed.get_definition(id).is_some() {
                installed.get_keys(id)
            } else {
                definition.default_keys.clone()
            };
            (id.to_string(), json!({"defaultKeys": keys, "description": definition.description}))
        })
        .collect();
    json!({"definitions": definitions, "user": {}})
}
