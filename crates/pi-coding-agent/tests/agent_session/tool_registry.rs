//! The session's tool registry (upstream `default-tools-setting.test.ts` and
//! `agent-session-dynamic-tools.test.ts`): `defaultTools`, `--tools`, `--exclude-tools`,
//! `--no-tools`, tools registered later, tool deltas, and the built-in extension toggles.

use std::sync::Arc;

use async_trait::async_trait;
use pi_agent::{AgentTool, AgentToolResult, SharedTool, ToolUpdateCallback};
use pi_ai::Message;
use pi_ai::providers::faux::FauxResponse;
use pi_coding_agent::agent_session::{
    AgentSession, CreateAgentSessionOptions, CreateServicesOptions, NoTools, ToolSource, create_agent_session,
    create_agent_session_services,
};
use pi_coding_agent::session::SessionManager;
use pi_coding_agent::settings::{SettingsManager, SettingsOptions};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::harness::{Harness, Options, harness_with, open_session_with};

struct Plain {
    name: &'static str,
    snippet: Option<&'static str>,
    guidelines: Vec<String>,
    default_active: bool,
}

#[async_trait]
impl AgentTool for Plain {
    fn name(&self) -> &str {
        self.name
    }
    fn description(&self) -> &str {
        "A test tool"
    }
    fn parameters(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }
    fn prompt_snippet(&self) -> Option<&str> {
        self.snippet
    }
    fn prompt_guidelines(&self) -> Vec<String> {
        self.guidelines.clone()
    }
    fn default_active(&self) -> bool {
        self.default_active
    }
    async fn execute(
        &self,
        _: &str,
        _: Value,
        _: CancellationToken,
        _: ToolUpdateCallback,
    ) -> anyhow::Result<AgentToolResult> {
        Ok(AgentToolResult::text("ok"))
    }
}

pub(crate) fn tool(name: &'static str, default_active: bool) -> SharedTool {
    Arc::new(Plain { name, snippet: None, guidelines: Vec::new(), default_active })
}

fn strings(names: &[&str]) -> Vec<String> {
    names.iter().map(|name| name.to_string()).collect()
}

#[derive(Default)]
pub(crate) struct Select {
    pub default_tools: Option<Vec<&'static str>>,
    pub tools: Option<Vec<&'static str>>,
    pub exclude: Vec<&'static str>,
    pub no_tools: Option<NoTools>,
    pub custom: Vec<SharedTool>,
}

pub(crate) async fn session_with(select: Select) -> Harness {
    let settings = match &select.default_tools {
        Some(defaults) => json!({ "defaultTools": defaults }),
        None => json!({}),
    };
    harness_with(Options { settings, ..Options::default() }, |create| apply(create, select)).await
}

pub(crate) fn apply(create: &mut CreateAgentSessionOptions, select: Select) {
    create.tools = select.tools.map(|tools| strings(&tools));
    create.exclude_tools = strings(&select.exclude);
    create.no_tools = select.no_tools;
    create.custom_tools = select.custom;
}

fn active_sorted(h: &Harness) -> Vec<String> {
    let mut names = h.session.active_tool_names();
    names.sort();
    names
}

#[tokio::test]
async fn uses_the_configured_list_as_the_initial_built_in_selection() {
    let h = session_with(Select { default_tools: Some(vec!["grep", "find"]), ..Select::default() }).await;
    let all: Vec<String> = h.session.all_tools().into_iter().map(|tool| tool.name).collect();
    for name in ["read", "bash", "edit", "write", "grep", "find", "ls"] {
        assert!(all.contains(&name.to_string()), "{name} in {all:?}");
    }
    assert_eq!(h.session.active_tool_names(), ["grep", "find"]);
    assert!(h.session.system_prompt().contains("- grep:"));
    assert!(!h.session.system_prompt().contains("- read:"));
}

#[tokio::test]
async fn activates_an_inactive_tool_with_plus_name() {
    let custom = vec![tool("inactive_tool", false)];
    let h = session_with(Select { default_tools: Some(vec!["+inactive_tool", "-write"]), custom, ..Select::default() })
        .await;
    assert_eq!(active_sorted(&h), ["bash", "edit", "inactive_tool", "read"]);
}

