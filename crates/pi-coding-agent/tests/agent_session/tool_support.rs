//! Sessions with the built-in MCP, codemode, and tool_search extensions over the fake MCP
//! server, and helpers to read declared tools, tool results, and prompt sections.

use std::sync::{Arc, Mutex};

use pi_ai::{ContentBlock, Message, ToolResultMessage};
use pi_coding_agent::agent_session::AgentSession;
use serde_json::{Value, json};

use crate::harness::{Harness, Options, harness_with, wait_until};
use crate::mcp_fake::{Fake, FakeOptions};

pub struct Setup {
    pub h: Harness,
    pub fake: Fake,
    pub notifications: Arc<Mutex<Vec<String>>>,
}

pub struct SetupOptions {
    /// `--tools`.
    pub tools: Option<Vec<&'static str>>,
    /// `--exclude-tools`.
    pub exclude: Vec<&'static str>,
    /// The `defaultTools` setting: the tools active at the start.
    pub default_tools: Vec<&'static str>,
    pub fake: FakeOptions,
    /// `autoEnableCodemode` and `startupWaitMs`.
    pub extra: Value,
    /// More settings.
    pub settings: Value,
    pub server: &'static str,
    /// Wait for `mcp__<server>__search` to register.
    pub wait_for_tools: bool,
}

impl Default for SetupOptions {
    fn default() -> Self {
        Self {
            tools: None,
            exclude: Vec::new(),
            default_tools: Vec::new(),
            fake: FakeOptions::default(),
            extra: json!({}),
            settings: json!({}),
            server: "docs",
            wait_for_tools: true,
        }
    }
}

/// Notifications the session shows, as text.
pub fn capture_notifications(session: &AgentSession) -> Arc<Mutex<Vec<String>>> {
    let notifications: Arc<Mutex<Vec<String>>> = Arc::default();
    let sink = notifications.clone();
    session.set_notify_handler(Some(Arc::new(move |message: &str, _| sink.lock().unwrap().push(message.into()))));
    notifications
}

/// A session with one MCP server of `config` (plus `url` when it has no transport).
pub async fn setup(mut config: Value, options: SetupOptions) -> Setup {
    if config.get("url").is_none() && config.get("command").is_none() {
        config["url"] = json!("http://unused.invalid");
    }
    let fake = Fake::new(options.fake.clone());
    let mut settings = json!({ "defaultTools": options.default_tools });
    for (key, value) in options.settings.as_object().unwrap() {
        settings[key] = value.clone();
    }
    let mcp = fake.session_options(options.server, config, options.extra.clone());
    let tools = options.tools.clone().map(|tools| tools.iter().map(|name| name.to_string()).collect());
    let exclude: Vec<String> = options.exclude.iter().map(|name| name.to_string()).collect();
    let h = harness_with(Options { settings, ..Options::default() }, |create| {
        create.tools = tools;
        create.exclude_tools = exclude;
        create.mcp = mcp;
    })
    .await;
    let notifications = capture_notifications(&h.session);
    if options.wait_for_tools {
        let search = format!("mcp__{}__search", options.server.replace('-', "_"));
        wait_until(|| h.session.all_tools().iter().any(|tool| tool.name == search)).await;
    }
    Setup { h, fake, notifications }
}

/// Names of every tool the transcript declared, in order.
pub fn declared_tool_names(session: &AgentSession) -> Vec<String> {
    session
        .messages()
        .iter()
        .filter_map(|message| match message {
            Message::System(system) => system.tools_added.clone(),
            _ => None,
        })
        .flatten()
        .map(|tool| tool.name)
        .collect()
}

/// The last result of the tool `name`.
pub fn tool_result(session: &AgentSession, name: &str) -> ToolResultMessage {
    let mut results = session.messages().into_iter().filter_map(|message| match message {
        Message::ToolResult(result) if result.tool_name == name => Some(result),
        _ => None,
    });
    results.next_back().unwrap_or_else(|| panic!("no result of {name}"))
}

pub fn result_text(result: &ToolResultMessage) -> String {
    result.content.iter().filter_map(ContentBlock::as_text).collect::<Vec<_>>().join("\n")
}

/// The last line of a tool result's text, where codemode puts a returned value.
pub fn last_line(result: &ToolResultMessage) -> String {
    result_text(result).lines().last().unwrap_or_default().to_string()
}

/// The `mcp_servers` section as the model has it now.
pub fn servers_section(session: &AgentSession) -> Option<String> {
    let mut section = None;
    for message in session.messages() {
        if let Message::System(system) = message
            && let Some(value) = system.sections.as_ref().and_then(|sections| sections.get("mcp_servers"))
        {
            section = value.as_str().map(str::to_string);
        }
    }
    section
}

/// The description the agent sends for a declared tool.
pub fn declared_description(session: &AgentSession, name: &str) -> Option<String> {
    session.with_state(|state| {
        state.tools.iter().find(|tool| tool.name() == name).map(|tool| tool.description().to_string())
    })
}
