//! Dialogs in the editor slot that answer through a channel, so flows can await them (upstream
//! `showExtensionSelector`, `showExtensionConfirm`, `showExtensionInput`, `showExtensionEditor`,
//! and `pushExtensionDialog`).

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use pi_tui::program_status::BlockedKind;
use pi_tui::{ComponentRef, component};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use super::mode::InteractiveMode;
use super::selector::{SelectorDone, SelectorSpec};
use crate::interactive_components::{
    ExtensionEditor, ExtensionEditorOptions, ExtensionInput, ExtensionInputOptions, ExtensionSelector,
    ExtensionSelectorOptions,
};

const DIALOG_SOURCE: &str = "extension-dialog";

/// Where the submit and cancel callbacks send a one-shot answer.
type AnswerSlot<T> = Rc<RefCell<Option<oneshot::Sender<T>>>>;

fn answer<T: 'static>() -> (AnswerSlot<T>, oneshot::Receiver<T>) {
    let (sender, receiver) = oneshot::channel();
    (Rc::new(RefCell::new(Some(sender))), receiver)
}

fn send<T>(slot: &AnswerSlot<T>, value: T) {
    if let Some(sender) = slot.borrow_mut().take() {
        let _ = sender.send(value);
    }
}

/// Close a dialog with `None` once `cancel` fires.
fn watch_cancel(
    cancel: Option<CancellationToken>,
    slot: &AnswerSlot<Option<String>>,
    done: &SelectorDone,
    weak: std::rc::Weak<InteractiveMode>,
) -> Option<tokio::task::AbortHandle> {
    let cancel = cancel?;
    let (slot, done) = (slot.clone(), done.clone());
    let watcher = tokio::task::spawn_local(async move {
        cancel.cancelled().await;
        send(&slot, None);
        done();
        if let Some(mode) = weak.upgrade() {
            mode.request_render();
        }
    });
    Some(watcher.abort_handle())
}

/// An open dialog or inline view: the editor slot shows the last one.
struct DialogEntry {
    id: u64,
    component: ComponentRef,
    focus: ComponentRef,
    dispose: Option<Box<dyn FnOnce()>>,
    blocked: Option<(BlockedKind, String)>,
}

/// Dialogs in the editor slot (upstream `extensionDialogs`). A new dialog hides the visible one,
/// and closing the visible dialog shows the previous one again, so every dialog stays answerable.
#[derive(Default)]
pub(super) struct DialogStack {
    entries: Vec<DialogEntry>,
    next_id: u64,
}

impl InteractiveMode {
    /// Whether an extension dialog or inline `custom()` component is open.
    pub(super) fn dialog_open(&self) -> bool {
        !self.dialogs.borrow().entries.is_empty()
    }

    fn push_dialog(&self, entry: DialogEntry) {
        self.dialogs.borrow_mut().entries.push(entry);
        self.show_top_dialog();
    }

    fn next_dialog_id(&self) -> u64 {
        let mut dialogs = self.dialogs.borrow_mut();
        dialogs.next_id += 1;
        dialogs.next_id
    }

    /// Remove a dialog. If it was visible, show the previous dialog or the editor.
    fn close_dialog(&self, id: u64) {
        let (entry, was_top) = {
            let mut dialogs = self.dialogs.borrow_mut();
            let Some(index) = dialogs.entries.iter().position(|entry| entry.id == id) else { return };
            let entry = dialogs.entries.remove(index);
            (entry, index == dialogs.entries.len())
        };
        if let Some(dispose) = entry.dispose {
            dispose();
        }
        if was_top {
            self.show_top_dialog();
        }
    }

    fn show_top_dialog(&self) {
        let top = self
            .dialogs
            .borrow()
            .entries
            .last()
            .map(|entry| (entry.component.clone(), entry.focus.clone(), entry.blocked.clone()));
        let Some((component, focus, blocked)) = top else {
            self.selector.restore_editor();
            self.set_blocked_status(DIALOG_SOURCE, None);
            return self.request_render();
        };
        self.selector.show(Box::new(move |_done| SelectorSpec::new(component).with_focus(focus)));
        self.set_blocked_status(DIALOG_SOURCE, blocked);
        self.request_render();
    }