#[tokio::test]
async fn keeps_sdk_tools_and_tools_registered_later_enabled_with_their_sources() {
    let custom = vec![tool("sdk_tool", true)];
    let h = session_with(Select { default_tools: Some(vec!["grep"]), custom, ..Select::default() }).await;
    let dynamic = Plain {
        name: "dynamic_tool",
        snippet: Some("Run dynamic test behavior"),
        guidelines: vec!["Use dynamic_tool when the user asks for dynamic behavior tests.".into()],
        default_active: true,
    };
    h.session.register_tool(Arc::new(dynamic), ToolSource::extension("<inline:1>", "inline"));

    assert_eq!(active_sorted(&h), ["dynamic_tool", "grep", "sdk_tool"]);
    let all = h.session.all_tools();
    let source = |name: &str| {
        let info = all.iter().find(|tool| tool.name == name).unwrap();
        (info.source_path.clone(), info.source.clone())
    };
    assert_eq!(source("sdk_tool"), ("<sdk:sdk_tool>".into(), "sdk".into()));
    assert_eq!(source("read"), ("builtin:read".into(), "builtin".into()));
    assert_eq!(source("dynamic_tool"), ("<inline:1>".into(), "inline".into()));
    let dynamic = all.iter().find(|tool| tool.name == "dynamic_tool").unwrap();
    assert_eq!(dynamic.prompt_guidelines, ["Use dynamic_tool when the user asks for dynamic behavior tests."]);
    let prompt = h.session.system_prompt();
    assert!(prompt.contains("- dynamic_tool: Run dynamic test behavior"));
    assert!(prompt.contains("- Use dynamic_tool when the user asks for dynamic behavior tests."));

    // Tools without a snippet stay active but are not listed in the prompt.
    h.session.register_tool(tool("hidden_tool", true), ToolSource::extension("<inline:2>", "inline"));
    assert!(h.session.active_tool_names().contains(&"hidden_tool".to_string()));
    assert!(!h.session.system_prompt().contains("hidden_tool"));

    assert!(h.session.unregister_tool("dynamic_tool"));
    assert!(!h.session.all_tools().iter().any(|tool| tool.name == "dynamic_tool"));
    assert!(!h.session.system_prompt().contains("dynamic_tool"));
    assert!(!h.session.unregister_tool("read"), "built-in tools stay");
}

#[tokio::test]
async fn preserves_explicit_tool_option_precedence() {
    let allowlisted =
        session_with(Select { default_tools: Some(vec!["grep"]), tools: Some(vec!["read"]), ..Select::default() })
            .await;
    assert_eq!(allowlisted.session.active_tool_names(), ["read"]);

    let excluded =
        session_with(Select { default_tools: Some(vec!["read", "grep"]), exclude: vec!["read"], ..Select::default() })
            .await;
    assert_eq!(excluded.session.active_tool_names(), ["grep"]);

    let tool_less =
        session_with(Select { default_tools: Some(vec!["read"]), no_tools: Some(NoTools::All), ..Select::default() })
            .await;
    assert!(tool_less.session.all_tools().is_empty());
    assert!(tool_less.session.active_tool_names().is_empty());

    // --no-builtin-tools only empties the default selection.
    let builtinless = session_with(Select { no_tools: Some(NoTools::Builtin), ..Select::default() }).await;
    let all: Vec<String> = builtinless.session.all_tools().into_iter().map(|tool| tool.name).collect();
    assert!(all.contains(&"read".to_string()) && all.contains(&"codemode".to_string()));
    assert!(builtinless.session.active_tool_names().is_empty());
}

#[tokio::test]
async fn applies_plus_and_minus_tool_options_to_the_default_selection() {
    let custom = || vec![tool("inactive_tool", false), tool("active_tool", true)];
    let h = session_with(Select {
        default_tools: Some(vec!["+grep"]),
        tools: Some(vec!["+inactive_tool", "-write"]),
        custom: custom(),
        ..Select::default()
    })
    .await;
    assert_eq!(active_sorted(&h), ["active_tool", "bash", "edit", "grep", "inactive_tool", "read"]);

    let tool_less = session_with(Select {
        default_tools: Some(vec!["read"]),
        tools: Some(vec!["+inactive_tool"]),
        no_tools: Some(NoTools::All),
        custom: custom(),
        ..Select::default()
    })
    .await;
    assert_eq!(tool_less.session.active_tool_names(), ["inactive_tool"]);
}

