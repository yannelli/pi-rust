//! Keyboard actions and prompt submission (upstream `setupKeyHandlers`,
//! `setupEditorSubmitHandler`, queue handling, and `handleBashCommand`).

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use pi_tui::{Spacer, TruncatedText, component};

use super::components::BashExecutionComponent;
use super::mode::{CompactionQueued, InteractiveMode};
use super::util::quote_if_needed;
use crate::agent_session::{PromptOptions, StreamingBehavior};
use crate::interactive_components::{ExternalEditorResult, edit_in_external_editor, key_display_text};
use crate::settings::schema::DoubleEscapeAction;
use crate::theme::{ThemeColor, theme};

const DOUBLE_PRESS: Duration = Duration::from_millis(500);

impl InteractiveMode {
    /// Before startup finishes: text is kept, and only clear, exit, and submit feedback work.
    pub(super) fn setup_startup_handlers(&self) {
        self.editor.on_action("app.clear", {
            let action = self.action(|mode| mode.handle_ctrl_c());
            move || action()
        });
        self.editor.set_on_ctrl_d(Some(self.action(|mode| mode.handle_ctrl_d())));
        let weak = self.weak.clone();
        self.editor.on_submit(move |text| {
            if let Some(mode) = weak.upgrade() {
                mode.editor.set_text(text);
                mode.show_status("Startup is still in progress");
            }
        });
    }

    pub(super) fn setup_key_handlers(&self) {
        self.editor.set_on_escape(Some(self.action(|mode| mode.handle_escape())));
        let bind = |id: &str, f: fn(&Rc<InteractiveMode>)| {
            let action = self.action(f);
            self.editor.on_action(id, move || action());
        };
        bind("app.clear", |mode| mode.handle_ctrl_c());
        bind("app.suspend", |mode| mode.handle_ctrl_z());
        bind("app.thinking.cycle", |mode| mode.cycle_thinking_level());
        bind("app.model.cycleForward", |mode| mode.cycle_model(false));
        bind("app.model.cycleBackward", |mode| mode.cycle_model(true));
        bind("app.model.select", |mode| mode.show_model_selector(None));
        bind("app.tools.expand", |mode| mode.toggle_tool_output_expansion());
        bind("app.thinking.toggle", |mode| mode.toggle_thinking_block_visibility());
        bind("app.editor.external", |mode| mode.handle_open_external_editor());
        bind("app.message.copy", |mode| mode.handle_copy_command(true, true));
        bind("app.message.followUp", |mode| mode.handle_follow_up());
        bind("app.message.dequeue", |mode| mode.handle_dequeue());
        bind("app.session.new", |mode| mode.handle_new_session());
        bind("app.session.tree", |mode| mode.show_tree_selector(None));
        bind("app.session.fork", |mode| mode.show_user_message_selector());
        bind("app.session.resume", |mode| mode.show_session_selector());
        self.editor.set_on_ctrl_d(Some(self.action(|mode| mode.handle_ctrl_d())));
        let weak = self.weak.clone();
        self.editor.on_change(move |text| {
            let Some(mode) = weak.upgrade() else { return };
            let bash_mode = text.trim_start().starts_with('!');
            let changed = std::mem::replace(&mut mode.state.borrow_mut().is_bash_mode, bash_mode) != bash_mode;
            if changed {
                mode.update_editor_border_color();
            }
        });
        self.editor.set_on_paste_image(Some(self.action(|mode| mode.handle_clipboard_paste())));
        let weak = self.weak.clone();
        self.editor.on_submit(move |text| {
            if let Some(mode) = weak.upgrade() {
                mode.handle_submit(text);
            }
        });
    }

