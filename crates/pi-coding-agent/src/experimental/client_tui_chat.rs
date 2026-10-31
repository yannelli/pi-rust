//! Port of upstream `experimental/client-tui-chat.ts`: renders the root conversation's durable view for the
//! service-only experimental presentation.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;

use pi_ai::{AssistantMessage, ContentBlock, Message, StopReason};
use pi_durable::{EntryId, EntryRecord};
use pi_tui::components::{Spacer, Text, TruncatedText};
use pi_tui::{Container, Tui, component};
use serde_json::{Value, json};

use super::durable::tui_parts::user_text;
use crate::interactive::components::RenderOptions;
use crate::interactive::components::messages::{AssistantMessageComponent, UserMessageComponent};
use crate::interactive::components::status_indicator::{SharedIndicator, StatusIndicator};
use crate::interactive::components::tool_execution::{ToolDisplay, ToolExecutionComponent, ToolExecutionOptions};
use crate::interactive::renderers::{ToolResultView, builtin_renderer};
use crate::theme::{ThemeColor, theme};

type Card = Rc<RefCell<ToolExecutionComponent>>;

/// The `pi.live` document of a view: the active run, the streaming answer, and running tools.
pub fn live_of(view: &Value) -> &Value {
    &view["docs"]["pi.live"]
}

fn text_result(text: String, details: Option<Value>, is_error: bool) -> ToolResultView {
    let content = vec![ContentBlock::Text { text, text_signature: None }];
    ToolResultView { content, details, is_error, duration_ms: None }
}

fn muted(text: &str) -> String {
    theme().fg(ThemeColor::Muted, text)
}

#[derive(Default)]
struct State {
    /// The newest card per call ID; provider call IDs may repeat across turns.
    tools: HashMap<String, Card>,
    /// Every card shown, also older ones whose call ID a later turn reused.
    cards: Vec<Card>,
    /// Call IDs whose cards the streaming answer created; its entry takes them over.
    streaming_calls: HashSet<String>,
    rendered_entry_ids: Vec<EntryId>,
    streaming: Option<Rc<RefCell<AssistantMessageComponent>>>,
    indicator: Option<SharedIndicator>,
    status_text: String,
}

pub struct ExperimentalChatView {
    pub transcript: Rc<RefCell<Container>>,
    pub pending_messages: Rc<RefCell<Container>>,
    pub status: Rc<RefCell<Container>>,
    tui: Tui,
    cwd: PathBuf,
    state: RefCell<State>,
}

impl ExperimentalChatView {
    pub fn new(tui: &Tui, cwd: PathBuf) -> Self {
        Self {
            transcript: component(Container::new()),
            pending_messages: component(Container::new()),
            status: component(Container::new()),
            tui: tui.clone(),
            cwd,
            state: RefCell::default(),
        }
    }

