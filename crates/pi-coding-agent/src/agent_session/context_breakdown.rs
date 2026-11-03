//! The estimated breakdown of the next request by part (pi-rust extension), for `/context` and
//! for extensions through `ctx.getContextBreakdown()`.

use pi_ai::{JsonObject, Message, SystemMessage, Tool, UserContent};
use serde_json::{Value, json};

use super::tools::ToolSource;
use super::{AgentSession, lock};
use crate::compaction::estimate_tokens;
use crate::system_prompt::{SystemPromptState, build_system_prompt_state, sections_to_json};

/// Where a declared tool comes from, for the breakdown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolOrigin {
    Builtin,
    /// Tools of extensions, packages, the SDK, and built-in extensions other than MCP.
    Extension,
    Mcp,
}

impl ToolOrigin {
    fn of(name: &str, source: &ToolSource) -> Self {
        match (source.source.as_str(), source.path.strip_prefix("builtin:")) {
            ("builtin", Some("mcp")) => Self::Mcp,
            ("builtin", Some(path)) if path == name => Self::Builtin,
            _ => Self::Extension,
        }
    }
}

/// Estimated tokens of each part of the next request.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ContextBreakdown {
    pub system_prompt: u64,
    pub builtin_tools: u64,
    pub extension_tools: u64,
    pub mcp_tools: u64,
    pub context_files: u64,
    pub skills: u64,
    /// User prompts, plus shell command output, extension messages, and summaries.
    pub user: u64,
    pub assistant: u64,
    pub tool_results: u64,
}

/// One part of a [`ContextBreakdown`]: its wire id, its label, and its tokens.
pub type BreakdownPart = (&'static str, &'static str, u64);

impl ContextBreakdown {
    /// The parts in display order. Colors and bar cells follow this order.
    pub fn parts(&self) -> [BreakdownPart; 9] {
        [
            ("systemPrompt", "System prompt", self.system_prompt),
            ("builtinTools", "Built-in tools", self.builtin_tools),
            ("extensionTools", "Extension tools", self.extension_tools),
            ("mcpTools", "MCP tools", self.mcp_tools),
            ("contextFiles", "Context files", self.context_files),
            ("skills", "Skills", self.skills),
            ("userMessages", "User messages", self.user),
            ("assistantMessages", "Assistant messages", self.assistant),
            ("toolResults", "Tool results", self.tool_results),
        ]
    }

    pub fn total(&self) -> u64 {
        self.parts().iter().map(|(_, _, tokens)| tokens).sum()
    }

    /// `{"parts": [{"id", "label", "tokens"}, ...], "total"}` for extensions.
    pub fn to_json(&self) -> Value {
        let parts: Vec<Value> = self
            .parts()
            .iter()
            .map(|(id, label, tokens)| json!({"id": id, "label": label, "tokens": tokens}))
            .collect();
        json!({"parts": parts, "total": self.total()})
    }
}

/// The compaction estimate of a system message with these parts.
pub(crate) fn system_tokens(content: &str, sections: Option<JsonObject>, tools: &[Tool]) -> u64 {
    let system = SystemMessage {
        content: UserContent::Text(content.to_string()),
        sections,
        tools_added: (!tools.is_empty()).then(|| tools.to_vec()),
        tools_removed: None,
        timestamp: 0,
    };
    estimate_tokens(&Message::System(system))
}

/// Context files and skills are prompt sections; the rest of the prompt is the system prompt.
/// System messages in `messages` only patch the prompt, so they are not counted again.
pub fn context_breakdown(
    prompt: &SystemPromptState,
    tools: &[(Tool, ToolOrigin)],
    messages: &[Message],
) -> ContextBreakdown {
    let sections = prompt.sections.as_deref().unwrap_or_default();
    let section = |name: &str| {
        sections.iter().find(|(section, _)| section == name).map_or(0, |(_, text)| system_tokens(text, None, &[]))
    };
    let (context_files, skills) = (section("project_context"), section("skills"));
    let whole = system_tokens(&prompt.content, prompt.sections.as_ref().map(sections_to_json), &[]);
    let tool_tokens = |origin: ToolOrigin| {
        let declared: Vec<Tool> =
            tools.iter().filter(|(_, from)| *from == origin).map(|(tool, _)| tool.clone()).collect();
        if declared.is_empty() { 0 } else { system_tokens("", None, &declared) }
    };
    let mut breakdown = ContextBreakdown {
        system_prompt: whole.saturating_sub(context_files + skills),
        builtin_tools: tool_tokens(ToolOrigin::Builtin),
        extension_tools: tool_tokens(ToolOrigin::Extension),
        mcp_tools: tool_tokens(ToolOrigin::Mcp),
        context_files,
        skills,
        ..ContextBreakdown::default()
    };
    for message in messages {
        let slot = match message {
            Message::System(_) => continue,
            Message::User(_) | Message::Custom(_) => &mut breakdown.user,
            Message::Assistant(_) => &mut breakdown.assistant,
            Message::ToolResult(_) => &mut breakdown.tool_results,
        };
        *slot += estimate_tokens(message);
    }
    breakdown
}

