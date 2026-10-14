//! Port of upstream `durable/tui.ts` (shared by `vacation/`): renders the `DurableView` with pi's interactive
//! components in the alternate screen. It runs on one thread inside a `LocalSet`, as pi's TUI does.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;

use pi_ai::{AssistantMessage, ContentBlock, Message, ThinkingLevel};
use pi_durable::{EntryId, EntryRecord};
use pi_tui::components::{ScrollFollow, ScrollOverscroll, StackChild, StackEntryOptions, StackOptions};
use pi_tui::layout_node::StackBasis;
use pi_tui::{
    ComponentRef, Container, EditorOptions, ScrollView, ScrollViewHandle, ScrollViewOptions, Spacer, Text, Tui, VStack,
    component,
};
use serde_json::{Value, json};

use super::runtime::{DurableView, agent_of};
use super::tui_parts::{CompactionCard, user_text};
use crate::interactive::components::RenderOptions;
use crate::interactive::components::custom_editor::{CustomEditor, EditorHandle};
use crate::interactive::components::messages::{AssistantMessageComponent, UserMessageComponent};
use crate::interactive::components::status_indicator::SharedIndicator;
use crate::interactive::components::tool_execution::{ToolDisplay, ToolExecutionComponent, ToolExecutionOptions};
use crate::interactive::renderers::{ToolResultView, builtin_renderer};
use crate::theme::get_editor_theme;
use crate::theme::{ThemeColor, theme};

pub(super) type Card = Rc<RefCell<ToolExecutionComponent>>;

fn shared() -> Rc<RefCell<Container>> {
    component(Container::new())
}

pub(super) fn fg(color: ThemeColor, text: &str) -> String {
    theme().fg(color, text)
}

pub(super) fn text_result(text: String, details: Option<Value>, is_error: bool) -> ToolResultView {
    let content = vec![ContentBlock::Text { text, text_signature: None }];
    ToolResultView { content, details, is_error, duration_ms: None }
}

#[derive(Default)]
pub(super) struct State {
    /// The newest card per call ID; provider call IDs may repeat across turns.
    tools: HashMap<String, Card>,
    /// Every card shown, also older ones whose call ID a later turn reused.
    cards: Vec<Card>,
    /// Call IDs whose cards the streaming answer created; its entry takes them over.
    streaming_calls: HashSet<String>,
    summaries: Vec<CompactionCard>,
    /// Tool output and summaries shown in full; toggled like pi.
    expanded: bool,
    rendered_entry_ids: Vec<EntryId>,
    streaming: Option<Rc<RefCell<AssistantMessageComponent>>>,
    pub(super) indicator: Option<SharedIndicator>,
    pub(super) status_text: String,
    /// Set when the transcript was rebuilt: the next render repaints the screen and shows the end.
    rebuilt: bool,
}

pub(crate) struct DurableTui {
    pub tui: Tui,
    chat: Rc<RefCell<Container>>,
    pub(super) tasks: Rc<RefCell<Container>>,
    pub(super) queue: Rc<RefCell<Container>>,
    pub(super) notices: Rc<RefCell<Container>>,
    pub(super) footer_stats: Rc<RefCell<Text>>,
    pub(super) footer_hints: Rc<RefCell<Text>>,
    editor_container: Rc<RefCell<Container>>,
    pub editor: EditorHandle,
    editor_component: ComponentRef,
    transcript: ScrollViewHandle,
    cwd: PathBuf,
    pub(super) state: RefCell<State>,
}

fn entry(child: ComponentRef, options: StackEntryOptions) -> StackChild {
    StackChild::Entry(child, options)
}

fn stack_entry(basis: Option<StackBasis>, grow: Option<usize>, min_size: usize) -> StackEntryOptions {
    StackEntryOptions { basis, grow, shrink: Some(1), min_size: Some(min_size), ..Default::default() }
}