    pub fn apply(&self, view: &Value) {
        let live = live_of(view);
        let entries: Vec<EntryRecord> = serde_json::from_value(view["entries"].clone()).unwrap_or_default();
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
            let status = slot["status"].as_str().unwrap_or_default();
            if status == "pending" {
                continue;
            }
            let (name, call_id) =
                (slot["name"].as_str().unwrap_or_default(), slot["callId"].as_str().unwrap_or_default());
            let card = self.tool(name, call_id, None, false);
            let mut card = card.borrow_mut();
            card.set_args_complete();
            if status != "running" {
                continue;
            }
            card.mark_execution_started();
            if let Some(output) = slot["output"].as_str() {
                let details = Some(slot["details"].clone()).filter(|details| !details.is_null());
                card.update_result(text_result(output.to_string(), details, false), true);
            }
        }
        self.sync_queue(&view["docs"]["pi.inbox"]);
        self.sync_status(live);
        self.transcript.borrow_mut().invalidate_children();
        self.pending_messages.borrow_mut().invalidate_children();
        self.status.borrow_mut().invalidate_children();
    }

    pub fn refresh_theme(&self, view: &Value) {
        let indicator = {
            let mut state = self.state.borrow_mut();
            state.status_text.clear();
            state.indicator.take()
        };
        if let Some(indicator) = indicator {
            indicator.borrow_mut().dispose();
        }
        self.status.borrow_mut().clear();
        let entries: Vec<EntryRecord> = serde_json::from_value(view["entries"].clone()).unwrap_or_default();
        self.rebuild(&entries);
        self.apply(view);
    }

    pub fn dispose(&self) {
        self.discard_tools();
        if let Some(indicator) = self.state.borrow_mut().indicator.take() {
            indicator.borrow_mut().dispose();
        }
    }

    /// Finish every card: a running bash card keeps a timer until it gets a final result.
    fn discard_tools(&self) {
        let mut state = self.state.borrow_mut();
        for card in state.cards.drain(..) {
            let empty = ToolResultView { content: Vec::new(), details: None, is_error: false, duration_ms: None };
            card.borrow_mut().update_result(empty, false);
        }
        state.tools.clear();
        state.streaming_calls.clear();
    }

    fn sync_queue(&self, inbox: &Value) {
        let mut pending = self.pending_messages.borrow_mut();
        pending.clear();
        for item in inbox["items"].as_array().into_iter().flatten() {
            let mode = item["mode"].as_str().unwrap_or_default();
            let text = if mode == "write" {
                let kind = &item["entry"]["kind"];
                format!("<{}>", kind.as_str().map_or_else(|| kind.to_string(), str::to_string))
            } else {
                let content =
                    serde_json::from_value(item["content"].clone()).unwrap_or(pi_ai::UserContent::Text(String::new()));
                user_text(&content).split_whitespace().collect::<Vec<_>>().join(" ")
            };
            pending.add_child(component(TruncatedText::new(&muted(&format!("[{mode}] {text}")), 1, 0)));
        }
    }

    fn sync_status(&self, live: &Value) {
        let (generation, compaction) = (&live["generation"], &live["compactions"][0]);
        let running = live["tools"].as_array().into_iter().flatten().find(|slot| slot["status"] == "running");
        let attempt = |value: &Value| value["attempt"].as_u64().unwrap_or_default() + 1;
        let text = if !generation["retry"].is_null() {
            let error = generation["retry"]["error"].as_str().unwrap_or_default();
            format!("Retrying (attempt {}): {error}", attempt(generation))
        } else if !generation["deferred"].is_null() {
            "Waiting for deferred response...".to_string()
        } else if !compaction.is_null() {
            let reason = compaction["trigger"].as_str().unwrap_or_default();
            match compaction["retry"].as_bool() == Some(true) {
                true => format!("Retrying {reason} compaction (attempt {})...", attempt(compaction)),
                false => format!("Compacting ({reason})..."),
            }
        } else if let Some(slot) = running {
            format!("Running {}... (esc to abort)", slot["name"].as_str().unwrap_or_default())
        } else if !live["run"].is_null() {
            "Working... (esc to abort)".to_string()
        } else {
            String::new()
        };
        let mut state = self.state.borrow_mut();
        if text == state.status_text {
            return;
        }
        state.status_text = text.clone();
        if let Some(indicator) = state.indicator.take() {
            indicator.borrow_mut().dispose();
        }
        let mut status = self.status.borrow_mut();
        status.clear();
        if !text.is_empty() {
            let indicator = StatusIndicator::working(&self.tui, &text, None, None);
            status.add_child(indicator.clone());
            state.indicator = Some(indicator);
        }
    }

    fn sync_transcript(&self, entries: &[EntryRecord]) {
        // Compaction and resets replace the head of the active transcript.
        let diverged = {
            let rendered = &self.state.borrow().rendered_entry_ids;
            rendered.iter().enumerate().any(|(index, id)| entries.get(index).map(|entry| entry.id) != Some(*id))
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
        self.transcript.borrow_mut().clear();
        self.discard_tools();
        {
            let mut state = self.state.borrow_mut();
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
        match (record.kind.as_str(), message) {
            ("pi.user", Some(Message::User(user))) => {
                let mut transcript = self.transcript.borrow_mut();
                transcript.add_child(component(Spacer::new(1)));
                let text = user_text(&user.content);
                transcript.add_child(component(UserMessageComponent::new(&text, &RenderOptions::default())));
            }
            ("pi.assistant", Some(Message::Assistant(answer))) => self.add_answer(answer),
            ("pi.tool-result", Some(Message::ToolResult(result))) => {
                let view = ToolResultView {
                    content: result.content.clone(),
                    details: result.details.clone(),
                    is_error: result.is_error,
                    duration_ms: None,
                };
                self.tool(&result.tool_name, &result.tool_call_id, None, false).borrow_mut().update_result(view, false);
            }
            ("pi.compaction", message) => {
                self.add_text(&muted("[compaction]"));
                if let Some(Message::User(user)) = message {
                    self.add_text(&user_text(&user.content));
                }
            }
            ("pi.reset", _) => self.add_text(&muted("[new context]")),
            _ => {}
        }
    }

    fn new_answer_card(&self) -> Rc<RefCell<AssistantMessageComponent>> {
        let card = component(AssistantMessageComponent::new(None, false, "Thinking...", RenderOptions::default()));
        self.transcript.borrow_mut().add_child(card.clone());
        card
    }

    fn add_answer(&self, answer: &AssistantMessage) {
        let streaming = self.state.borrow_mut().streaming.take();
        let card = streaming.unwrap_or_else(|| self.new_answer_card());
        card.borrow_mut().update_content(answer, false);
        // Only a tool-calling answer runs its calls; an aborted, failed, or truncated one never does.
        let ran = answer.stop_reason == StopReason::ToolUse;
        for block in &answer.content {
            let ContentBlock::ToolCall(call) = block else { continue };
            let streamed = self.state.borrow().streaming_calls.contains(&call.id);
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
        let card = ToolExecutionComponent::new(name, call_id, args, options, display, &self.tui, self.cwd.clone());
        let card = component(card);
        self.transcript.borrow_mut().add_child(card.clone());
        let mut state = self.state.borrow_mut();
        state.cards.push(card.clone());
        state.tools.insert(call_id.to_string(), card.clone());
        card
    }

    fn add_text(&self, text: &str) {
        let mut transcript = self.transcript.borrow_mut();
        transcript.add_child(component(Spacer::new(1)));
        transcript.add_child(component(Text::new(text, 1, 0)));
    }
}
