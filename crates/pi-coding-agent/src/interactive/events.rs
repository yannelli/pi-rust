//! Session events on the UI thread (upstream `InteractiveMode.handleEvent`).

use std::cell::RefCell;
use std::rc::Rc;

use pi_ai::{ContentBlock, Message, StopReason};
use serde_json::Value;

use super::bridge::UiEvent;
use super::chat::{abort_message, error_result};
use super::components::{AssistantMessageComponent, StatusIndicator, StatusKind};
use super::mode::InteractiveMode;
use super::renderers::ToolResultView;
use crate::agent_session::{CompactionReason, SummarizationSource};
use crate::compaction::CompactionResult;
use crate::session::entry::Entry;
use crate::theme::{ThemeColor, theme};

impl InteractiveMode {
    pub(super) fn handle_event(&self, event: UiEvent) {
        if let Ok(mut footer) = self.footer.try_borrow_mut() {
            pi_tui::Component::invalidate(&mut *footer);
        }
        if self.program_status.borrow_mut().handle_event(&event) {
            self.report_program_status();
        }
        match event {
            UiEvent::AgentStart => {
                self.state.borrow_mut().pending_tools.clear();
                let retry = self.state.borrow_mut().retry_escape.take();
                if let Some(handler) = retry {
                    self.editor.set_on_escape(handler);
                }
            }
            UiEvent::TurnStart => {
                self.set_terminal_progress(true);
                if self.state.borrow().working_visible {
                    if self.active_status_kind() != Some(StatusKind::Working) {
                        self.show_working_status_indicator();
                    }
                } else {
                    self.clear_status_indicator(None);
                }
            }
            UiEvent::QueueUpdate => self.update_pending_messages_display(),
            UiEvent::EntryAppended(entry) => self.on_entry_appended(entry),
            UiEvent::SessionInfoChanged => self.update_terminal_title(),
            UiEvent::ThinkingLevelChanged => self.update_editor_border_color(),
            UiEvent::MessageStart(message) => self.on_message_start(message),
            UiEvent::MessageUpdate(message) => self.on_message_update(message),
            UiEvent::MessageEnd(message) => self.on_message_end(message),
            UiEvent::ToolStart { id, name, args, nested } => {
                if nested {
                    return;
                }
                let tool = self.pending_tool(&id).unwrap_or_else(|| {
                    let tool = self.new_tool_component(&name, &id, args);
                    self.state.borrow_mut().pending_tools.push((id.clone(), tool.clone()));
                    tool
                });
                tool.borrow_mut().mark_execution_started();
            }
            UiEvent::ToolUpdate { id, partial } => {
                if let Some(tool) = self.pending_tool(&id) {
                    let view = ToolResultView {
                        content: partial.content,
                        details: partial.details,
                        is_error: false,
                        duration_ms: None,
                    };
                    tool.borrow_mut().update_result(view, true);
                }
            }
            UiEvent::ToolEnd { id, result, is_error, duration_ms } => {
                if let Some(tool) = self.pending_tool(&id) {
                    let view =
                        ToolResultView { content: result.content, details: result.details, is_error, duration_ms };
                    tool.borrow_mut().update_result(view, false);
                    self.state.borrow_mut().pending_tools.retain(|(pending, _)| *pending != id);
                }
            }
            UiEvent::AgentEnd => {
                self.set_terminal_progress(false);
                self.clear_status_indicator(Some(StatusKind::Working));
                let streaming = {
                    let mut state = self.state.borrow_mut();
                    state.streaming_message = None;
                    state.pending_tools.clear();
                    state.streaming_component.take()
                };
                if let Some(streaming) = streaming {
                    let streaming: pi_tui::ComponentRef = streaming;
                    self.ui.chat.borrow_mut().remove_child(&streaming);
                }
            }
            UiEvent::AgentSettled { .. } => {}
            UiEvent::CompactionStart { reason } => {
                self.set_terminal_progress(true);
                let abort = self.action(|mode| mode.session().abort_compaction());
                let replaced = self.editor.set_on_escape(Some(abort));
                self.state.borrow_mut().compaction_escape = Some(replaced);
                self.show_compaction_indicator(reason);
            }
            UiEvent::CompactionEnd { reason, result, aborted, will_retry, error_message } => {
                self.on_compaction_end(reason, result, aborted, error_message);
                self.flush_compaction_queue(will_retry);
            }
            UiEvent::AutoRetryStart { attempt, max_attempts, delay_ms } => {
                let abort = self.action(|mode| mode.session().abort_retry());
                let replaced = self.editor.set_on_escape(Some(abort));
                self.state.borrow_mut().retry_escape = Some(replaced);
                self.show_status_indicator(StatusIndicator::retry(&self.tui(), attempt, max_attempts, delay_ms));
            }
            UiEvent::AutoRetryEnd { success, attempt, final_error } => {
                let retry = self.state.borrow_mut().retry_escape.take();
                if let Some(handler) = retry {
                    self.editor.set_on_escape(handler);
                }
                self.clear_status_indicator(Some(StatusKind::Retry));
                if !success {
                    let error = final_error.unwrap_or_else(|| "Unknown error".into());
                    self.show_error(&format!("Retry failed after {attempt} attempts: {error}"));
                }
            }
            UiEvent::SummarizationRetryScheduled { attempt, max_attempts, delay_ms, error_message } => {
                self.show_error(&error_message);
                self.show_status_indicator(StatusIndicator::retry(&self.tui(), attempt, max_attempts, delay_ms));
            }
            UiEvent::SummarizationRetryAttemptStart { source } => {
                self.clear_status_indicator(Some(StatusKind::Retry));
                match source {
                    SummarizationSource::BranchSummary => {
                        self.show_status_indicator(StatusIndicator::branch_summary(&self.tui()))
                    }
                    SummarizationSource::Compaction(reason) => self.show_compaction_indicator(reason),
                }
            }
            UiEvent::SummarizationRetryFinished => self.clear_status_indicator(Some(StatusKind::Retry)),
            UiEvent::Rebind(_) => self.rebind_current_session(),
            UiEvent::Error(message) => self.show_error(&message),
            UiEvent::ThemeChanged => {
                self.tui().invalidate();
                self.update_editor_border_color();
            }
            UiEvent::Notify { message, level } => self.show_notice(&message, level),
            UiEvent::ModelRouted => {}
            UiEvent::CacheMiss(notice) => self.add_warning_notice(notice),
            UiEvent::Call(call) => {
                if let Some(mode) = self.weak.upgrade() {
                    (call.0)(&mode);
                }
            }
        }
        self.request_render();
    }

