//! Tree selector tests: a port of upstream `tree-selector.test.ts`, terminal-driven tests, and
//! row text and branch summary prompt checks.

mod details;
mod terminal;
mod upstream;

use std::collections::HashMap;

use serde_json::{Value, json};

use crate::interactive_components::tree_selector::{TreeData, TreeNode, TreeSelector};
use crate::session::{Entry, iso_timestamp};

fn base(kind: &str, id: &str, parent_id: Option<&str>) -> serde_json::Map<String, Value> {
    let value = json!({"type": kind, "id": id, "parentId": parent_id, "timestamp": iso_timestamp()});
    value.as_object().cloned().unwrap_or_default()
}

fn with(kind: &str, id: &str, parent_id: Option<&str>, fields: Value) -> Entry {
    let mut object = base(kind, id, parent_id);
    object.extend(fields.as_object().cloned().unwrap_or_default());
    Entry::from_raw(object)
}

fn usage() -> Value {
    json!({"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 0,
        "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0}})
}

fn user_message(id: &str, parent_id: Option<&str>, content: &str) -> Entry {
    with("message", id, parent_id, json!({"message": {"role": "user", "content": content, "timestamp": 1}}))
}

fn assistant(id: &str, parent_id: Option<&str>, content: Value, stop_reason: &str) -> Entry {
    let message = json!({"role": "assistant", "content": content, "api": "anthropic-messages",
        "provider": "anthropic", "model": "claude-sonnet-4", "usage": usage(), "stopReason": stop_reason,
        "timestamp": 1});
    with("message", id, parent_id, json!({ "message": message }))
}

fn assistant_message(id: &str, parent_id: Option<&str>, text: &str) -> Entry {
    assistant(id, parent_id, json!([{"type": "text", "text": text}]), "stop")
}

/// A tool-call-only assistant message (hidden in the default view).
fn tool_call_only_assistant(id: &str, parent_id: Option<&str>) -> Entry {
    let call =
        json!([{"type": "toolCall", "id": format!("tc-{id}"), "name": "read", "arguments": {"path": "test.ts"}}]);
    assistant(id, parent_id, call, "toolUse")
}

fn tool_result(id: &str, parent_id: Option<&str>, call_id: &str, tool_name: &str) -> Entry {
    let message = json!({"role": "toolResult", "toolCallId": call_id, "toolName": tool_name,
        "content": [{"type": "text", "text": "ok"}], "isError": false, "timestamp": 1});
    with("message", id, parent_id, json!({ "message": message }))
}

fn model_change(id: &str, parent_id: Option<&str>) -> Entry {
    with("model_change", id, parent_id, json!({"provider": "anthropic", "modelId": "claude-sonnet-4"}))
}

/// Builds a tree like upstream's test helper: children in entry order, roots have no parent,
/// and entries with a missing parent are dropped.
fn build_tree(entries: &[Entry]) -> TreeData {
    let index: HashMap<&str, usize> = entries.iter().enumerate().map(|(i, entry)| (entry.id(), i)).collect();
    let mut nodes: Vec<TreeNode> = entries
        .iter()
        .map(|entry| TreeNode { entry: entry.clone(), children: Vec::new(), label: None, label_timestamp: None })
        .collect();
    let mut roots = Vec::new();
    for (position, entry) in entries.iter().enumerate() {
        match entry.parent_id() {
            None => roots.push(position),
            Some(parent) => {
                if let Some(&parent) = index.get(parent) {
                    nodes[parent].children.push(position);
                }
            }
        }
    }
    TreeData { nodes, roots }
}

/// A selector with no-op callbacks and a 24-row terminal.
fn selector(tree: &TreeData, leaf: &str) -> TreeSelector {
    TreeSelector::new(tree.clone(), Some(leaf), 24, |_: &str| {}, || {}, None, None, None)
}

fn selected_id(selector: &TreeSelector) -> Option<String> {
    selector.tree_list().get_selected_node().map(|node| node.entry.id().to_string())
}

/// Upstream's folding tree: user-1 to asst-2 in a chain, then branch A (user-3a to asst-4a,
/// active) and branch B (user-3b to user-4b).
fn branching_tree() -> TreeData {
    build_tree(&[
        user_message("user-1", None, "first message"),
        assistant_message("asst-1", Some("user-1"), "response 1"),
        user_message("user-2", Some("asst-1"), "second message"),
        assistant_message("asst-2", Some("user-2"), "response 2"),
        user_message("user-3a", Some("asst-2"), "branch A start"),
        assistant_message("asst-3a", Some("user-3a"), "branch A response"),
        user_message("user-4a", Some("asst-3a"), "branch A deep"),
        assistant_message("asst-4a", Some("user-4a"), "branch A leaf"),
        user_message("user-3b", Some("asst-2"), "branch B start"),
        assistant_message("asst-3b", Some("user-3b"), "branch B response"),
        user_message("user-4b", Some("asst-3b"), "branch B deep"),
    ])
}