#[tokio::test]
async fn rejects_invalid_tool_modifier_options() {
    let h = session_with(Select::default()).await;
    for (tools, message) in [
        (vec!["read", "+grep"], "Invalid tools option: tool names cannot be mixed with +name or -name entries"),
        (vec!["-gr*"], "Invalid tools option: +name and -name entries take exact tool names, not patterns: -gr*"),
    ] {
        let mut options = CreateAgentSessionOptions::new(h.services.clone(), SessionManager::in_memory(&h.cwd));
        options.tools = Some(strings(&tools));
        let error = create_agent_session(options).await.err().expect("rejected");
        assert_eq!(error.to_string(), message);
    }
}

/// Write `<agent_dir>/settings.json` with `defaultTools` (when given) for the next reload.
fn write_settings(h: &Harness, default_tools: Option<&[&str]>) {
    let mut settings = json!({ "defaultProvider": "local", "defaultModel": "tiny" });
    if let Some(tools) = default_tools {
        settings["defaultTools"] = json!(tools);
    }
    std::fs::write(h.agent_dir.join("settings.json"), settings.to_string()).unwrap();
}

/// A session over the settings file of a fresh harness, so `reload()` reads what the test writes.
async fn file_session(default_tools: Option<&[&str]>, select: Select) -> (Harness, Arc<AgentSession>) {
    let h = session_with(Select::default()).await;
    write_settings(&h, default_tools);
    let settings = Arc::new(SettingsManager::create(&h.cwd, &h.agent_dir, SettingsOptions::default()));
    let options = CreateServicesOptions {
        settings: Some(settings),
        model_runtime: Some(h.services.model_runtime.clone()),
        ..CreateServicesOptions::new(&h.cwd, &h.agent_dir)
    };
    let services = create_agent_session_services(options).await;
    let session = open_session_with(&services, SessionManager::in_memory(&h.cwd), |create| apply(create, select)).await;
    (h, session)
}

pub(crate) fn sorted(session: &AgentSession) -> Vec<String> {
    let mut names = session.active_tool_names();
    names.sort();
    names
}

fn inactive() -> Select {
    Select { custom: vec![tool("inactive_tool", false)], ..Select::default() }
}

#[tokio::test]
async fn activates_only_tools_newly_added_to_default_tools_on_reload() {
    let (h, session) = file_session(None, inactive()).await;
    assert_eq!(session.active_tool_names(), ["read", "bash", "edit", "write"]);
    session.set_active_tools_by_name(&strings(&["read", "edit", "write"]));

    write_settings(&h, Some(&["+inactive_tool", "+grep"]));
    session.reload();
    // bash was turned off during the session and is not newly added, so it stays off.
    assert_eq!(sorted(&session), ["edit", "grep", "inactive_tool", "read", "write"]);

    // Removing tools from the setting does not turn them off.
    write_settings(&h, Some(&["-read"]));
    session.reload();
    assert_eq!(sorted(&session), ["edit", "grep", "inactive_tool", "read", "write"]);
}

#[tokio::test]
async fn keeps_tools_removed_by_minus_tool_options_removed_on_reload() {
    let select = Select { tools: Some(vec!["-bash", "+grep"]), ..inactive() };
    let (h, session) = file_session(Some(&["read"]), select).await;
    assert_eq!(session.active_tool_names(), ["read", "grep"]);
    write_settings(&h, Some(&["read", "bash", "inactive_tool"]));
    session.reload();
    assert_eq!(sorted(&session), ["grep", "inactive_tool", "read"]);
}

