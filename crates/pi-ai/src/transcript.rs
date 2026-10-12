//! System message replay: the leading system message is the prompt, later ones patch it.
//! Port of upstream `utils/transcript.ts` and the system message text helpers.

use std::collections::BTreeMap;

use serde_json::{Map, Value};

use crate::types::{Context, Message, SystemMessage, Tool, ToolReference, UserContent};

/// The leading system message for a prompt and tool set, or `None` when both are empty.
pub fn create_initial_system_message(system_prompt: Option<&str>, tools: &[Tool]) -> Option<SystemMessage> {
    let prompt = system_prompt.unwrap_or_default();
    if prompt.is_empty() && tools.is_empty() {
        return None;
    }
    Some(SystemMessage {
        content: UserContent::Text(prompt.to_string()),
        sections: None,
        tools_added: (!tools.is_empty()).then(|| tools.to_vec()),
        tools_removed: None,
        timestamp: 0,
    })
}

/// Fold `system_prompt` and `tools` into a leading system message.
pub fn normalize_context(context: &Context) -> Vec<Message> {
    let initial = create_initial_system_message(context.system_prompt.as_deref(), &context.tools);
    initial.map(Message::System).into_iter().chain(context.messages.iter().cloned()).collect()
}

fn system_messages(messages: &[Message]) -> impl Iterator<Item = &SystemMessage> {
    messages.iter().filter_map(|message| match message {
        Message::System(system) => Some(system),
        _ => None,
    })
}

/// The leading system message, if the transcript starts with one.
pub fn initial_system_message(messages: &[Message]) -> Option<&SystemMessage> {
    match messages.first() {
        Some(Message::System(system)) => Some(system),
        _ => None,
    }
}

/// Tools available after applying every system message in order.
pub fn current_tools(messages: &[Message]) -> Vec<Tool> {
    let mut tools: Vec<Tool> = Vec::new();
    for system in system_messages(messages) {
        for removed in system.tools_removed.iter().flatten() {
            tools.retain(|tool| tool.name != removed.name);
        }
        for added in system.tools_added.iter().flatten() {
            match tools.iter_mut().find(|tool| tool.name == added.name) {
                Some(existing) => *existing = added.clone(),
                None => tools.push(added.clone()),
            }
        }
    }
    tools
}

/// Replay every system message into one holding the current prompt, sections, and tools.
pub fn current_system_message(messages: &[Message]) -> Option<SystemMessage> {
    let mut content: Vec<String> = Vec::new();
    let mut sections = Map::new();
    let mut timestamp = None;
    for system in system_messages(messages) {
        timestamp.get_or_insert(system.timestamp);
        let text = system.content.text();
        if !text.is_empty() {
            content.push(text);
        }
        for (name, value) in system.sections.iter().flatten() {
            match value {
                Value::Null => {
                    sections.shift_remove(name);
                }
                value => {
                    sections.insert(name.clone(), value.clone());
                }
            }
        }
    }
    let tools = current_tools(messages);
    if timestamp.is_none() && tools.is_empty() {
        return None;
    }
    Some(SystemMessage {
        content: UserContent::Text(content.join("\n\n")),
        sections: (!sections.is_empty()).then_some(sections),
        tools_added: (!tools.is_empty()).then_some(tools),
        tools_removed: None,
        timestamp: timestamp.unwrap_or_default(),
    })
}

/// A system message as a complete prompt: its content followed by its sections.
pub fn system_message_text(message: &SystemMessage) -> String {
    let mut parts = vec![message.content.text()];
    parts.extend(message.sections.iter().flatten().filter_map(|(_, value)| value.as_str().map(String::from)));
    parts.retain(|part| !part.is_empty());
    parts.join("\n\n")
}

/// A later system message for APIs that accept system messages mid-conversation.
pub fn render_system_message_update(message: &SystemMessage) -> String {
    let mut parts = Vec::new();
    let text = message.content.text();
    if !text.is_empty() {
        parts.push(text);
    }
    for (name, value) in message.sections.iter().flatten() {
        parts.push(match value.as_str() {
            Some(value) => format!("Updated system prompt section \"{name}\":\n\n{value}"),
            None => format!("Removed system prompt section \"{name}\"."),
        });
    }
    parts.join("\n\n")
}