    /// Close every dialog without answering it and show the editor (upstream `resetExtensionUI`).
    pub(super) fn reset_dialogs(&self) {
        let entries = std::mem::take(&mut self.dialogs.borrow_mut().entries);
        if entries.is_empty() {
            return;
        }
        for entry in entries {
            if let Some(dispose) = entry.dispose {
                dispose();
            }
        }
        self.show_top_dialog();
    }

    /// Show `create`'s component in the editor slot, reporting `blocked` while it is visible.
    fn show_dialog(
        &self,
        blocked: (BlockedKind, String),
        create: impl FnOnce(SelectorDone) -> (ComponentRef, Option<Box<dyn FnOnce()>>) + 'static,
    ) {
        let id = self.next_dialog_id();
        let weak = self.weak.clone();
        let closing: SelectorDone = Rc::new(move || {
            if let Some(mode) = weak.upgrade() {
                mode.close_dialog(id);
            }
        });
        let (component, dispose) = create(closing);
        let entry = DialogEntry { id, focus: component.clone(), component, dispose, blocked: Some(blocked) };
        self.push_dialog(entry);
    }

    /// Show `view` in the editor slot until the returned function closes it.
    pub(super) fn mount_view(&self, view: ComponentRef) -> Rc<dyn Fn()> {
        self.mount_view_blocked(view, None)
    }

    /// [`Self::mount_view`] that reports pi as blocked on user input while `view` is visible.
    pub(super) fn mount_view_blocked(
        &self,
        view: ComponentRef,
        blocked: Option<(BlockedKind, String)>,
    ) -> Rc<dyn Fn()> {
        let id = self.next_dialog_id();
        self.push_dialog(DialogEntry { id, focus: view.clone(), component: view, dispose: None, blocked });
        let weak = self.weak.clone();
        Rc::new(move || {
            if let Some(mode) = weak.upgrade() {
                mode.close_dialog(id);
                mode.request_render();
            }
        })
    }

    /// A list of options; `None` when dismissed.
    pub(super) fn ask_choice(&self, title: &str, options: Vec<String>) -> oneshot::Receiver<Option<String>> {
        self.ask_choice_with(title, options, (BlockedKind::Question, title.to_string()))
    }

    fn ask_choice_with(
        &self,
        title: &str,
        options: Vec<String>,
        blocked: (BlockedKind, String),
    ) -> oneshot::Receiver<Option<String>> {
        self.ask_choice_within(title, options, blocked, None, None)
    }

    /// [`InteractiveMode::ask_choice`] that closes with `None` after `timeout` or on `cancel`.
    pub(super) fn ask_choice_within(
        &self,
        title: &str,
        options: Vec<String>,
        blocked: (BlockedKind, String),
        timeout: Option<Duration>,
        cancel: Option<CancellationToken>,
    ) -> oneshot::Receiver<Option<String>> {
        let (slot, receiver) = answer();
        let title = title.to_string();
        let tui = self.tui();
        let toggle = self.action(|mode| mode.toggle_tool_output_expansion());
        let weak = self.weak.clone();
        self.show_dialog(blocked, move |done| {
            let watcher = watch_cancel(cancel, &slot, &done, weak);
            let (on_select, on_cancel) = (slot.clone(), slot);
            let (done_select, done_cancel) = (done.clone(), done);
            let selector = ExtensionSelector::new(
                &title,
                options,
                move |option| {
                    send(&on_select, Some(option.to_string()));
                    done_select();
                },
                move || {
                    send(&on_cancel, None);
                    done_cancel();
                },
                ExtensionSelectorOptions {
                    tui: Some(tui),
                    timeout,
                    on_toggle_tools_expanded: Some(Box::new(move || toggle())),
                    description: None,
                },
            );
            let dispose_selector = selector.disposer();
            let dispose = move || {
                dispose_selector();
                if let Some(watcher) = watcher {
                    watcher.abort();
                }
            };
            (component(selector), Some(Box::new(dispose) as Box<dyn FnOnce()>))
        });
        receiver
    }