impl DurableTui {
    pub fn new(tui: Tui, cwd: PathBuf) -> Rc<Self> {
        let editor_options = EditorOptions { padding_x: 1, ..Default::default() };
        let custom_editor = CustomEditor::new(&tui, get_editor_theme(), editor_options, true);
        let editor = custom_editor.handle();
        let editor_component: ComponentRef = component(custom_editor);
        let editor_container = shared();
        editor_container.borrow_mut().add_child(editor_component.clone());
        let (chat, tasks, queue, notices, footer) = (shared(), shared(), shared(), shared(), shared());
        let (footer_stats, footer_hints) = (component(Text::new("", 1, 0)), component(Text::new("", 1, 0)));
        footer.borrow_mut().add_child(footer_stats.clone());
        footer.borrow_mut().add_child(footer_hints.clone());
        // One empty line between the transcript and everything below it.
        let content = shared();
        content.borrow_mut().add_child(chat.clone());
        content.borrow_mut().add_child(component(Spacer::new(1)));
        let options = ScrollViewOptions {
            follow: ScrollFollow::End,
            primary: true,
            overscroll: ScrollOverscroll::Chain,
            ..ScrollViewOptions::default()
        };
        let transcript = ScrollView::new(content, options);
        let transcript_handle = transcript.handle();
        let dock = VStack::new(
            vec![
                entry(tasks.clone(), stack_entry(None, None, 0)),
                entry(queue.clone(), stack_entry(None, None, 0)),
                entry(notices.clone(), stack_entry(None, None, 0)),
                entry(editor_container.clone(), stack_entry(None, None, 3)),
                entry(footer.clone(), stack_entry(None, None, 0)),
            ],
            StackOptions::default(),
        );
        for child in [&chat, &tasks, &queue, &notices, &editor_container, &footer] {
            tui.add_child(child.clone());
        }
        let top = stack_entry(Some(StackBasis::Fixed(0)), Some(1), 1);
        let bottom = stack_entry(Some(StackBasis::Auto), Some(0), 1);
        let root = VStack::new(
            vec![entry(component(transcript), top), entry(component(dock), bottom)],
            StackOptions::default(),
        );
        tui.set_layout_root(Some(component(root)));
        tui.set_focus(Some(editor_component.clone()));
        Rc::new(Self {
            tui,
            chat,
            tasks,
            queue,
            notices,
            footer_stats,
            footer_hints,
            editor_container,
            editor,
            editor_component,
            transcript: transcript_handle,
            cwd,
            state: RefCell::default(),
        })
    }

    pub fn stop(&self) {
        self.discard_tools();
        if let Some(indicator) = self.state.borrow_mut().indicator.take() {
            indicator.borrow_mut().dispose();
        }
        self.tui.stop(pi_tui::TuiStopOptions::default());
    }

    /// Finish every card: a running bash card keeps a timer until it gets a final result.
    fn discard_tools(&self) {
        let mut state = self.state.borrow_mut();
        for card in state.cards.drain(..) {
            card.borrow_mut().update_result(text_result(String::new(), None, false), false);
        }
        state.tools.clear();
        state.streaming_calls.clear();
    }

    pub fn mount(&self, child: ComponentRef) {
        let mut container = self.editor_container.borrow_mut();
        container.clear();
        container.add_child(child.clone());
        drop(container);
        self.tui.set_focus(Some(child));
        self.tui.request_render(false);
    }

    pub fn restore_editor(&self) {
        self.mount(self.editor_component.clone());
    }

    pub fn toggle_expanded(&self) {
        let mut state = self.state.borrow_mut();
        state.expanded = !state.expanded;
        let expanded = state.expanded;
        for card in &state.cards {
            card.borrow_mut().set_expanded(expanded);
        }
        for summary in &state.summaries {
            summary.set_expanded(expanded);
        }
        drop(state);
        self.tui.request_render(false);
    }

