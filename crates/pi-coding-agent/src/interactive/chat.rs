//! The transcript: messages, tool rows, notices, and status lines (upstream `addMessageToChat`,
//! `renderSessionItems`, `showStatus`, `showError`, and `showWarning`).

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use pi_ai::{Message, StopReason, Usage};
use pi_tui::{ComponentRef, Spacer, component};
use serde_json::Value;

use super::components::summaries::user_content_text;
use super::components::{
    AssistantMessageComponent, BashExecutionComponent, Collapsible, CollapsibleMessage, CustomMessageComponent,
    RenderOptions, ThemedText, ToolDisplay, ToolExecutionComponent, ToolExecutionOptions, UserMessageComponent,
    set_component_expanded,
};
use super::mode::{InteractiveMode, LastStatus};
use super::renderers::{ToolResultView, builtin_renderer};
use super::util::format_tokens;
use crate::cache_stats::{CacheMiss, cache_miss_notice, collect_cache_misses, format_cache_warming_usage};
use crate::markdown_transform::create_mermaid_markdown_transformer;
use crate::markdown_transform::{ActiveThemeStyle, MermaidTransformerOptions};
use crate::messages::{
    BashExecutionMessage, BranchSummaryMessage, CompactionSummaryMessage, CustomMessage, CustomRole,
};
use crate::resources::parse_skill_block;
use crate::session::entry::{Entry, EntryData};
use crate::session::session_entry_to_context_messages;
use crate::theme::{ThemeColor, get_markdown_theme, theme};

/// One transcript item of a session replay.
pub(super) enum RenderItem {
    Message {
        message: Box<Message>,
        entry_id: Option<String>,
    },
    CacheWarm(crate::session::entry::UsageEntry),
    SummaryCost {
        compaction: bool,
        usage: Usage,
    },
    /// A `custom` entry, drawn when an extension registered a renderer for it.
    CustomEntry(serde_json::Value),
}

/// The text of an aborted tool row, with the retry count.
pub(super) fn abort_message(retry_attempt: u32) -> String {
    match retry_attempt {
        0 => "Operation aborted".to_string(),
        1 => "Aborted after 1 retry attempt".to_string(),
        n => format!("Aborted after {n} retry attempts"),
    }
}

impl InteractiveMode {
    /// Markdown theme with the code block indent setting, and the mermaid transformer.
    pub(super) fn render_options(&self) -> RenderOptions {
        let settings = self.settings();
        let mut markdown_theme = get_markdown_theme();
        markdown_theme.code_block_indent = Some(settings.code_block_indent());
        let mode_source = settings.clone();
        let mermaid = create_mermaid_markdown_transformer(MermaidTransformerOptions {
            get_mode: Arc::new(move || mode_source.mermaid_rendering_mode()),
            theme: Some(Arc::new(ActiveThemeStyle)),
        });
        let mut transformers = vec![mermaid];
        transformers.extend(self.extension_markdown_transformers());
        RenderOptions { markdown_theme, output_pad: self.state.borrow().output_pad, transformers }
    }

    pub(super) fn add_to_chat(&self, child: ComponentRef) {
        self.ui.chat.borrow_mut().add_child(child);
    }

    pub(super) fn add_spacer(&self) {
        self.add_to_chat(component(Spacer::new(1)));
    }

    pub(super) fn add_text(&self, build: impl Fn() -> String + 'static, padding_x: usize, padding_y: usize) {
        self.add_to_chat(component(ThemedText::new(build, padding_x, padding_y)));
    }

    fn chat_is_empty(&self) -> bool {
        self.ui.chat.borrow().children.is_empty()
    }