    /// `Yes`/`No` under `title` and `message`; false when dismissed.
    pub(super) async fn ask_confirm(&self, title: &str, message: &str) -> bool {
        let options = vec!["Yes".to_string(), "No".to_string()];
        let receiver =
            self.ask_choice_with(&format!("{title}\n{message}"), options, (BlockedKind::Permission, title.to_string()));
        receiver.await.ok().flatten().as_deref() == Some("Yes")
    }

    /// Callback form of [`InteractiveMode::ask_confirm`].
    pub(super) fn show_confirm(
        &self,
        title: &str,
        message: &str,
        on_done: impl FnOnce(&Rc<InteractiveMode>, bool) + 'static,
    ) {
        let (title, message) = (title.to_string(), message.to_string());
        self.spawn(move |mode| async move {
            let confirmed = mode.ask_confirm(&title, &message).await;
            on_done(&mode, confirmed);
        });
    }

    /// A single-line input; `None` when dismissed. Firing `cancel` closes it with `None`.
    pub(super) fn ask_input_until(
        &self,
        title: &str,
        initial: Option<String>,
        cancel: Option<CancellationToken>,
    ) -> oneshot::Receiver<Option<String>> {
        self.ask_input_within(title, initial, None, cancel)
    }

    /// [`InteractiveMode::ask_input_until`] that also closes with `None` after `timeout`.
    pub(super) fn ask_input_within(
        &self,
        title: &str,
        initial: Option<String>,
        timeout: Option<Duration>,
        cancel: Option<CancellationToken>,
    ) -> oneshot::Receiver<Option<String>> {
        let (slot, receiver) = answer();
        let title = title.to_string();
        let tui = self.tui();
        let weak = self.weak.clone();
        self.show_dialog((BlockedKind::Question, title.clone()), move |done| {
            let watcher = watch_cancel(cancel, &slot, &done, weak);
            let (on_submit, on_cancel) = (slot.clone(), slot);
            let (done_submit, done_cancel) = (done.clone(), done);
            let input = ExtensionInput::new(
                &title,
                None,
                move |value| {
                    send(&on_submit, Some(value.to_string()));
                    done_submit();
                },
                move || {
                    send(&on_cancel, None);
                    done_cancel();
                },
                ExtensionInputOptions { tui: Some(tui), timeout, initial_value: initial, description: None },
            );
            let dispose_input = input.disposer();
            let dispose = move || {
                dispose_input();
                if let Some(watcher) = watcher {
                    watcher.abort();
                }
            };
            (component(input), Some(Box::new(dispose) as Box<dyn FnOnce()>))
        });
        receiver
    }

    /// A multi-line editor with external editor support; `None` when dismissed.
    pub(super) fn ask_editor(&self, title: &str, prefill: Option<String>) -> oneshot::Receiver<Option<String>> {
        let (slot, receiver) = answer();
        let title = title.to_string();
        let tui = self.tui();
        let command = Some(self.settings().external_editor_command());
        self.show_dialog((BlockedKind::Question, title.clone()), move |done| {
            let (on_submit, on_cancel) = (slot.clone(), slot);
            let (done_submit, done_cancel) = (done.clone(), done);
            let editor = ExtensionEditor::new(
                &tui,
                &title,
                prefill.as_deref(),
                move |value| {
                    send(&on_submit, Some(value.to_string()));
                    done_submit();
                },
                move || {
                    send(&on_cancel, None);
                    done_cancel();
                },
                ExtensionEditorOptions { editor: Default::default(), description: None },
                command,
            );
            (component(editor), None)
        });
        receiver
    }
}