impl AgentSession {
    /// The declared tools (active and not hidden) with their origin.
    pub(crate) fn declared_tools_by_origin(&self) -> Vec<(Tool, ToolOrigin)> {
        let hidden = self.hidden_declarations();
        let tools = lock(&self.tools);
        let declared =
            tools.registry.active_tools().iter().filter(|tool| !hidden.iter().any(|name| name == tool.name()));
        declared.map(|tool| (tool.declaration(), ToolOrigin::of(tool.name(), &tools.source_of(tool.name())))).collect()
    }

    /// The estimated breakdown of the next request (pi-rust extension).
    pub fn context_breakdown(&self) -> ContextBreakdown {
        let prompt = build_system_prompt_state(&self.effective_prompt_options())
            .unwrap_or(SystemPromptState { content: String::new(), sections: None });
        context_breakdown(&prompt, &self.declared_tools_by_origin(), &self.messages())
    }
}

#[cfg(test)]
mod tests {
    use pi_ai::{AssistantMessage, ContentBlock, ToolResultMessage, UserMessage};

    use super::*;

    fn text_of(chars: usize) -> String {
        "x".repeat(chars)
    }

    fn sectioned(sections: &[(&str, usize)]) -> SystemPromptState {
        let sections = sections.iter().map(|(name, chars)| (name.to_string(), text_of(*chars))).collect();
        SystemPromptState { content: String::new(), sections: Some(sections) }
    }

    fn message(role: &str, chars: usize) -> Message {
        let text = text_of(chars);
        match role {
            "system" => Message::System(SystemMessage {
                content: UserContent::Text(text),
                sections: None,
                tools_added: None,
                tools_removed: None,
                timestamp: 0,
            }),
            "user" => Message::User(UserMessage { content: UserContent::Text(text), timestamp: 0 }),
            "assistant" => Message::Assistant(AssistantMessage {
                content: vec![ContentBlock::text(text)],
                ..AssistantMessage::default()
            }),
            _ => Message::ToolResult(ToolResultMessage {
                content: vec![ContentBlock::text(text)],
                ..ToolResultMessage::default()
            }),
        }
    }

    #[test]
    fn splits_the_prompt_sections_tools_and_messages() {
        let prompt = sectioned(&[("preamble", 400), ("project_context", 80), ("skills", 40), ("cwd", 4)]);
        let tool = |name: &str| Tool::new(name, "A tool", json!({"type": "object"}));
        let tools = [
            (tool("read"), ToolOrigin::Builtin),
            (tool("todo"), ToolOrigin::Extension),
            (tool("mcp__gh__list"), ToolOrigin::Mcp),
            (tool("mcp__gh__view"), ToolOrigin::Mcp),
        ];
        let messages =
            vec![message("system", 800), message("user", 12), message("assistant", 20), message("tool", 400)];
        let breakdown = context_breakdown(&prompt, &tools, &messages);
        let tokens = |names: &[&str]| system_tokens("", None, &names.iter().map(|name| tool(name)).collect::<Vec<_>>());
        let expected = ContextBreakdown {
            system_prompt: 101,
            builtin_tools: tokens(&["read"]),
            extension_tools: tokens(&["todo"]),
            mcp_tools: tokens(&["mcp__gh__list", "mcp__gh__view"]),
            context_files: 20,
            skills: 10,
            user: 3,
            assistant: 5,
            tool_results: 100,
        };
        assert!(expected.builtin_tools > 0 && expected.mcp_tools > expected.extension_tools);
        assert_eq!(breakdown, expected);
    }

    #[test]
    fn a_forced_prompt_counts_as_the_system_prompt() {
        let prompt = SystemPromptState { content: text_of(40), sections: None };
        let breakdown = context_breakdown(&prompt, &[], &[]);
        assert_eq!(breakdown, ContextBreakdown { system_prompt: 10, ..ContextBreakdown::default() });
    }

    #[test]
    fn tool_origin_follows_the_source() {
        let origin = |name: &str, source: ToolSource| ToolOrigin::of(name, &source);
        assert_eq!(origin("read", ToolSource::builtin("read")), ToolOrigin::Builtin);
        assert_eq!(origin("mcp__github__list", ToolSource::builtin("mcp")), ToolOrigin::Mcp);
        assert_eq!(origin("todo_write", ToolSource::builtin("todo")), ToolOrigin::Extension);
        assert_eq!(origin("custom", ToolSource::sdk("custom")), ToolOrigin::Extension);
        assert_eq!(origin("ext", ToolSource::extension("/x/ext.ts", "local")), ToolOrigin::Extension);
    }

    #[test]
    fn the_json_lists_the_parts_in_order() {
        let breakdown = ContextBreakdown { system_prompt: 5, mcp_tools: 7, ..ContextBreakdown::default() };
        let value = breakdown.to_json();
        assert_eq!(value["total"], 12);
        assert_eq!(value["parts"][0], json!({"id": "systemPrompt", "label": "System prompt", "tokens": 5}));
        assert_eq!(value["parts"][3], json!({"id": "mcpTools", "label": "MCP tools", "tokens": 7}));
        assert_eq!(value["parts"].as_array().map(Vec::len), Some(9));
    }
}