    pub fn apply(&self, view: &DurableView) {
        let conversation = &view.conversation;
        let live = &conversation["docs"]["pi.live"];
        let entries: Vec<EntryRecord> = serde_json::from_value(conversation["entries"].clone()).unwrap_or_default();
        self.sync_transcript(&entries);
        let message: Option<AssistantMessage> = serde_json::from_value(live["generation"]["message"].clone()).ok();
        // A partial without its entry was dropped, for example by a retry: render the transcript again.
        let streaming = self.state.borrow().streaming.is_some();
        match message {
            None if streaming => self.rebuild(&entries),
            Some(message) => self.sync_streaming(&message),
            None => {}
        }
        for slot in live["tools"].as_array().into_iter().flatten() {
            self.sync_tool_slot(slot);
        }
        self.sync_tasks(view.tasks.as_ref());
        self.sync_queue(&conversation["docs"]["pi.inbox"]);
        self.sync_notices(view);
        let level = agent_of(conversation).thinking_level.unwrap_or(ThinkingLevel::Off);
        let border = theme().get_thinking_border_color(level);
        self.editor.set_border_color(Rc::new(move |text: &str| border(text)));
        self.sync_status(live);
        self.sync_footer(view, &entries);
        let rebuilt = std::mem::take(&mut self.state.borrow_mut().rebuilt);
        if rebuilt {
            self.transcript.scroll_to_end();
        }
        self.tui.request_render(rebuilt);
    }

    fn sync_tool_slot(&self, slot: &Value) {
        let status = slot["status"].as_str().unwrap_or_default();
        if status == "pending" {
            return;
        }
        let (name, call_id) = (slot["name"].as_str().unwrap_or_default(), slot["callId"].as_str().unwrap_or_default());
        let card = self.tool(name, call_id, None, false);
        let mut card = card.borrow_mut();
        card.set_args_complete();
        if status != "running" {
            return;
        }
        card.mark_execution_started();
        let details = Some(slot["details"].clone()).filter(|details| !details.is_null());
        match (slot["output"].as_str(), slot["details"]["conversationId"].as_u64()) {
            (None, Some(child)) => {
                let text = format!("Subagent {child} is working. /agents switches to it.");
                card.update_result(text_result(text, details, false), true);
            }
            (Some(output), _) => card.update_result(text_result(output.to_string(), details, false), true),
            (None, None) => {}
        }
    }

    fn sync_transcript(&self, entries: &[EntryRecord]) {
        // Compaction and resets replace the head of the active transcript.
        let diverged = {
            let rendered = &self.state.borrow().rendered_entry_ids;
            rendered.iter().enumerate().any(|(index, id)| entries.get(index).map(|e| e.id) != Some(*id))
        };
        if diverged {
            self.rebuild(entries);
        }
        let start = self.state.borrow().rendered_entry_ids.len();
        for record in entries.iter().skip(start) {
            self.add_entry(record);
            self.state.borrow_mut().rendered_entry_ids.push(record.id);
        }
    }

    fn rebuild(&self, entries: &[EntryRecord]) {
        self.chat.borrow_mut().clear();
        self.discard_tools();
        {
            let mut state = self.state.borrow_mut();
            state.summaries.clear();
            state.rebuilt = true;
            state.rendered_entry_ids.clear();
            state.streaming = None;
        }
        for record in entries {
            self.add_entry(record);
            self.state.borrow_mut().rendered_entry_ids.push(record.id);
        }
    }