    /// A dim status line; back-to-back statuses replace the line instead of adding one.
    pub fn show_status(&self, message: &str) {
        let reuse = {
            let state = self.state.borrow();
            let chat = self.ui.chat.borrow();
            let children = &chat.children;
            match (&state.last_status, children.len()) {
                (Some(last), len) if len >= 2 => {
                    pi_tui::same_component(&children[len - 1], &last.text)
                        && pi_tui::same_component(&children[len - 2], &last.spacer)
                }
                _ => false,
            }
        };
        if reuse {
            let state = self.state.borrow();
            if let Some(last) = &state.last_status {
                *last.message.borrow_mut() = message.to_string();
                if let Ok(mut text) = last.text.try_borrow_mut() {
                    text.invalidate();
                }
            }
            drop(state);
            self.request_render();
            return;
        }
        let shared = Rc::new(RefCell::new(message.to_string()));
        let source = shared.clone();
        let spacer: ComponentRef = component(Spacer::new(1));
        let text: ComponentRef =
            component(ThemedText::new(move || theme().fg(ThemeColor::Dim, &source.borrow()), 1, 0));
        self.add_to_chat(spacer.clone());
        self.add_to_chat(text.clone());
        self.state.borrow_mut().last_status = Some(LastStatus { spacer, text, message: shared });
        self.request_render();
    }

    pub fn show_error(&self, message: &str) {
        let message = format!("Error: {message}");
        let pad = self.state.borrow().output_pad;
        self.add_spacer();
        self.add_text(move || theme().fg(ThemeColor::Error, &message), pad, 0);
        self.request_render();
    }

    pub fn show_warning(&self, message: &str) {
        let message = format!("Warning: {message}");
        self.add_spacer();
        self.add_text(move || theme().fg(ThemeColor::Warning, &message), 1, 0);
        self.request_render();
    }

    pub(super) fn tool_options(&self) -> ToolExecutionOptions {
        let settings = self.settings();
        ToolExecutionOptions {
            show_images: settings.show_images(),
            image_width_cells: settings.image_width_cells() as usize,
            output_pad: self.state.borrow().output_pad,
        }
    }

    /// Built-in renderer, a registered tool without one, or an unknown tool.
    fn tool_display(&self, name: &str) -> ToolDisplay {
        if let Some(renderer) = self.extension_tool_renderer(name) {
            return ToolDisplay::Renderer(renderer);
        }
        if let Some(renderer) = builtin_renderer(name) {
            return ToolDisplay::Renderer(renderer);
        }
        if self.session().all_tools().iter().any(|tool| tool.name == name) {
            ToolDisplay::Generic
        } else {
            ToolDisplay::Unknown
        }
    }

    pub(super) fn new_tool_component(&self, name: &str, id: &str, args: Value) -> Rc<RefCell<ToolExecutionComponent>> {
        let cwd = self.session().cwd().to_path_buf();
        let mut tool =
            ToolExecutionComponent::new(name, id, args, self.tool_options(), self.tool_display(name), &self.tui(), cwd);
        tool.set_expanded(self.state.borrow().tool_output_expanded);
        let tool = Rc::new(RefCell::new(tool));
        self.add_to_chat(tool.clone());
        tool
    }

    fn add_collapsible(&self, kind: Collapsible) {
        let mut message = CollapsibleMessage::new(kind, self.render_options());
        message.set_expanded(self.state.borrow().tool_output_expanded);
        self.add_to_chat(component(message));
    }

    /// Upstream `addMessageToChat`. Tool results render inside their tool rows.
    pub(super) fn add_message_to_chat(&self, message: &Message, populate_history: bool) {
        match message {
            Message::User(user) => {
                let text = user_content_text(&user.content);
                if text.is_empty() {
                    return;
                }
                if !self.chat_is_empty() {
                    self.add_spacer();
                }
                let options = self.render_options();
                match parse_skill_block(&text) {
                    Some(block) => {
                        let user_message = block.user_message.clone();
                        self.add_collapsible(Collapsible::Skill(block));
                        if let Some(user_message) = user_message.filter(|text| !text.is_empty()) {
                            self.add_spacer();
                            self.add_to_chat(component(UserMessageComponent::new(&user_message, &options)));
                        }
                    }
                    None => self.add_to_chat(component(UserMessageComponent::new(&text, &options))),
                }
                if populate_history {
                    self.editor.add_to_history(&text);
                }
            }
            Message::Assistant(assistant) => {
                let (hide, label) = {
                    let state = self.state.borrow();
                    (state.hide_thinking_block, state.hidden_thinking_label.clone())
                };
                let component = AssistantMessageComponent::new(Some(assistant), hide, &label, self.render_options());
                self.add_to_chat(Rc::new(RefCell::new(component)));
            }
            Message::Custom(_) => self.add_custom_role_message(message),
            Message::System(_) | Message::ToolResult(_) => {}
        }
    }