    fn pending_tool(&self, id: &str) -> Option<Rc<RefCell<super::components::ToolExecutionComponent>>> {
        let state = self.state.borrow();
        state.pending_tools.iter().find(|(pending, _)| pending == id).map(|(_, tool)| tool.clone())
    }

    fn on_entry_appended(&self, entry: Value) {
        let id = entry.get("id").and_then(Value::as_str).unwrap_or_default().to_string();
        if self.state.borrow_mut().boundary_rendered_entries.remove(&id) {
            return;
        }
        let Some(object) = entry.as_object() else { return };
        let entry = Entry::from_raw(object.clone());
        match entry.data() {
            crate::session::entry::EntryData::CustomMessage(custom) if custom.display => {
                let message = crate::messages::create_custom_message(
                    &custom.custom_type,
                    custom.content.clone(),
                    custom.display,
                    custom.details.clone(),
                    entry.timestamp(),
                );
                self.add_message_to_chat(&message, false);
            }
            crate::session::entry::EntryData::Usage(_) => {
                let items = Self::render_items_of(std::slice::from_ref(&entry));
                self.render_session_items(items, false, false);
            }
            crate::session::entry::EntryData::Compaction(_) => self.on_compaction_entry(&entry),
            _ => {}
        }
    }

    /// A compaction appended outside a compaction run (an extension or recovery): rebuild the
    /// chat with the summary at its position.
    fn on_compaction_entry(&self, entry: &Entry) {
        let session = self.session();
        let (entries, after) = {
            let manager = session.session_manager();
            let entries: Vec<Entry> = manager.build_context_entries().into_iter().cloned().collect();
            let branch = manager.get_branch(None);
            let index = branch.iter().position(|candidate| candidate.id() == entry.id());
            let after: Vec<String> = index
                .map(|index| branch[index + 1..].iter().map(|entry| entry.id().to_string()).collect())
                .unwrap_or_default();
            (entries, after)
        };
        if entries.first().map(Entry::id) != Some(entry.id()) {
            return;
        }
        self.ui.chat.borrow_mut().clear();
        let retained = &entries[1..];
        let (before, later): (Vec<Entry>, Vec<Entry>) =
            retained.iter().cloned().partition(|candidate| !after.iter().any(|id| id == candidate.id()));
        self.render_session_entries(&before, false, false);
        let summary = entries[0].compaction().cloned();
        if let Some(compaction) = summary {
            let message = crate::messages::create_compaction_summary_message(
                &compaction.summary,
                compaction.tokens_before,
                entry.timestamp(),
            );
            self.add_message_to_chat(&message, false);
            if let Some(usage) = compaction.usage {
                self.add_summary_cost_notice(true, &usage);
            }
        }
        self.render_session_entries(&later, false, false);
        self.state.borrow_mut().boundary_rendered_entries.extend(after);
    }