    fn add_entry(&self, record: &EntryRecord) {
        let message = record.model.as_ref().and_then(|model| model.first());
        let mut chat = self.chat.borrow_mut();
        match (record.kind.as_str(), message) {
            ("pi.user", Some(Message::User(user))) => {
                chat.add_child(component(Spacer::new(1)));
                let text = user_text(&user.content);
                chat.add_child(component(UserMessageComponent::new(&text, &RenderOptions::default())));
            }
            ("pi.assistant", Some(Message::Assistant(answer))) => {
                drop(chat);
                self.add_answer(answer);
            }
            ("pi.tool-result", Some(Message::ToolResult(result))) => {
                drop(chat);
                let view = ToolResultView {
                    content: result.content.clone(),
                    details: result.details.clone(),
                    is_error: result.is_error,
                    duration_ms: None,
                };
                self.tool(&result.tool_name, &result.tool_call_id, None, false).borrow_mut().update_result(view, false);
            }
            ("pi.compaction", message) => {
                let summary = match message {
                    Some(Message::User(user)) => user_text(&user.content),
                    _ => String::new(),
                };
                let card = CompactionCard::new(summary, self.state.borrow().expanded);
                chat.add_child(component(Spacer::new(1)));
                chat.add_child(card.component.clone());
                self.state.borrow_mut().summaries.push(card);
            }
            ("pi.reset", _) => {
                chat.add_child(component(Spacer::new(1)));
                chat.add_child(component(Text::new(&fg(ThemeColor::Muted, "[new context]"), 1, 0)));
            }
            _ => {}
        }
    }

    fn new_answer_card(&self) -> Rc<RefCell<AssistantMessageComponent>> {
        let card = component(AssistantMessageComponent::new(None, false, "Thinking...", RenderOptions::default()));
        self.chat.borrow_mut().add_child(card.clone());
        card
    }

    fn add_answer(&self, answer: &AssistantMessage) {
        let streaming = self.state.borrow_mut().streaming.take();
        let card = streaming.unwrap_or_else(|| self.new_answer_card());
        card.borrow_mut().update_content(answer, false);
        // Only a tool-calling answer runs its calls; an aborted, failed, or truncated one never does.
        let ran = answer.stop_reason == pi_ai::StopReason::ToolUse;
        for block in &answer.content {
            let ContentBlock::ToolCall(call) = block else { continue };
            let streamed = self.state.borrow().streaming_calls.contains(&call.id);
            // Only cards the stream already showed are kept for calls that never run.
            if !ran && !streamed {
                continue;
            }
            let args = serde_json::to_value(&call.arguments).unwrap_or_default();
            let card = self.tool(&call.name, &call.id, Some(args), !streamed);
            let mut card = card.borrow_mut();
            card.set_args_complete();
            if !ran {
                card.update_result(text_result("Not run: the answer was interrupted.".into(), None, true), false);
            }
        }
        self.state.borrow_mut().streaming_calls.clear();
    }

    fn sync_streaming(&self, message: &AssistantMessage) {
        let existing = self.state.borrow().streaming.clone();
        let card = existing.unwrap_or_else(|| {
            let card = self.new_answer_card();
            self.state.borrow_mut().streaming = Some(card.clone());
            card
        });
        card.borrow_mut().update_content(message, true);
        for block in &message.content {
            let ContentBlock::ToolCall(call) = block else { continue };
            let fresh = !self.state.borrow().streaming_calls.contains(&call.id);
            let args = serde_json::to_value(&call.arguments).unwrap_or_default();
            self.tool(&call.name, &call.id, Some(args), fresh);
            self.state.borrow_mut().streaming_calls.insert(call.id.clone());
        }
    }

    /// The card of a call; `fresh` starts a new one for a call ID an earlier turn used.
    fn tool(&self, name: &str, call_id: &str, args: Option<Value>, fresh: bool) -> Card {
        let existing = (!fresh).then(|| self.state.borrow().tools.get(call_id).cloned()).flatten();
        if let Some(existing) = existing {
            if let Some(args) = args {
                existing.borrow_mut().update_args(args);
            }
            return existing;
        }
        let display = builtin_renderer(name).map_or(ToolDisplay::Generic, ToolDisplay::Renderer);
        let (args, options) = (args.unwrap_or(json!({})), ToolExecutionOptions::default());
        let mut card = ToolExecutionComponent::new(name, call_id, args, options, display, &self.tui, self.cwd.clone());
        let mut state = self.state.borrow_mut();
        card.set_expanded(state.expanded);
        let card = component(card);
        self.chat.borrow_mut().add_child(card.clone());
        state.cards.push(card.clone());
        state.tools.insert(call_id.to_string(), card.clone());
        card
    }
}
