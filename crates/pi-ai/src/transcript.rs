//! System message replay: the leading system message is the prompt, later ones patch it.
//! Port of upstream `utils/transcript.ts` and the system message text helpers.

use std::collections::{BTreeMap, HashSet};

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
    tools_after(system_messages(messages))
}

fn tools_after<'a>(systems: impl Iterator<Item = &'a SystemMessage>) -> Vec<Tool> {
    let mut tools: Vec<Tool> = Vec::new();
    for system in systems {
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

/// Keep later system messages in place when the API accepts them; otherwise collapse them.
pub fn resolve_transcript(messages: Vec<Message>, supports_mid_convo_system_messages: bool) -> Vec<Message> {
    if supports_mid_convo_system_messages { messages } else { collapse_system_messages(&messages) }
}

/// Drop the leading system message for APIs that carry the prompt outside the message list.
pub fn without_initial_system_message(messages: &[Message]) -> &[Message] {
    match messages.first() {
        Some(Message::System(_)) => &messages[1..],
        _ => messages,
    }
}

/// Every tool definition the transcript declares, in first-declaration order with the latest value.
pub fn declared_tools(messages: &[Message]) -> Vec<Tool> {
    let mut tools: Vec<Tool> = Vec::new();
    for tool in system_messages(messages).flat_map(|system| system.tools_added.iter().flatten()) {
        match tools.iter_mut().find(|existing| existing.name == tool.name) {
            Some(existing) => *existing = tool.clone(),
            None => tools.push(tool.clone()),
        }
    }
    tools
}

/// Whether a tool name was declared twice with different definitions.
pub fn has_tool_redefinitions(messages: &[Message]) -> bool {
    let mut declared: BTreeMap<&str, &Tool> = BTreeMap::new();
    for tool in system_messages(messages).flat_map(|system| system.tools_added.iter().flatten()) {
        if declared.insert(&tool.name, tool).is_some_and(|before| before != tool) {
            return true;
        }
    }
    false
}

/// How a transport can express tool changes after the first request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TranscriptToolSupport {
    /// Only the top-level tool list exists; it always holds the current tools.
    None,
    /// Tools can be added at a later message, but not removed or redefined.
    Additions,
    /// Every tool, including the initial ones, is declared in place, and a later declaration under
    /// the same name replaces the earlier one. Removals are not expressible on the wire, so the
    /// transport announces them as text and the removed tools stay declared.
    Changes,
}

/// Tool declarations split between the request-level field and in-place changes.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TranscriptTools {
    /// Tools sent in the top-level request field.
    pub request_tools: Vec<Tool>,
    /// Number of leading system messages whose tool changes `request_tools` already holds. Every
    /// later system message carries its own tool changes in place.
    pub folded_system_messages: usize,
}