    fn on_message_start(&self, message: Message) {
        match &message {
            Message::Custom(_) => self.add_message_to_chat(&message, false),
            Message::User(_) => {
                self.add_message_to_chat(&message, false);
                self.update_pending_messages_display();
            }
            Message::Assistant(assistant) => {
                let (hide, label) = {
                    let state = self.state.borrow();
                    (state.hide_thinking_block, state.hidden_thinking_label.clone())
                };
                let mut component = AssistantMessageComponent::new(None, hide, &label, self.render_options());
                component.update_content(assistant, true);
                let component = Rc::new(RefCell::new(component));
                self.add_to_chat(component.clone());
                let mut state = self.state.borrow_mut();
                state.streaming_component = Some(component);
                state.streaming_message = Some(assistant.clone());
            }
            Message::System(_) | Message::ToolResult(_) => {}
        }
    }

    fn on_message_update(&self, message: Message) {
        let Message::Assistant(assistant) = message else { return };
        let Some(streaming) = self.state.borrow().streaming_component.clone() else { return };
        streaming.borrow_mut().update_content(&assistant, true);
        for block in &assistant.content {
            let ContentBlock::ToolCall(call) = block else { continue };
            let arguments = Value::Object(call.arguments.clone());
            match self.pending_tool(&call.id) {
                Some(tool) => tool.borrow_mut().update_args(arguments),
                None => {
                    let tool = self.new_tool_component(&call.name, &call.id, arguments);
                    self.state.borrow_mut().pending_tools.push((call.id.clone(), tool));
                }
            }
        }
        self.state.borrow_mut().streaming_message = Some(assistant);
    }