    fn add_custom_role_message(&self, message: &Message) {
        if let Some(bash) = BashExecutionMessage::from_message(message) {
            let pad = self.state.borrow().output_pad;
            let exclude = bash.exclude_from_context.unwrap_or(false);
            let mut component = BashExecutionComponent::new(&bash.command, &self.tui(), exclude, pad);
            if !bash.output.is_empty() {
                component.append_output(&bash.output);
            }
            component.set_complete(bash.exit_code, bash.cancelled, bash.truncated, bash.full_output_path);
            component.set_expanded(self.state.borrow().tool_output_expanded);
            self.add_to_chat(pi_tui::component(component));
        } else if let Some(custom) = CustomMessage::from_message(message) {
            if custom.display {
                let renderer = self.extension_message_renderer(&custom);
                let mut component = CustomMessageComponent::with_renderer(custom, self.render_options(), renderer);
                component.set_expanded(self.state.borrow().tool_output_expanded);
                self.add_to_chat(pi_tui::component(component));
            }
        } else if let Some(summary) = CompactionSummaryMessage::from_message(message) {
            self.add_spacer();
            self.add_collapsible(Collapsible::Compaction {
                summary: summary.summary,
                tokens_before: summary.tokens_before,
            });
        } else if let Some(summary) = BranchSummaryMessage::from_message(message) {
            self.add_spacer();
            self.add_collapsible(Collapsible::BranchSummary { summary: summary.summary });
        }
    }

    /// Transcript items of `entries`, with summary billing after compactions and branch summaries.
    pub(super) fn render_items_of(entries: &[Entry]) -> Vec<RenderItem> {
        let mut items = Vec::new();
        for entry in entries {
            if let EntryData::Custom { .. } = entry.data() {
                items.push(RenderItem::CustomEntry(serde_json::Value::Object(entry.raw().clone())));
                continue;
            }
            if let EntryData::Usage(usage) = entry.data()
                && usage.kind == "cache_warm"
            {
                items.push(RenderItem::CacheWarm(usage.clone()));
                continue;
            }
            let messages = session_entry_to_context_messages(entry);
            let has_messages = !messages.is_empty();
            items.extend(
                messages.into_iter().map(|message| RenderItem::Message {
                    message: Box::new(message),
                    entry_id: Some(entry.id().into()),
                }),
            );
            let cost = match entry.data() {
                EntryData::Compaction(compaction) => compaction.usage.map(|usage| (true, usage)),
                EntryData::BranchSummary(summary) => summary.usage.map(|usage| (false, usage)),
                _ => None,
            };
            if let (Some((compaction, usage)), true) = (cost, has_messages) {
                items.push(RenderItem::SummaryCost { compaction, usage });
            }
        }
        items
    }

    /// Replay entries into the chat (upstream `renderSessionEntries`).
    pub(super) fn render_session_entries(&self, entries: &[Entry], update_footer: bool, populate_history: bool) {
        let tui = self.tui();
        if tui.is_viewport() {
            tui.reset_text_selection();
        }
        self.render_session_items(Self::render_items_of(entries), update_footer, populate_history);
    }