/// The current system prompt text after replaying every system message.
pub fn current_system_prompt(messages: &[Message]) -> String {
    current_system_message(messages).map(|message| system_message_text(&message)).unwrap_or_default()
}

/// Rebuild for APIs without mid-conversation system messages: the replayed one leads.
pub fn collapse_system_messages(messages: &[Message]) -> Vec<Message> {
    let head = current_system_message(messages).map(Message::System);
    head.into_iter().chain(messages.iter().filter(|message| !matches!(message, Message::System(_))).cloned()).collect()
}

/// A provider request context with every system message folded into `system_prompt` and `tools`.
pub fn flatten_context(context: &Context) -> Context {
    let messages = normalize_context(context);
    if !messages.iter().any(|message| matches!(message, Message::System(_))) {
        return context.clone();
    }
    Context {
        system_prompt: Some(current_system_prompt(&messages)).filter(|prompt| !prompt.is_empty()),
        tools: current_tools(&messages),
        messages: messages.into_iter().filter(|message| !matches!(message, Message::System(_))).collect(),
    }
}

/// Added and removed tools between two complete tool states.
pub fn tool_state_changes(previous: &[Tool], current: &[Tool]) -> (Vec<Tool>, Vec<ToolReference>) {
    let by_name =
        |tools: &[Tool]| tools.iter().map(|tool| (tool.name.clone(), tool.clone())).collect::<BTreeMap<_, _>>();
    let (before, after) = (by_name(previous), by_name(current));
    let added = current.iter().filter(|tool| before.get(&tool.name) != Some(*tool)).cloned().collect();
    let removed = previous
        .iter()
        .filter(|tool| after.get(&tool.name) != Some(*tool))
        .map(|tool| ToolReference { name: tool.name.clone() })
        .collect();
    (added, removed)
}

/// Whether tool history has a removal or redeclaration an addition-only transport cannot replay.
pub fn has_non_additive_tool_changes(messages: &[Message]) -> bool {
    let mut declared = std::collections::HashSet::new();
    for system in system_messages(messages) {
        if system.tools_removed.as_ref().is_some_and(|removed| !removed.is_empty()) {
            return true;
        }
        for tool in system.tools_added.iter().flatten() {
            if !declared.insert(tool.name.clone()) {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tool(name: &str) -> Tool {
        Tool::new(name, "d", json!({"type": "object"}))
    }

    fn system(value: Value) -> Message {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn replays_sections_content_and_tools() {
        let messages = vec![
            system(json!({"role": "system", "content": "base", "sections": {"skills": "<skills/>", "env": "cwd"},
                "toolsAdded": [{"name": "read", "description": "d", "parameters": {}}], "timestamp": 1})),
            Message::user("hi"),
            system(json!({"role": "system", "content": "more", "sections": {"skills": null},
                "toolsAdded": [{"name": "bash", "description": "d", "parameters": {}}],
                "toolsRemoved": [{"name": "read"}], "timestamp": 2})),
        ];
        assert_eq!(current_system_prompt(&messages), "base\n\nmore\n\ncwd");
        let names: Vec<String> = current_tools(&messages).into_iter().map(|tool| tool.name).collect();
        assert_eq!(names, ["bash"]);
        let Message::System(update) = &messages[2] else { unreachable!() };
        assert_eq!(render_system_message_update(update), "more\n\nRemoved system prompt section \"skills\".");
        assert!(has_non_additive_tool_changes(&messages));
        let collapsed = collapse_system_messages(&messages);
        assert_eq!(collapsed.len(), 2);
        let flat = flatten_context(&Context { messages, ..Context::default() });
        assert_eq!(flat.system_prompt.as_deref(), Some("base\n\nmore\n\ncwd"));
        assert_eq!(flat.messages.len(), 1);
    }

    #[test]
    fn plain_contexts_pass_through() {
        let context =
            Context { system_prompt: Some("p".into()), messages: vec![Message::user("x")], tools: vec![tool("read")] };
        assert_eq!(flatten_context(&context), context);
        let (added, removed) = tool_state_changes(&[tool("read"), tool("ls")], &[tool("read"), tool("grep")]);
        assert_eq!((added[0].name.as_str(), removed[0].name.as_str()), ("grep", "ls"));
    }
}