    fn handle_escape(&self) {
        let session = self.session();
        if session.is_streaming() {
            self.restore_queued_messages_to_editor(true, None);
        } else if session.is_bash_running() {
            session.abort_bash();
        } else if self.state.borrow().is_bash_mode {
            self.editor.set_text("");
            self.state.borrow_mut().is_bash_mode = false;
            self.update_editor_border_color();
        } else if self.editor.get_text().trim().is_empty() {
            let action = self.settings().double_escape_action();
            if action == DoubleEscapeAction::None {
                return;
            }
            let now = Instant::now();
            let double = self.state.borrow().last_escape.is_some_and(|last| now - last < DOUBLE_PRESS);
            if double {
                self.state.borrow_mut().last_escape = None;
                match action {
                    DoubleEscapeAction::Tree => self.show_tree_selector(None),
                    _ => self.show_user_message_selector(),
                }
            } else {
                self.state.borrow_mut().last_escape = Some(now);
            }
        }
    }

    /// First press clears the editor; a second press within 500 ms quits.
    pub(super) fn handle_ctrl_c(&self) {
        let now = Instant::now();
        let double = self.state.borrow().last_sigint.is_some_and(|last| now - last < DOUBLE_PRESS);
        if double {
            self.request_shutdown();
        } else {
            self.editor.set_text("");
            self.state.borrow_mut().last_sigint = Some(now);
            self.request_render();
        }
    }

    /// Ctrl+D on an empty editor.
    pub(super) fn handle_ctrl_d(&self) {
        self.request_shutdown();
    }

    fn cycle_thinking_level(&self) {
        match self.session().cycle_thinking_level(true) {
            None => self.show_status("Current model does not support thinking"),
            Some(level) => {
                self.update_editor_border_color();
                self.show_status(&format!("Thinking level: {}", level.as_str()));
            }
        }
    }

    fn cycle_model(&self, backward: bool) {
        let session = self.session();
        match session.cycle_model(backward, true) {
            None => {
                let scoped = !session.scoped_models().is_empty();
                self.show_status(if scoped { "Only one model in scope" } else { "Only one model available" });
            }
            Some(result) => {
                self.update_editor_border_color();
                let thinking = if result.model.reasoning && result.thinking_level != pi_ai::ThinkingLevel::Off {
                    format!(" (thinking: {})", result.thinking_level.as_str())
                } else {
                    String::new()
                };
                let name = if result.model.name.is_empty() { &result.model.id } else { &result.model.name };
                self.show_status(&format!("Switched to {name}{thinking}"));
            }
        }
    }

    /// Ctrl+G: edit the prompt in `externalEditor`, `$VISUAL`, or `$EDITOR`.
    fn handle_open_external_editor(&self) {
        let command = self.settings().external_editor_command();
        let content = self.editor.get_expanded_text();
        self.spawn(move |mode| async move {
            let tui = mode.tui();
            tui.stop(pi_tui::TuiStopOptions::default());
            let result = edit_in_external_editor(&command, &content).await;
            if let ExternalEditorResult::Complete(text) = result {
                mode.editor.set_text(&text);
            }
            tui.start();
            mode.request_full_render();
        });
    }

    /// Alt+Enter: queue a follow-up while streaming, otherwise submit.
    fn handle_follow_up(&self) {
        let text = self.editor.get_expanded_text().trim().to_string();
        if text.is_empty() {
            return;
        }
        let session = self.session();
        if session.is_compacting() {
            self.queue_compaction_message(text, true);
        } else if session.is_streaming() {
            self.editor.add_to_history(&text);
            self.editor.set_text("");
            self.prompt_while_streaming(text, StreamingBehavior::FollowUp);
        } else {
            self.editor.set_text("");
            self.handle_submit(&text);
        }
    }

    fn handle_dequeue(&self) {
        match self.restore_queued_messages_to_editor(false, None) {
            0 => self.show_status("No queued messages to restore"),
            1 => self.show_status("Restored 1 queued message to editor"),
            count => self.show_status(&format!("Restored {count} queued messages to editor")),
        }
    }

