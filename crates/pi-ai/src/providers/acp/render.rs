//! `session/update` notifications as pi transcript text: message and thought chunks, tool calls
//! with their status, diffs, and terminal output, and the plan.

use std::collections::HashMap;

use serde_json::Value;

use super::host::PlanEntry;
use super::terminal::Terminals;
use crate::stream::MessageBuilder;

const MAX_LINES: usize = 60;

#[derive(Default)]
struct ToolState {
    title: String,
    kind: String,
    status: String,
    content: Vec<Value>,
}

/// What one prompt turn has rendered so far.
#[derive(Default)]
pub(crate) struct Transcript {
    tools: HashMap<String, ToolState>,
    pub plan: Vec<PlanEntry>,
    plan_rendered: Vec<PlanEntry>,
    pub context_tokens: Option<u64>,
    pub cost_usd: Option<f64>,
}

fn fenced(language: &str, text: &str) -> String {
    let fence = if text.contains("```") { "````" } else { "```" };
    let lines: Vec<&str> = text.lines().collect();
    let shown = lines.len().min(MAX_LINES);
    let mut body = lines[..shown].join("\n");
    if lines.len() > shown {
        body.push_str(&format!("\n... {} more lines", lines.len() - shown));
    }
    format!("{fence}{language}\n{body}\n{fence}\n")
}

/// A unified diff of the changed lines with two lines of context.
pub(crate) fn diff(path: &str, old: Option<&str>, new: &str) -> String {
    let old_lines: Vec<&str> = old.unwrap_or_default().lines().collect();
    let new_lines: Vec<&str> = new.lines().collect();
    let prefix = old_lines.iter().zip(&new_lines).take_while(|(a, b)| a == b).count();
    let max_suffix = old_lines.len().min(new_lines.len()) - prefix;
    let suffix =
        old_lines.iter().rev().zip(new_lines.iter().rev()).take(max_suffix).take_while(|(a, b)| a == b).count();
    let start = prefix.saturating_sub(2);
    let mut out = vec![format!("--- {}", if old.is_some() { path } else { "/dev/null" }), format!("+++ {path}")];
    out.extend(old_lines[start..prefix].iter().map(|line| format!(" {line}")));
    out.extend(old_lines[prefix..old_lines.len() - suffix].iter().map(|line| format!("-{line}")));
    out.extend(new_lines[prefix..new_lines.len() - suffix].iter().map(|line| format!("+{line}")));
    let context_end = (new_lines.len() - suffix + 2).min(new_lines.len());
    out.extend(new_lines[new_lines.len() - suffix..context_end].iter().map(|line| format!(" {line}")));
    out.join("\n")
}

fn content_text(content: &Value) -> Option<String> {
    match content["type"].as_str()? {
        "text" => content["text"].as_str().map(str::to_string),
        "resource_link" => content["uri"].as_str().map(str::to_string),
        "resource" => content.pointer("/resource/text").and_then(Value::as_str).map(str::to_string),
        "image" => Some("[image]".into()),
        _ => None,
    }
}

impl Transcript {
    pub fn update(&mut self, out: &mut MessageBuilder, update: &Value, terminals: &Terminals) {
        let text = || update.pointer("/content").and_then(content_text).unwrap_or_default();
        match update["sessionUpdate"].as_str().unwrap_or_default() {
            "agent_message_chunk" => out.text_delta(&text()),
            "agent_thought_chunk" => out.thinking_delta(&text()),
            "tool_call" | "tool_call_update" => self.tool(out, update, terminals),
            "plan" => {
                let entries = update["entries"].as_array().into_iter().flatten();
                self.plan = entries
                    .map(|entry| PlanEntry {
                        content: entry["content"].as_str().unwrap_or_default().to_string(),
                        status: entry["status"].as_str().unwrap_or("pending").to_string(),
                    })
                    .collect();
            }
            "usage_update" => {
                self.context_tokens = update["used"].as_u64().filter(|used| *used > 0).or(self.context_tokens);
                if update.pointer("/cost/currency").and_then(Value::as_str) == Some("USD") {
                    self.cost_usd = update.pointer("/cost/amount").and_then(Value::as_f64);
                }
            }
            _ => {}
        }
    }

    fn tool(&mut self, out: &mut MessageBuilder, update: &Value, terminals: &Terminals) {
        let id = update["toolCallId"].as_str().unwrap_or_default().to_string();
        let known = self.tools.contains_key(&id);
        let state = self.tools.entry(id).or_default();
        let text = |key: &str| update[key].as_str().map(str::to_string);
        state.title = text("title").unwrap_or(std::mem::take(&mut state.title));
        state.kind = text("kind").unwrap_or(std::mem::take(&mut state.kind));
        let status = text("status").unwrap_or_else(|| state.status.clone());
        if let Some(content) = update["content"].as_array() {
            state.content = content.clone();
        }
        let title = if state.title.is_empty() { "tool call".to_string() } else { state.title.clone() };
        let kind = if state.kind.is_empty() { "other".to_string() } else { state.kind.clone() };
        let finished = matches!(status.as_str(), "completed" | "failed");
        let was_finished = matches!(state.status.as_str(), "completed" | "failed");
        state.status = status.clone();
        if !known {
            out.text_delta(&format!("\n\n> `{kind}` {title}\n"));
        }
        if !finished || was_finished {
            return;
        }
        let mut lines = format!("\n> `{kind}` {title}: **{status}**\n\n");
        for item in &state.content {
            match item["type"].as_str() {
                Some("diff") => {
                    let path = item["path"].as_str().unwrap_or_default();
                    let new = item["newText"].as_str().unwrap_or_default();
                    lines.push_str(&fenced("diff", &diff(path, item["oldText"].as_str(), new)));
                }
                Some("terminal") => {
                    let output = item["terminalId"].as_str().and_then(|id| terminals.text(id));
                    if let Some((output, _)) = output.filter(|(output, _)| !output.trim().is_empty()) {
                        lines.push_str(&fenced("", output.trim_end()));
                    }
                }
                Some("content") => {
                    if let Some(text) = content_text(&item["content"]).filter(|text| !text.trim().is_empty()) {
                        lines.push_str(&fenced("", text.trim_end()));
                    }
                }
                _ => {}
            }
        }
        out.text_delta(&lines);
    }

    /// The plan as a checklist when it changed since the last call.
    pub fn flush_plan(&mut self, out: &mut MessageBuilder) {
        if self.plan.is_empty() || self.plan == self.plan_rendered {
            return;
        }
        let mut text = String::from("\n\n**Plan**\n\n");
        for entry in &self.plan {
            let mark = match entry.status.as_str() {
                "completed" => "[x]",
                "in_progress" => "[~]",
                _ => "[ ]",
            };
            text.push_str(&format!("- {mark} {}\n", entry.content));
        }
        out.text_delta(&text);
        self.plan_rendered = self.plan.clone();
    }
}

#[cfg(test)]
mod tests {
    use super::diff;

    #[test]
    fn diffs_show_changed_lines_with_context() {
        let old = "a\nb\nc\nd\ne\nf\n";
        let new = "a\nb\nc\nD\ne\nf\n";
        assert_eq!(diff("x.rs", Some(old), new), "--- x.rs\n+++ x.rs\n b\n c\n-d\n+D\n e\n f");
        assert_eq!(diff("n.rs", None, "hi\n"), "--- /dev/null\n+++ n.rs\n+hi");
    }
}