    fn on_message_end(&self, message: Message) {
        let Message::Assistant(mut assistant) = message else { return };
        let Some(streaming) = self.state.borrow().streaming_component.clone() else { return };
        let mut error = None;
        if assistant.stop_reason == StopReason::Aborted {
            let text = abort_message(self.session().retry_attempt());
            assistant.error_message = Some(text.clone());
            error = Some(text);
        }
        streaming.borrow_mut().update_content(&assistant, false);
        let tools: Vec<_> = self.state.borrow().pending_tools.iter().map(|(_, tool)| tool.clone()).collect();
        if matches!(assistant.stop_reason, StopReason::Aborted | StopReason::Error) {
            let text = error.or_else(|| assistant.error_message.clone()).unwrap_or_else(|| "Error".into());
            for tool in tools {
                tool.borrow_mut().update_result(error_result(&text), false);
            }
            self.state.borrow_mut().pending_tools.clear();
            self.maybe_suggest_bug_report(&assistant);
        } else {
            for tool in tools {
                tool.borrow_mut().set_args_complete();
            }
            self.maybe_show_thinking_drop_notice(&assistant);
        }
        let mut state = self.state.borrow_mut();
        state.streaming_component = None;
        state.streaming_message = None;
    }

    fn on_compaction_end(
        &self,
        reason: CompactionReason,
        result: Option<CompactionResult>,
        aborted: bool,
        error_message: Option<String>,
    ) {
        self.set_terminal_progress(false);
        let replaced = self.state.borrow_mut().compaction_escape.take();
        if let Some(handler) = replaced {
            self.editor.set_on_escape(handler);
        }
        self.clear_status_indicator(Some(StatusKind::Compaction));
        if aborted {
            if reason == CompactionReason::Manual {
                self.show_error("Compaction cancelled");
            } else {
                self.show_status("Auto-compaction cancelled");
            }
        } else if let Some(result) = result {
            let entries: Vec<Entry> =
                self.session().session_manager().build_context_entries().into_iter().cloned().collect();
            if entries.first().and_then(Entry::compaction).is_none() {
                self.show_error("Completed compaction is missing from the session context");
                return;
            }
            self.ui.chat.borrow_mut().clear();
            self.render_session_entries(&entries[1..], false, false);
            let timestamp = jiff::Timestamp::now().to_string();
            let message =
                crate::messages::create_compaction_summary_message(&result.summary, result.tokens_before, &timestamp);
            self.add_message_to_chat(&message, false);
            if result.usage.total_tokens > 0 || result.usage.cost.total > 0.0 {
                self.add_summary_cost_notice(true, &result.usage);
            }
        } else if let Some(error) = error_message {
            if reason == CompactionReason::Manual {
                self.show_error(&error);
            } else {
                self.add_spacer();
                self.add_text(move || theme().fg(ThemeColor::Error, &error), 1, 0);
            }
        }
    }

    fn maybe_show_thinking_drop_notice(&self, message: &pi_ai::AssistantMessage) {
        if !self.settings().show_cache_miss_notices() {
            return;
        }
        let dropped = count_dropped_thinking_blocks(message);
        if dropped == 0 {
            return;
        }
        let last_dropped = {
            let session = self.session();
            let manager = session.session_manager();
            let branch = manager.get_branch(None);
            branch
                .iter()
                .rev()
                .find_map(|entry| match entry.data() {
                    crate::session::entry::EntryData::Message(Message::Assistant(last)) => Some(last),
                    _ => None,
                })
                .map(count_dropped_thinking_blocks)
                .unwrap_or(0)
        };
        if dropped <= last_dropped {
            return;
        }
        let noun = if dropped == 1 { "thinking block" } else { "thinking blocks" };
        let text = format!("Anthropic dropped {dropped} {noun} (details in session)");
        self.add_spacer();
        self.add_text(move || theme().fg(ThemeColor::Warning, &text), 1, 0);
    }
}

fn count_dropped_thinking_blocks(message: &pi_ai::AssistantMessage) -> usize {
    message
        .diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.get("type").and_then(Value::as_str) == Some("anthropic_input_transformations"))
        .filter_map(|diagnostic| diagnostic.pointer("/details/transformations").and_then(Value::as_array))
        .flatten()
        .filter(|transformation| transformation.get("type").and_then(Value::as_str) == Some("thinking_dropped"))
        .count()
}