    /// Queued steering and follow-up texts, the session's and those waiting for compaction.
    fn queued_messages(&self) -> (Vec<String>, Vec<String>) {
        let session = self.session();
        let (mut steering, mut follow_up) = (session.steering_messages(), session.follow_up_messages());
        for queued in &self.state.borrow().compaction_queue {
            if queued.follow_up { &mut follow_up } else { &mut steering }.push(queued.text.clone());
        }
        (steering, follow_up)
    }

    fn clear_all_queues(&self) -> Vec<String> {
        let (mut steering, mut follow_up) = self.session().clear_queue();
        for queued in std::mem::take(&mut self.state.borrow_mut().compaction_queue) {
            if queued.follow_up { &mut follow_up } else { &mut steering }.push(queued.text);
        }
        steering.extend(follow_up);
        steering
    }

    /// The queue under the chat with a hint on how to edit it.
    pub(super) fn update_pending_messages_display(&self) {
        let (steering, follow_up) = self.queued_messages();
        let mut pending = self.ui.pending.borrow_mut();
        pending.clear();
        if steering.is_empty() && follow_up.is_empty() {
            return;
        }
        pending.add_child(component(Spacer::new(1)));
        let lines = steering
            .iter()
            .map(|message| format!("Steering: {message}"))
            .chain(follow_up.iter().map(|message| format!("Follow-up: {message}")));
        for line in lines {
            pending.add_child(component(TruncatedText::new(&theme().fg(ThemeColor::Dim, &line), 1, 0)));
        }
        let hint = format!("↳ {} to edit all queued messages", key_display_text("app.message.dequeue"));
        pending.add_child(component(TruncatedText::new(&theme().fg(ThemeColor::Dim, &hint), 1, 0)));
    }

    /// Move every queued message into the editor; `abort` also stops the run (Escape).
    pub(super) fn restore_queued_messages_to_editor(&self, abort: bool, current_text: Option<String>) -> usize {
        let queued = self.clear_all_queues();
        if !queued.is_empty() {
            let current = current_text.unwrap_or_else(|| self.editor.get_text());
            let combined: Vec<String> =
                [queued.join("\n\n"), current].into_iter().filter(|text| !text.trim().is_empty()).collect();
            self.editor.set_text(&combined.join("\n\n"));
        }
        self.update_pending_messages_display();
        if abort {
            let session = self.session();
            tokio::spawn(async move { session.abort().await });
        }
        queued.len()
    }

    fn queue_compaction_message(&self, text: String, follow_up: bool) {
        self.editor.add_to_history(&text);
        self.editor.set_text("");
        self.state.borrow_mut().compaction_queue.push(CompactionQueued { text, follow_up });
        self.update_pending_messages_display();
        self.show_status("Queued message for after compaction");
    }

    /// Send messages typed during compaction: the first as a prompt, the rest queued behind it.
    pub(super) fn flush_compaction_queue(&self, will_retry: bool) {
        let queued = std::mem::take(&mut self.state.borrow_mut().compaction_queue);
        if queued.is_empty() {
            return;
        }
        self.update_pending_messages_display();
        let session = self.session();
        let mut rest = queued.iter();
        if !will_retry && !session.is_streaming() {
            if let Some(first) = rest.next() {
                self.queue_prompt(first.text.clone(), Vec::new());
            }
            if session.is_idle() {
                for message in rest.by_ref() {
                    self.queue_prompt(message.text.clone(), Vec::new());
                }
            }
        }
        let rest: Vec<CompactionQueued> = rest.cloned().collect();
        if rest.is_empty() {
            return;
        }
        self.spawn(move |mode| async move {
            let queue = tokio::spawn(async move {
                for message in rest {
                    let behavior =
                        if message.follow_up { StreamingBehavior::FollowUp } else { StreamingBehavior::Steer };
                    session.queue_user_input(&message.text, Vec::new(), behavior, "interactive").await?;
                }
                anyhow::Ok(())
            });
            if let Err(error) = super::shutdown::joined(queue.await) {
                mode.show_error(&format!("Failed to send queued message: {error:#}"));
            }
            mode.update_pending_messages_display();
        });
    }

