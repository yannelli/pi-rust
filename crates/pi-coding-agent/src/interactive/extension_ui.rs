//! `ctx.ui` in the interactive mode (upstream `createExtensionUIContext`). Extensions call
//! [`TuiExtensionUi`] from any thread: each call runs on the UI thread through the event bridge,
//! and reads come from a mirror the UI loop refreshes.

use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError, Weak};

use async_trait::async_trait;
use pi_tui::program_status::{BlockedKind, ProgramState};
use serde_json::{Value, json};
use tokio::sync::oneshot;

use super::bridge::EventBridge;
use super::extension_view::Widget;
use super::mode::InteractiveMode;
use crate::extension_ui::{
    DialogOptions, ExtensionUi, NotifyType, ThemeInfo, WidgetPlacement, WorkingIndicatorOptions,
};
use crate::extensions::{ComponentFactory, CustomRequest};

/// UI state extensions read without waiting for the UI thread.
#[derive(Default)]
pub(super) struct UiMirror {
    pub editor_text: Mutex<String>,
    pub tools_expanded: AtomicBool,
    pub size: Mutex<(u16, u16)>,
    pub git_branch: Mutex<Option<String>>,
    pub statuses: Mutex<BTreeMap<String, String>>,
    pub provider_count: AtomicUsize,
    pub session_stats: Mutex<Value>,
    pub run_state: Mutex<&'static str>,
    pub editor_actions: Mutex<Vec<String>>,
    pub overlays: Mutex<std::collections::HashMap<u64, super::extension_overlay::OverlayMirror>>,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl UiMirror {
    pub fn set_overlay(&self, id: u64, state: Option<super::extension_overlay::OverlayMirror>) {
        match state {
            Some(state) => lock(&self.overlays).insert(id, state),
            None => lock(&self.overlays).remove(&id),
        };
    }

    fn overlay(&self, id: u64) -> Option<super::extension_overlay::OverlayMirror> {
        lock(&self.overlays).get(&id).cloned()
    }

    pub fn set_editor_text(&self, text: &str) {
        *lock(&self.editor_text) = text.to_string();
    }

    pub fn set_size(&self, columns: usize, rows: usize) {
        *lock(&self.size) = (columns.min(u16::MAX as usize) as u16, rows.min(u16::MAX as usize) as u16);
    }

    pub fn set_footer(&self, branch: Option<String>, statuses: &BTreeMap<String, String>, providers: usize) {
        *lock(&self.git_branch) = branch;
        *lock(&self.statuses) = statuses.clone();
        self.provider_count.store(providers, Ordering::SeqCst);
    }

    pub fn set_editor_actions(&self, actions: Vec<String>) {
        *lock(&self.editor_actions) = actions;
    }

    pub fn set_run_footer(&self, session_stats: Value, run_state: &'static str) {
        *lock(&self.session_stats) = session_stats;
        *lock(&self.run_state) = run_state;
    }
}

/// `data` as written for `ctx.ui.writeTerminal`: wrapped for tmux when asked and inside tmux.
pub(super) fn terminal_bytes(data: &str, tmux_passthrough: bool, in_tmux: bool) -> String {
    super::tmux::passthrough(data, tmux_passthrough && in_tmux)
}

/// The `getRunState()` name of a program state.
fn run_state_name(state: ProgramState) -> &'static str {
    match state {
        ProgramState::Idle | ProgramState::Clear => "idle",
        ProgramState::Working => "working",
        ProgramState::Blocked => "blocked",
        ProgramState::Done => "done",
        ProgramState::Error => "error",
    }
}

impl InteractiveMode {
    /// Copy the footer's token, cost, and context numbers and the program state into `mirror`.
    pub(super) fn sync_run_footer(&self, mirror: &UiMirror) {
        let Ok(mut footer) = self.footer.try_borrow_mut() else { return };
        let snapshot = footer.snapshot();
        drop(footer);
        let totals = snapshot.totals;
        let stats = json!({
            "input": totals.input,
            "output": totals.output,
            "cacheRead": totals.cache_read,
            "cacheWrite": totals.cache_write,
            "cost": totals.cost,
            "contextTokens": snapshot.context.as_ref().and_then(|usage| usage.tokens),
            "contextWindow": snapshot.context_window,
            "contextPercent": snapshot.context_percent(),
        });
        let state = self.program_status.borrow().status(None).state;
        mirror.set_run_footer(stats, run_state_name(state));
    }

