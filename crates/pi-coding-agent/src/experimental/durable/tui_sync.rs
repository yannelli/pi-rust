//! The dock of upstream `durable/tui.ts`: the task panel, the queue, notices, the status line, and the footer.

use std::collections::HashSet;

use pi_ai::ThinkingLevel;
use pi_durable::EntryRecord;
use pi_tui::component;
use pi_tui::components::{StyleFn, TruncatedText};
use serde_json::{Value, json};

use super::runtime::{DurableView, NoticeLevel, agent_of, conversation_id_of};
use super::tui::{DurableTui, fg};
use super::tui_parts::{context_tokens, describe_task, total_usage, user_text};
use crate::interactive::components::status_indicator::StatusIndicator;
use crate::interactive::util::format_tokens;
use crate::interactive_components::keybinding_hints::key_text;
use crate::theme::ThemeColor;

fn muted_line(text: &str) -> std::rc::Rc<std::cell::RefCell<TruncatedText>> {
    component(TruncatedText::new(&fg(ThemeColor::Muted, text), 1, 0))
}

fn visit(node: &Value, depth: usize, nodes: &[&Value], lines: &mut Vec<String>) {
    lines.push(format!("{}{}", "  ".repeat(depth + 1), describe_task(node)));
    let conversations = node["conversations"].as_array().cloned().unwrap_or_default();
    for child in nodes {
        let owner = &child["owner"];
        if *owner == node["id"] || (owner.is_null() && conversations.contains(&child["conversationId"])) {
            visit(child, depth + 1, nodes, lines);
        }
    }
}

impl DurableTui {
    pub(super) fn sync_tasks(&self, graph: Option<&Value>) {
        let mut tasks = self.tasks.borrow_mut();
        tasks.clear();
        let Some(graph) = graph else { return };
        let nodes: Vec<&Value> = graph["tasks"].as_object().into_iter().flat_map(|map| map.values()).collect();
        let mut lines = vec![fg(ThemeColor::Accent, &format!("Tasks ({} live, /tasks to hide)", nodes.len()))];
        // A conversation-owned task sits under the task that owns its conversation, when that task is live.
        let owned: HashSet<String> = nodes
            .iter()
            .flat_map(|node| node["conversations"].as_array().into_iter().flatten())
            .map(Value::to_string)
            .collect();
        for node in &nodes {
            if node["owner"].is_null() && !owned.contains(&node["conversationId"].to_string()) {
                visit(node, 0, &nodes, &mut lines);
            }
        }
        for line in lines {
            tasks.add_child(muted_line(&line));
        }
    }

    pub(super) fn sync_queue(&self, inbox: &Value) {
        let mut queue = self.queue.borrow_mut();
        queue.clear();
        for item in inbox["items"].as_array().into_iter().flatten() {
            let mode = item["mode"].as_str().unwrap_or_default();
            let text = match mode {
                "write" => format!("<{}>", item["entry"]["kind"].as_str().unwrap_or_default()),
                _ => serde_json::from_value(item["content"].clone()).map(|c| user_text(&c)).unwrap_or_default(),
            };
            queue.add_child(muted_line(&format!("[{mode}] {text}")));
        }
    }

    pub(super) fn sync_notices(&self, view: &DurableView) {
        let mut notices = self.notices.borrow_mut();
        notices.clear();
        for notice in &view.notices[view.notices.len().saturating_sub(4)..] {
            let color = match notice.level {
                NoticeLevel::Error => ThemeColor::Error,
                NoticeLevel::Warning => ThemeColor::Warning,
                NoticeLevel::Info => ThemeColor::Muted,
            };
            notices.add_child(component(TruncatedText::new(&fg(color, &notice.message), 1, 0)));
        }
    }

    pub(super) fn sync_status(&self, live: &Value) {
        let (generation, compaction) = (&live["generation"], &live["compactions"][0]);
        let running = live["tools"].as_array().into_iter().flatten().find(|slot| slot["status"] == "running");
        let attempt = |value: &Value| value["attempt"].as_u64().unwrap_or_default() + 1;
        let text = if !generation["retry"].is_null() {
            let error = generation["retry"]["error"].as_str().unwrap_or_default();
            format!("Retrying (attempt {}): {error}", attempt(generation)) // concise-ignore
        } else if !generation["deferred"].is_null() {
            "Waiting for deferred response...".to_string()
        } else if !compaction.is_null() {
            let reason = compaction["trigger"].as_str().unwrap_or_default();
            match compaction["retry"].as_bool() == Some(true) {
                true => format!("Retrying {reason} compaction (attempt {})...", attempt(compaction)), // concise-ignore
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
        let color: StyleFn = self.editor.border_color();
        state.indicator = (!text.is_empty()).then(|| StatusIndicator::working(&self.tui, &text, None, Some(color)));
        self.editor.set_working_status_indicator(state.indicator.clone());
    }

    pub(super) fn sync_footer(&self, view: &DurableView, entries: &[EntryRecord]) {
        let agent = agent_of(&view.conversation);
        let usage = total_usage(&view.conversation["docs"]["pi.usage"]);
        let mut stats = Vec::new();
        let counts = [("↑", usage.input), ("↓", usage.output), ("R", usage.cache_read), ("W", usage.cache_write)];
        for (prefix, count) in counts {
            if count > 0 {
                stats.push(format!("{prefix}{}", format_tokens(count)));
            }
        }
        stats.push(format!("${:.3}", usage.cost));
        let selected = |provider: &str, id: &str| {
            agent.model.as_ref().is_some_and(|model| model.provider == provider && model.model_id == id)
        };
        let models = view.models.iter();
        let window = models.filter(|model| selected(&model.provider, &model.model_id)).map(|m| m.context_window);
        let window = window.max().unwrap_or(0);
        if window > 0 {
            let percent = context_tokens(entries).map(|tokens| tokens as f64 / window as f64 * 100.0);
            let shown = percent.map_or("?".into(), |percent| format!("{percent:.1}"));
            let text = format!("{shown}%/{}", format_tokens(window));
            stats.push(if percent.is_some_and(|percent| percent > 90.0) { fg(ThemeColor::Error, &text) } else { text });
        }
        let line = format!("{}  {}", stats.join(" "), view.session.cwd);
        self.footer_stats.borrow_mut().set_text(&fg(ThemeColor::Dim, &line));
        let model = agent.model.as_ref().map_or("no model".into(), |m| format!("{}/{}", m.provider, m.model_id));
        let shown = conversation_id_of(&view.conversation);
        let label = view
            .conversations
            .iter()
            .find(|summary| Some(summary.id) == shown)
            .map(|summary| summary.label.clone())
            .unwrap_or_else(|| format!("conversation {}", view.conversation["conversation"]["id"]));
        let thinking = serde_json::to_value(agent.thinking_level.unwrap_or(ThinkingLevel::Off)).unwrap_or(json!("off"));
        let hints = format!(
            " · {model} · thinking:{} ({}) · {} or /model · /agents · /compact · /tasks · {} follow-up · {} exit",
            thinking.as_str().unwrap_or("off"),
            key_text("app.thinking.cycle"),
            key_text("app.model.select"),
            key_text("app.message.followUp"),
            key_text("app.clear"),
        );
        let color = if label == "main" { ThemeColor::Dim } else { ThemeColor::Accent };
        self.footer_hints.borrow_mut().set_text(&format!("{}{}", fg(color, &label), fg(ThemeColor::Dim, &hints)));
    }
}