    /// Enter or Alt+Enter while streaming: the prompt path with a streaming behavior, so
    /// extension commands run, `input` handlers see it, and templates expand (upstream
    /// `session.prompt(text, { streamingBehavior })`).
    fn prompt_while_streaming(&self, text: String, behavior: StreamingBehavior) {
        let session = self.session();
        self.spawn(move |mode| async move {
            let options = PromptOptions { streaming_behavior: Some(behavior), ..PromptOptions::default() };
            let prompt = tokio::spawn(async move { session.prompt_with(&text, options).await });
            if let Err(error) = super::shutdown::joined(prompt.await) {
                mode.show_error(&format!("{error:#}"));
            }
            mode.update_pending_messages_display();
            mode.request_render();
        });
    }

    /// Enter in the editor: slash commands, `!` commands, queueing, or a new prompt.
    pub(super) fn handle_submit(&self, text: &str) {
        let text = text.trim().to_string();
        if text.is_empty() {
            return;
        }
        if self.run_slash_command(&text) {
            return;
        }
        if let Some(rest) = text.strip_prefix('!') {
            let (excluded, command) = match rest.strip_prefix('!') {
                Some(command) => (true, command.trim()),
                None => (false, rest.trim()),
            };
            if !command.is_empty() {
                if self.session().is_bash_running() {
                    self.show_warning("A bash command is already running. Press Esc to cancel it first.");
                    self.editor.set_text(&text);
                    return;
                }
                self.editor.add_to_history(&text);
                self.handle_bash_command(command.to_string(), excluded);
                self.state.borrow_mut().is_bash_mode = false;
                self.update_editor_border_color();
                return;
            }
        }
        let session = self.session();
        if session.is_compacting() {
            self.queue_compaction_message(text, false);
            return;
        }
        if session.is_streaming() {
            self.editor.add_to_history(&text);
            self.editor.set_text("");
            self.prompt_while_streaming(text, StreamingBehavior::Steer);
            return;
        }
        self.flush_pending_bash_components();
        self.queue_prompt(text.clone(), Vec::new());
        self.editor.add_to_history(&text);
    }

    fn flush_pending_bash_components(&self) {
        let components = std::mem::take(&mut self.state.borrow_mut().pending_bash_components);
        for bash in components {
            let bash: pi_tui::ComponentRef = bash;
            self.ui.pending.borrow_mut().remove_child(&bash);
            self.add_to_chat(bash);
        }
    }

    /// `!command` (sent to the model) or `!!command` (kept out of its context).
    fn handle_bash_command(&self, command: String, exclude_from_context: bool) {
        let session = self.session();
        let pad = self.state.borrow().output_pad;
        let bash = Rc::new(RefCell::new(BashExecutionComponent::new(&command, &self.tui(), exclude_from_context, pad)));
        if session.is_streaming() {
            self.ui.pending.borrow_mut().add_child(bash.clone());
            self.state.borrow_mut().pending_bash_components.push(bash.clone());
        } else {
            self.add_to_chat(bash.clone());
        }
        self.state.borrow_mut().bash_component = Some(bash.clone());
        self.request_render();
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel::<String>();
        let run = tokio::spawn(async move {
            let streamed = std::sync::atomic::AtomicBool::new(false);
            let forward = |chunk: &str| {
                streamed.store(true, std::sync::atomic::Ordering::SeqCst);
                let _ = sender.send(chunk.to_string());
            };
            if let Some(result) = session.extension_user_bash_with(&command, exclude_from_context, &forward).await {
                if !streamed.load(std::sync::atomic::Ordering::SeqCst) && !result.output.is_empty() {
                    let _ = sender.send(result.output.clone());
                }
                session.record_bash_result(&command, &result, exclude_from_context);
                return Ok(result);
            }
            let mut on_chunk = move |chunk: &str| {
                let _ = sender.send(chunk.to_string());
            };
            session.execute_bash(&command, exclude_from_context, None, &mut on_chunk).await
        });
        self.spawn(move |mode| async move {
            while let Some(chunk) = receiver.recv().await {
                bash.borrow_mut().append_output(&chunk);
                mode.request_render();
            }
            match run.await {
                Ok(Ok(result)) => bash.borrow_mut().set_complete(
                    result.exit_code.map(i64::from),
                    result.cancelled,
                    result.truncated,
                    result.full_output_path.map(|path| path.display().to_string()),
                ),
                Ok(Err(error)) => {
                    bash.borrow_mut().set_complete(None, false, false, None);
                    mode.show_error(&format!("Bash command failed: {error}"));
                }
                Err(error) => {
                    bash.borrow_mut().set_complete(None, false, false, None);
                    mode.show_error(&format!("Bash command failed: {error}"));
                }
            }
            mode.state.borrow_mut().bash_component = None;
            mode.request_render();
        });
    }