    /// Call the extension `onBranchChange` listeners after the mirror has the new branch.
    pub(super) fn notify_branch_change(&self) {
        self.sync_extension_mirror();
        let listeners = self.footer_data.borrow().branch_listeners();
        for (key, factory) in listeners {
            tokio::task::spawn_local(async move {
                let _ = factory.invoke_callback(key, Value::Null).await;
            });
        }
    }
}

/// The interactive mode's [`ExtensionUi`].
pub struct TuiExtensionUi {
    bridge: Weak<EventBridge>,
    mirror: Arc<UiMirror>,
}

impl TuiExtensionUi {
    pub(super) fn new(bridge: &Arc<EventBridge>, mirror: Arc<UiMirror>) -> Self {
        Self { bridge: Arc::downgrade(bridge), mirror }
    }

    /// Run `f` on the UI thread; nothing happens once the mode is gone.
    fn run(&self, f: impl FnOnce(&Rc<InteractiveMode>) + Send + 'static) {
        if let Some(bridge) = self.bridge.upgrade() {
            bridge.call(f);
        }
    }

    /// Open a dialog on the UI thread and wait for its answer; `default` when the UI is gone.
    async fn ask<T: Send + 'static>(
        &self,
        default: T,
        cancelled: bool,
        open: impl FnOnce(&Rc<InteractiveMode>) -> oneshot::Receiver<T> + Send + 'static,
    ) -> T {
        if cancelled {
            return default;
        }
        let (sender, receiver) = oneshot::channel();
        self.run(move |mode| {
            let answer = open(mode);
            tokio::task::spawn_local(async move {
                if let Ok(value) = answer.await {
                    let _ = sender.send(value);
                }
            });
        });
        receiver.await.unwrap_or(default)
    }
}

fn is_cancelled(opts: &DialogOptions) -> bool {
    opts.cancel.as_ref().is_some_and(|cancel| cancel.is_cancelled())
}

#[async_trait]
impl ExtensionUi for TuiExtensionUi {
    async fn select(&self, title: &str, options: &[String], opts: DialogOptions) -> Option<String> {
        let (title, options) = (title.to_string(), options.to_vec());
        let cancelled = is_cancelled(&opts);
        self.ask(None, cancelled, move |mode| {
            let blocked = (BlockedKind::Question, title.clone());
            mode.ask_choice_within(&title, options, blocked, opts.timeout, opts.cancel)
        })
        .await
    }

    async fn confirm(&self, title: &str, message: &str, opts: DialogOptions) -> bool {
        let (title, message) = (title.to_string(), message.to_string());
        let cancelled = is_cancelled(&opts);
        let choice = self
            .ask(None, cancelled, move |mode| {
                let options = vec!["Yes".to_string(), "No".to_string()];
                let blocked = (BlockedKind::Permission, title.clone());
                mode.ask_choice_within(&format!("{title}\n{message}"), options, blocked, opts.timeout, opts.cancel)
            })
            .await;
        choice.as_deref() == Some("Yes")
    }

    async fn input(&self, title: &str, _placeholder: Option<&str>, opts: DialogOptions) -> Option<String> {
        let title = title.to_string();
        let cancelled = is_cancelled(&opts);
        self.ask(None, cancelled, move |mode| mode.ask_input_within(&title, None, opts.timeout, opts.cancel)).await
    }

    async fn editor(&self, title: &str, prefill: Option<&str>, _opts: DialogOptions) -> Option<String> {
        let (title, prefill) = (title.to_string(), prefill.map(str::to_string));
        self.ask(None, false, move |mode| mode.ask_editor(&title, prefill)).await
    }

    fn notify(&self, message: &str, kind: Option<NotifyType>) {
        let message = message.to_string();
        self.run(move |mode| match kind.unwrap_or_default() {
            NotifyType::Error => mode.show_error(&message),
            NotifyType::Warning => mode.show_warning(&message),
            NotifyType::Info => mode.show_status(&message),
        });
    }