    pub(super) fn render_session_items(&self, items: Vec<RenderItem>, update_footer: bool, populate_history: bool) {
        self.state.borrow_mut().pending_tools.clear();
        let session = self.session();
        let cache_misses = if self.settings().show_cache_miss_notices() {
            let manager = session.session_manager();
            collect_cache_misses(manager.entries(), session.model_runtime().as_ref())
        } else {
            Default::default()
        };
        if update_footer {
            self.update_editor_border_color();
        }
        let mut pending: Vec<(String, Rc<RefCell<ToolExecutionComponent>>)> = Vec::new();
        for item in items {
            match item {
                RenderItem::CacheWarm(usage) => self.add_cache_warming_usage(&usage),
                RenderItem::CustomEntry(entry) => self.add_custom_entry_to_chat(&entry),
                RenderItem::SummaryCost { compaction, usage } => self.add_summary_cost_notice(compaction, &usage),
                RenderItem::Message { message, entry_id } => match *message {
                    Message::Assistant(assistant) => {
                        self.add_message_to_chat(&Message::Assistant(assistant.clone()), false);
                        let failed = matches!(assistant.stop_reason, StopReason::Aborted | StopReason::Error);
                        for block in &assistant.content {
                            let pi_ai::ContentBlock::ToolCall(call) = block else { continue };
                            let id = &call.id;
                            let tool = self.new_tool_component(&call.name, id, Value::Object(call.arguments.clone()));
                            if failed {
                                let text = match assistant.stop_reason {
                                    StopReason::Aborted => abort_message(session.retry_attempt()),
                                    _ => assistant.error_message.clone().unwrap_or_else(|| "Error".into()),
                                };
                                tool.borrow_mut().update_result(error_result(&text), false);
                            } else {
                                pending.push((id.clone(), tool));
                            }
                        }
                        if !failed && let Some(miss) = entry_id.and_then(|id| cache_misses.get(&id).copied()) {
                            self.add_cache_miss_notice(miss);
                        }
                    }
                    Message::ToolResult(result) => {
                        if let Some(index) = pending.iter().position(|(id, _)| *id == result.tool_call_id) {
                            let (_, tool) = pending.remove(index);
                            let view = ToolResultView {
                                content: result.content.clone(),
                                details: result.details.clone(),
                                is_error: result.is_error,
                                duration_ms: None,
                            };
                            tool.borrow_mut().update_result(view, false);
                        }
                    }
                    message => self.add_message_to_chat(&message, populate_history),
                },
            }
        }
        self.state.borrow_mut().pending_tools = pending;
        self.request_render();
    }

    fn add_cache_warming_usage(&self, usage: &crate::session::entry::UsageEntry) {
        if !self.settings().show_cache_miss_notices() {
            return;
        }
        let text = format_cache_warming_usage(usage);
        self.add_spacer();
        self.add_text(move || theme().fg(ThemeColor::Dim, &text), 1, 0);
    }

    /// Billing of a compaction or branch summary, derived from the stored usage.
    pub(super) fn add_summary_cost_notice(&self, compaction: bool, usage: &Usage) {
        if !self.settings().show_cache_miss_notices() {
            return;
        }
        let tokens = usage.input + usage.output + usage.cache_read + usage.cache_write;
        let cost = if usage.cost.total >= 0.01 { format!(" (~${:.2})", usage.cost.total) } else { String::new() };
        let label = if compaction { "Compaction" } else { "Branch summary" };
        let text = format!("{label}: {} tokens billed{cost}", format_tokens(tokens));
        self.add_spacer();
        self.add_text(move || theme().fg(ThemeColor::Warning, &text), 1, 0);
    }

    pub(super) fn add_cache_miss_notice(&self, miss: CacheMiss) {
        if let Some(notice) = cache_miss_notice(&miss) {
            self.add_warning_notice(notice);
        }
    }

    /// A transcript notice in the warning color, such as a cache miss.
    pub(super) fn add_warning_notice(&self, text: String) {
        self.add_spacer();
        self.add_text(move || theme().fg(ThemeColor::Warning, &text), 1, 0);
    }

