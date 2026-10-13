//! Tool loadout declarations: system messages in the transcript declare what the model may
//! call, and the loop announces differences from the executable tools before each request.

use pi_ai::transcript::tool_state_changes;
use pi_ai::{Message, SystemMessage, Tool, ToolReference, UserContent, now_ms};

use crate::agent_loop::AgentContext;

/// Whether the context keeps its prompt and tool declarations in transcript system messages.
/// A context with `system_prompt` set sends the prompt and tools beside the messages instead.
pub fn is_transcript_context(context: &AgentContext) -> bool {
    context.system_prompt.is_none()
}

/// Tools declared after replaying the system messages of `messages` in order.
pub fn declared_tools<'a>(messages: impl IntoIterator<Item = &'a Message>) -> Vec<Tool> {
    let mut tools: Vec<Tool> = Vec::new();
    for message in messages {
        let Message::System(system) = message else { continue };
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

/// Copy a system message with its tool fields replaced; empty lists leave a field out.
fn with_tool_changes(message: &SystemMessage, added: Vec<Tool>, removed: Vec<ToolReference>) -> SystemMessage {
    SystemMessage {
        tools_added: (!added.is_empty()).then_some(added),
        tools_removed: (!removed.is_empty()).then_some(removed),
        ..message.clone()
    }
}

/// Declare tool loadout changes before the next request (upstream `declareToolChanges`), so
/// replaying the transcript yields exactly `context.tools`. A pending system message carries the
/// changes; otherwise a new one goes before the first non-system message.
pub fn declare_tool_changes(context: &AgentContext, pending: Vec<Message>) -> Vec<Message> {
    if !is_transcript_context(context) {
        return pending;
    }
    let system_index = pending.iter().rposition(|message| matches!(message, Message::System(_)));
    // The pending message's tool fields are intent; the executable set decides.
    let mut baseline = pending.clone();
    if let Some(index) = system_index
        && let Message::System(system) = &pending[index]
    {
        baseline[index] = Message::System(with_tool_changes(system, Vec::new(), Vec::new()));
    }
    let declared = declared_tools(context.messages.iter().chain(baseline.iter()));
    let executable: Vec<Tool> = context.tools.iter().map(|tool| tool.declaration()).collect();
    let (added, removed) = tool_state_changes(&declared, &executable);
    let unchanged = added.is_empty() && removed.is_empty();

    if let Some(index) = system_index
        && let Message::System(system) = &pending[index]
    {
        let declares_nothing = system.tools_added.as_ref().is_none_or(Vec::is_empty)
            && system.tools_removed.as_ref().is_none_or(Vec::is_empty);
        if unchanged && declares_nothing {
            return pending;
        }
        baseline[index] = Message::System(with_tool_changes(system, added, removed));
        return baseline;
    }
    if unchanged {
        return pending;
    }
    let empty = SystemMessage {
        content: UserContent::Text(String::new()),
        sections: None,
        tools_added: None,
        tools_removed: None,
        timestamp: now_ms(),
    };
    let update = Message::System(with_tool_changes(&empty, added, removed));
    let index = pending.iter().position(|message| !matches!(message, Message::System(_))).unwrap_or(pending.len());
    let mut messages = pending;
    messages.insert(index, update);
    messages
}

/// Remove `hidden` tool declarations from every system message, for requests only. The whole
/// transcript is filtered with the current set, so projected declarations stay consistent.
pub fn hide_tool_declarations(messages: Vec<Message>, hidden: &[String]) -> Vec<Message> {
    if hidden.is_empty() {
        return messages;
    }
    let keep = |name: &str| !hidden.iter().any(|hidden| hidden == name);
    messages
        .into_iter()
        .map(|message| match message {
            Message::System(system) if system.tools_added.is_some() || system.tools_removed.is_some() => {
                let added: Vec<Tool> = system.tools_added.iter().flatten().filter(|t| keep(&t.name)).cloned().collect();
                let removed: Vec<ToolReference> =
                    system.tools_removed.iter().flatten().filter(|t| keep(&t.name)).cloned().collect();
                Message::System(with_tool_changes(&system, added, removed))
            }
            message => message,
        })
        .collect()
}