    fn set_status(&self, key: &str, text: Option<&str>) {
        let (key, text) = (key.to_string(), text.map(str::to_string));
        {
            let mut statuses = lock(&self.mirror.statuses);
            match &text {
                Some(text) => statuses.insert(key.clone(), text.clone()),
                None => statuses.remove(&key),
            };
        }
        self.run(move |mode| {
            mode.footer_data.borrow_mut().set_extension_status(&key, text.as_deref());
            mode.request_render();
        });
    }

    fn set_widget(&self, key: &str, lines: Option<&[String]>, placement: Option<WidgetPlacement>) {
        let key = key.to_string();
        let widget = lines.map(|lines| Widget::Lines(lines.to_vec()));
        self.run(move |mode| mode.set_extension_widget(&key, widget, placement.unwrap_or_default()));
    }

    fn set_widget_component(&self, key: &str, factory: Option<ComponentFactory>, placement: Option<WidgetPlacement>) {
        let key = key.to_string();
        self.run(move |mode| {
            mode.set_extension_widget(&key, factory.map(Widget::Factory), placement.unwrap_or_default())
        });
    }

    fn set_title(&self, title: &str) {
        let title = title.to_string();
        self.run(move |mode| mode.terminal.borrow_mut().set_title(&title));
    }

    fn set_editor_text(&self, text: &str) {
        self.mirror.set_editor_text(text);
        let text = text.to_string();
        self.run(move |mode| mode.set_extension_editor_text(&text));
    }

    fn paste_to_editor(&self, text: &str) {
        let text = text.to_string();
        self.run(move |mode| mode.paste_to_extension_editor(&text));
    }

    fn get_editor_text(&self) -> String {
        lock(&self.mirror.editor_text).clone()
    }

    fn set_working_message(&self, message: Option<&str>) {
        let message = message.map(str::to_string);
        self.run(move |mode| mode.set_working_message(message));
    }

    fn set_working_visible(&self, visible: bool) {
        self.run(move |mode| mode.set_working_visible(visible));
    }

    fn set_working_indicator(&self, options: Option<WorkingIndicatorOptions>) {
        self.run(move |mode| mode.set_working_indicator(options));
    }

    fn set_hidden_thinking_label(&self, label: Option<&str>) {
        let label = label.map(str::to_string);
        self.run(move |mode| mode.set_hidden_thinking_label(label));
    }

    fn get_all_themes(&self) -> Vec<ThemeInfo> {
        let themes = crate::theme::get_available_themes_with_paths();
        themes.into_iter().map(|theme| ThemeInfo { name: theme.name, path: theme.path }).collect()
    }

    fn set_theme(&self, name: &str) -> Result<(), String> {
        let known = name == crate::theme::SYSTEM_THEME_NAME || crate::theme::get_theme_by_name(name).is_some();
        if !known {
            return Err(format!("Theme not found: {name}"));
        }
        let name = name.to_string();
        self.run(move |mode| mode.set_extension_theme(&name));
        Ok(())
    }

    fn get_tools_expanded(&self) -> bool {
        self.mirror.tools_expanded.load(Ordering::SeqCst)
    }

    fn set_tools_expanded(&self, expanded: bool) {
        self.mirror.tools_expanded.store(expanded, Ordering::SeqCst);
        self.run(move |mode| mode.set_tools_expanded(expanded));
    }

    async fn custom(&self, request: CustomRequest) -> Result<Option<Value>, String> {
        let (sender, receiver) = oneshot::channel();
        self.run(move |mode| mode.show_extension_custom(request, sender));
        match receiver.await {
            Ok(result) => result.map(Some),
            Err(_) => Ok(None),
        }
    }

    fn overlay(&self, id: u64, method: &str, arg: &Value) -> Value {
        let Some(state) = self.mirror.overlay(id) else { return Value::Null };
        match method {
            "isHidden" => json!(state.hidden),
            "isFocused" => json!(state.focused),
            "getBounds" => state.bounds,
            "hide" | "setHidden" | "focus" | "unfocus" => {
                if method == "setHidden" {
                    let hidden = arg.as_bool().unwrap_or(false);
                    self.mirror.set_overlay(id, Some(super::extension_overlay::OverlayMirror { hidden, ..state }));
                }
                let (method, arg) = (method.to_string(), arg.clone());
                self.run(move |mode| mode.extension_overlay_action(id, &method, &arg));
                Value::Null
            }
            _ => Value::Null,
        }
    }