    /// Ctrl+V: copied file paths, then an image saved to a temporary file, then plain text.
    fn handle_clipboard_paste(&self) {
        let bash_mode = self.state.borrow().is_bash_mode;
        self.spawn(move |mode| async move {
            let read = tokio::task::spawn_blocking(move || {
                if let Some(paths) = crate::clipboard::read_clipboard_file_paths() {
                    if paths.iter().any(|path| path.chars().any(char::is_control)) {
                        return Err("Clipboard file path contains control characters".to_string());
                    }
                    let joined = if bash_mode {
                        paths.iter().map(|path| quote_if_needed(path)).collect::<Vec<_>>().join(" ")
                    } else {
                        paths.join("\n")
                    };
                    return Ok(Some((joined, true)));
                }
                if let Some(image) = crate::clipboard::read_clipboard_image() {
                    let path = crate::clipboard::save_clipboard_image(&image).map_err(|e| e.to_string())?;
                    return Ok(Some((path.display().to_string(), false)));
                }
                Ok(crate::clipboard::read_clipboard_text().map(|text| (text, false)))
            })
            .await;
            match read {
                Ok(Ok(Some((text, spaced)))) => {
                    let text = if spaced { mode.pad_for_cursor(&text) } else { text };
                    mode.editor.insert_text_at_cursor(&text);
                    mode.request_render();
                }
                Ok(Ok(None)) => {}
                Ok(Err(error)) => mode.show_error(&format!("Failed to paste from clipboard: {error}")),
                Err(error) => mode.show_error(&format!("Failed to paste from clipboard: {error}")),
            }
        });
    }

    /// Spaces around pasted paths when the cursor touches other text.
    fn pad_for_cursor(&self, text: &str) -> String {
        let cursor = self.editor.get_cursor();
        let current = self.editor.get_text();
        let line = current.split('\n').nth(cursor.line).unwrap_or("");
        let before = line[..cursor.col.min(line.len())].chars().next_back();
        let after = line.get(cursor.col..).and_then(|rest| rest.chars().next());
        let lead = if before.is_some_and(|c| !c.is_whitespace()) { " " } else { "" };
        let trail = if after.is_some_and(|c| !c.is_whitespace()) { " " } else { "" };
        format!("{lead}{text}{trail}")
    }

    /// Right click in fullscreen (Windows): paste clipboard text as a bracketed paste.
    pub(super) fn handle_right_click_paste(&self) {
        let Some(target) = self.tui().get_focused_component() else { return };
        self.spawn(move |mode| async move {
            let text = tokio::task::spawn_blocking(crate::clipboard::read_clipboard_text).await.ok().flatten();
            let Some(text) = text.filter(|text| !text.is_empty()) else { return };
            let focused = mode.tui().get_focused_component();
            if !focused.is_some_and(|focused| pi_tui::same_component(&focused, &target)) {
                return;
            }
            if let Ok(mut target) = target.try_borrow_mut() {
                target.handle_input(&format!("\x1b[200~{text}\x1b[201~"));
            }
            mode.request_render();
        });
    }
}