/// Split tool declarations between the top-level request field and in-place changes.
///
/// With `Changes`, the top-level list stays empty and every system message, including the leading
/// one, declares its tools in place: an in-place declaration can replace an earlier in-place one,
/// but not a top-level one. With `Additions`, a removal or redefinition cannot be expressed in
/// place, so the top-level list becomes the tool state after the last such change; later additions
/// stay anchored, and the cached prefix is invalidated once per removal instead of on every later
/// addition. With `None`, the top-level list is the current tool set.
pub fn resolve_transcript_tools(messages: &[Message], support: TranscriptToolSupport) -> TranscriptTools {
    match support {
        TranscriptToolSupport::None => TranscriptTools {
            request_tools: current_tools(messages),
            folded_system_messages: system_messages(messages).count(),
        },
        TranscriptToolSupport::Changes => TranscriptTools::default(),
        TranscriptToolSupport::Additions => {
            let mut folded = usize::from(initial_system_message(messages).is_some());
            // A name declared before counts as a redeclaration even after its removal: Kimi K3
            // ignores a removed tool that a later tool message declares again.
            let mut declared = HashSet::new();
            for (index, system) in system_messages(messages).enumerate() {
                let added = system.tools_added.as_deref().unwrap_or_default();
                let removes = system.tools_removed.as_ref().is_some_and(|removed| !removed.is_empty());
                if removes || added.iter().any(|tool| declared.contains(tool.name.as_str())) {
                    folded = folded.max(index + 1);
                }
                declared.extend(added.iter().map(|tool| tool.name.as_str()));
            }
            TranscriptTools {
                request_tools: tools_after(system_messages(messages).take(folded)),
                folded_system_messages: folded,
            }
        }
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
    fn resolves_tools_for_addition_only_transports() {
        let added = |name: &str, timestamp: i64| {
            system(json!({"role": "system", "content": "", "toolsAdded": [tool(name)], "timestamp": timestamp}))
        };
        let messages = vec![added("a", 1), Message::user("x"), added("b", 2)];
        let anchored = resolve_transcript_tools(&messages, TranscriptToolSupport::Additions);
        assert_eq!((anchored.folded_system_messages, anchored.request_tools), (1, vec![tool("a")]));
        let flat = resolve_transcript_tools(&messages, TranscriptToolSupport::None);
        assert_eq!((flat.folded_system_messages, flat.request_tools), (2, vec![tool("a"), tool("b")]));
        let mut redeclared = messages.clone();
        let changed = Tool::new("a", "changed", json!({"type": "object"}));
        redeclared.push(system(json!({"role": "system", "content": "", "toolsAdded": [changed], "timestamp": 3})));
        assert!(has_tool_redefinitions(&redeclared) && !has_tool_redefinitions(&messages));
        let resolved = resolve_transcript_tools(&redeclared, TranscriptToolSupport::Additions);
        assert_eq!(resolved.folded_system_messages, 3);
        assert_eq!(
            resolved.request_tools.iter().map(|tool| tool.description.as_str()).collect::<Vec<_>>(),
            ["changed", "d"]
        );
        assert_eq!(declared_tools(&redeclared).iter().map(|tool| tool.name.as_str()).collect::<Vec<_>>(), ["a", "b"]);
        assert_eq!(declared_tools(&redeclared)[0].description, "changed");
        assert_eq!(resolve_transcript(messages.clone(), true).len(), 3);
        assert_eq!(resolve_transcript(messages.clone(), false).len(), 2);
        assert_eq!(without_initial_system_message(&messages).len(), 2);
    }

    #[test]
    fn resolves_the_top_level_tool_boundary_per_transport() {
        let late = |description: &str| Tool::new("late_tool", description, json!({"type": "object"}));
        let (base, dropped) = (|| tool("base_tool"), || tool("drop_tool"));
        let at = |timestamp: i64, fields: Value| {
            let mut message = json!({"role": "system", "content": "", "timestamp": timestamp});
            message.as_object_mut().unwrap().extend(fields.as_object().unwrap().clone());
            system(message)
        };
        let messages = [
            at(0, json!({"content": "base prompt", "toolsAdded": [base(), dropped()]})),
            at(1, json!({"toolsRemoved": [{"name": "drop_tool"}]})),
            at(2, json!({"toolsAdded": [dropped()]})),
            at(3, json!({"toolsAdded": [late("x")]})),
            at(4, json!({"toolsAdded": [late("y")]})),
        ];
        let resolve = |count: usize, support| {
            let resolved = resolve_transcript_tools(&messages[..count], support);
            (resolved.request_tools, resolved.folded_system_messages)
        };
        let additions = TranscriptToolSupport::Additions;
        assert_eq!(resolve(2, additions), (vec![base()], 2));
        // Kimi K3 ignores a removed tool that a later tool message declares again, so the re-added
        // drop_tool moves the boundary; the new late_tool after it stays anchored.
        assert_eq!(resolve(4, additions), (vec![base(), dropped()], 3));
        // The redefinition of late_tool moves the boundary to its message.
        assert_eq!(resolve(5, additions), (vec![base(), dropped(), late("y")], 5));
        assert_eq!(resolve(5, TranscriptToolSupport::Changes), (Vec::new(), 0));
        assert_eq!(resolve(5, TranscriptToolSupport::None), (vec![base(), dropped(), late("y")], 5));
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