    /// Replay the session context, then the trust warning and compaction count.
    pub(super) fn render_initial_messages(&self) {
        let session = self.session();
        let (entries, compactions) = {
            let manager = session.session_manager();
            let entries: Vec<Entry> = manager.build_context_entries().into_iter().cloned().collect();
            let compactions = manager.entries().iter().filter(|entry| entry.compaction().is_some()).count();
            (entries, compactions)
        };
        self.render_session_entries(&entries, true, true);
        self.render_project_trust_warning();
        if compactions > 0 {
            let times = if compactions == 1 { "1 time".to_string() } else { format!("{compactions} times") };
            self.show_status(&format!("Session compacted {times}"));
        }
    }

    fn render_project_trust_warning(&self) {
        let session = self.session();
        let home = crate::config::home_dir();
        if session.settings().is_project_trusted()
            || !crate::resources::trust::has_trust_requiring_project_resources(session.cwd(), &home)
        {
            return;
        }
        if !self.chat_is_empty() {
            self.add_spacer();
        }
        let dir = crate::resources::brand::CONFIG_DIR_NAME;
        let text = format!(
            "This project is not trusted. Project {dir} resources and packages are ignored. Use /trust to save a trust decision, then restart pi."
        );
        self.add_text(move || theme().fg(ThemeColor::Warning, &text), 1, 0);
    }

    pub(super) fn rebuild_chat_from_messages(&self) {
        self.ui.chat.borrow_mut().clear();
        let entries: Vec<Entry> =
            self.session().session_manager().build_context_entries().into_iter().cloned().collect();
        self.render_session_entries(&entries, false, false);
    }

    /// Show the current session from scratch, after a session switch.
    pub(super) fn render_current_session_state(&self) {
        self.ui.loaded_resources.borrow_mut().clear();
        self.ui.chat.borrow_mut().clear();
        self.ui.pending.borrow_mut().clear();
        {
            let mut state = self.state.borrow_mut();
            state.compaction_queue.clear();
            state.streaming_component = None;
            state.streaming_message = None;
            state.pending_tools.clear();
            state.last_status = None;
        }
        self.render_initial_messages();
    }

    /// Ctrl+O: expand or collapse tool output, summaries, and the startup listing.
    pub(super) fn set_tools_expanded(&self, expanded: bool) {
        if expanded == self.state.borrow().tool_output_expanded {
            return;
        }
        self.state.borrow_mut().tool_output_expanded = expanded;
        let header = self.state.borrow().header.clone();
        if let Some(header) = header {
            header.borrow_mut().set_expanded(expanded);
        }
        for container in [&self.ui.loaded_resources, &self.ui.chat] {
            let children = container.borrow().children.clone();
            for child in &children {
                set_component_expanded(child, expanded);
            }
        }
        self.show_status(&format!("Tool output: {}", if expanded { "expanded" } else { "collapsed" }));
    }

    pub(super) fn toggle_tool_output_expansion(&self) {
        let expanded = self.state.borrow().tool_output_expanded;
        self.set_tools_expanded(!expanded);
    }

    /// Ctrl+T: hide or show thinking blocks, saved in the settings.
    pub(super) fn toggle_thinking_block_visibility(&self) {
        let hide = !self.state.borrow().hide_thinking_block;
        self.state.borrow_mut().hide_thinking_block = hide;
        self.settings().set_hide_thinking_block(hide);
        self.update_thinking_block_visibility();
        self.show_status(&format!("Thinking blocks: {}", if hide { "hidden" } else { "visible" }));
    }

    pub(super) fn update_thinking_block_visibility(&self) {
        let hide = self.state.borrow().hide_thinking_block;
        let children = self.ui.chat.borrow().children.clone();
        for child in &children {
            if let Ok(mut child) = child.try_borrow_mut()
                && let Some(message) = child.as_any_mut().downcast_mut::<AssistantMessageComponent>()
            {
                message.set_hide_thinking_block(hide);
            }
        }
        self.request_render();
    }
}

pub(super) fn error_result(text: &str) -> ToolResultView {
    ToolResultView { content: vec![pi_ai::ContentBlock::text(text)], details: None, is_error: true, duration_ms: None }
}