#[tokio::test]
async fn keeps_explicit_tool_options_on_reload() {
    let (h, allowlisted) = file_session(None, Select { tools: Some(vec!["read"]), ..inactive() }).await;
    write_settings(&h, Some(&["+grep"]));
    allowlisted.reload();
    assert_eq!(allowlisted.active_tool_names(), ["read"]);

    let (h, builtinless) = file_session(None, Select { no_tools: Some(NoTools::Builtin), ..inactive() }).await;
    write_settings(&h, Some(&["+grep"]));
    builtinless.reload();
    assert!(builtinless.active_tool_names().is_empty());

    let (h, excluded) = file_session(None, Select { exclude: vec!["grep"], ..inactive() }).await;
    write_settings(&h, Some(&["+grep", "+inactive_tool"]));
    excluded.reload();
    assert_eq!(sorted(&excluded), ["bash", "edit", "inactive_tool", "read", "write"]);
}

#[tokio::test]
async fn declares_tool_changes_between_prompts_in_system_messages() {
    let h = session_with(Select::default()).await;
    h.provider.push(FauxResponse::text("one"));
    h.session.prompt("first", Vec::new()).await.unwrap();
    h.session.set_active_tools_by_name(&strings(&["read", "edit", "write", "grep"]));
    h.provider.push(FauxResponse::text("two"));
    h.session.prompt("second", Vec::new()).await.unwrap();

    let systems: Vec<_> = h
        .session
        .messages()
        .into_iter()
        .filter_map(|message| match message {
            Message::System(system) => Some(system),
            _ => None,
        })
        .collect();
    assert_eq!(systems.len(), 2);
    let names = |tools: &Option<Vec<pi_ai::Tool>>| -> Vec<String> {
        tools.iter().flatten().map(|tool| tool.name.clone()).collect()
    };
    assert_eq!(names(&systems[0].tools_added), ["read", "bash", "edit", "write"]);
    assert_eq!(names(&systems[1].tools_added), ["grep"]);
    let removed: Vec<String> = systems[1].tools_removed.iter().flatten().map(|tool| tool.name.clone()).collect();
    assert_eq!(removed, ["bash"]);
    // The request carries the current declarations only.
    let request = h.requests().pop().unwrap();
    let declared: Vec<&str> = request.tools.iter().map(|tool| tool.name.as_str()).collect();
    assert_eq!(declared, ["read", "edit", "write", "grep"]);
}

#[tokio::test]
async fn turns_built_in_extensions_off_with_the_extensions_setting() {
    let on = session_with(Select::default()).await;
    let names: Vec<String> = on.session.all_tools().into_iter().map(|tool| tool.name).collect();
    assert!(names.contains(&"codemode".to_string()) && names.contains(&"tool_search".to_string()));
    assert!(on.session.mcp_manager().is_some());
    let loaded = |h: &Harness| -> Vec<String> {
        let loader = h.services.resources();
        loader.resources().extensions.iter().map(|entry| entry.path.display().to_string()).collect()
    };
    assert_eq!(loaded(&on), ["builtin:llama.cpp", "builtin:codemode", "builtin:tool-search", "builtin:mcp"]);

    let settings = json!({ "extensions": ["-builtin:codemode", "-builtin:mcp", "-builtin:llama.cpp"] });
    let off = harness_with(Options { settings, ..Options::default() }, |_| {}).await;
    let names: Vec<String> = off.session.all_tools().into_iter().map(|tool| tool.name).collect();
    assert!(!names.contains(&"codemode".to_string()));
    assert!(names.contains(&"tool_search".to_string()));
    assert!(off.session.mcp_manager().is_none());
    assert_eq!(loaded(&off), ["builtin:tool-search"]);
    assert!(off.services.resources().resources().extension_diagnostics.is_empty());
}

#[tokio::test]
async fn turns_mcp_off_for_the_run_like_no_mcp() {
    let h = session_with(Select::default()).await;
    let mut options = CreateServicesOptions {
        settings: Some(h.settings.clone()),
        model_runtime: Some(h.services.model_runtime.clone()),
        ..CreateServicesOptions::new(&h.cwd, &h.agent_dir)
    };
    options.resources.disabled_builtin_extensions = strings(&["mcp"]);
    let services = create_agent_session_services(options).await;
    let session = open_session_with(&services, SessionManager::in_memory(&h.cwd), |_| {}).await;
    assert!(session.mcp_manager().is_none());
    assert!(session.all_tools().iter().any(|tool| tool.name == "codemode"));
}