    async fn autocomplete_base(
        &self,
        request: crate::extension_ui::AutocompleteRequest,
    ) -> Option<pi_tui::autocomplete::AutocompleteSuggestions> {
        let (sender, receiver) = oneshot::channel();
        self.run(move |mode| mode.base_autocomplete_suggestions(request, sender));
        receiver.await.ok().flatten()
    }

    fn fatal_runtime_error(&self, prefix: &str, message: &str, stack: &str) {
        let error = crate::diagnostics::StackError { message: message.to_string(), stack: Some(stack.to_string()) };
        let (prefix, error) = (prefix.to_string(), anyhow::Error::new(error));
        self.run(move |mode| mode.handle_fatal_runtime_error(&prefix, &error));
    }

    fn add_autocomplete_provider(&self, factory: ComponentFactory) {
        self.run(move |mode| mode.add_extension_autocomplete(factory));
    }

    fn set_footer(&self, factory: Option<ComponentFactory>) {
        self.run(move |mode| mode.set_extension_footer(factory));
    }

    fn set_header(&self, factory: Option<ComponentFactory>) {
        self.run(move |mode| mode.set_extension_header(factory));
    }

    fn set_editor_component(&self, factory: Option<ComponentFactory>) {
        self.run(move |mode| mode.set_custom_editor(factory));
    }

    fn set_terminal_input_listening(&self, listening: bool) {
        self.run(move |mode| mode.set_extension_terminal_input(listening));
    }

    fn request_render(&self, force: bool) {
        self.run(move |mode| if force { mode.request_full_render() } else { mode.request_render() });
    }

    fn terminal_size(&self) -> (u16, u16) {
        *lock(&self.mirror.size)
    }

    fn footer_data(&self, kind: &str) -> Value {
        match kind {
            "gitBranch" => json!(*lock(&self.mirror.git_branch)),
            "statuses" => json!(*lock(&self.mirror.statuses)),
            "providerCount" => json!(self.mirror.provider_count.load(Ordering::SeqCst)),
            "sessionStats" => lock(&self.mirror.session_stats).clone(),
            "runState" => json!(match *lock(&self.mirror.run_state) {
                "" => "idle",
                state => state,
            }),
            _ => Value::Null,
        }
    }

    fn on_branch_change(&self, factory: ComponentFactory, key: u64) {
        self.run(move |mode| mode.footer_data.borrow_mut().add_branch_listener(factory, key));
    }

    fn off_branch_change(&self, target: u64, key: u64) {
        self.run(move |mode| mode.footer_data.borrow_mut().remove_branch_listener(target, key));
    }

    fn write_terminal(&self, data: &str, tmux_passthrough: bool) {
        let in_tmux = pi_tui::env::EnvLookup::non_empty(&pi_tui::env::ProcessEnv, "TMUX").is_some();
        let bytes = terminal_bytes(data, tmux_passthrough, in_tmux);
        self.run(move |mode| mode.terminal.borrow_mut().write(&bytes));
    }

    fn editor_event(&self, kind: &str, value: Option<&str>) {
        if kind == "change" {
            self.mirror.set_editor_text(value.unwrap_or_default());
            return;
        }
        let (kind, value) = (kind.to_string(), value.map(str::to_string));
        self.run(move |mode| mode.handle_custom_editor_event(&kind, value));
    }

    fn editor_actions(&self) -> Vec<String> {
        lock(&self.mirror.editor_actions).clone()
    }

    fn invalidate_tool(&self, tool_call_id: &str) {
        let id = tool_call_id.to_string();
        self.run(move |mode| mode.invalidate_extension_tool(&id));
    }
}

#[cfg(test)]
mod tests {
    use super::terminal_bytes;

    #[test]
    fn wraps_terminal_writes_for_tmux_only_when_asked_inside_tmux() {
        assert_eq!(terminal_bytes("\x1b]9;x\x07", false, true), "\x1b]9;x\x07");
        assert_eq!(terminal_bytes("\x1b]9;x\x07", true, false), "\x1b]9;x\x07");
        assert_eq!(terminal_bytes("\x1b]9;x\x07", true, true), "\x1bPtmux;\x1b\x1b]9;x\x07\x1b\\");
    }
}
