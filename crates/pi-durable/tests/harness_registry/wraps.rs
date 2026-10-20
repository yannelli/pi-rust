//! The upstream registry cases for tool and section wrappers, section rendering, and hook collection.

use std::collections::BTreeMap;

use pi_ai::ThinkingLevel;
use pi_chord::BACKGROUND_CONTEXT;
use pi_durable::ConversationId;
use pi_durable::harness::agent::agent_hooks;
use pi_durable::harness::generation::generation_task;
use pi_durable::harness::tool::tool_task;
use pi_durable::session::{SessionOptions, create_session};
use pi_durable::storage::MemoryStorage;

use super::*;

/// Resolve and also return the reported failures.
fn resolve_reporting(state: Option<&AgentState>, snapshot: &RegistrySnapshot) -> (Agent, Vec<String>) {
    let reports = Mutex::new(Vec::<String>::new());
    let settings = resolve_settings(Some(&HarnessSettings::default()));
    let agent = resolve_agent(state, snapshot, &settings, &|error, _| reports.lock().unwrap().push(error.to_string()));
    (agent, reports.into_inner().unwrap())
}

fn described(tool: &ToolRegistration, description: &str) -> ToolRegistration {
    let mut copy = tool.clone();
    copy.tool.description = description.to_owned();
    copy
}

fn tools(agent: &Agent) -> Vec<(String, String)> {
    agent.tools.iter().map(|tool| (tool.tool.name.clone(), tool.tool.description.clone())).collect()
}

fn filtered(filter: ToolFilter, snapshot: &RegistrySnapshot) -> Vec<String> {
    let state = AgentState { tools: Some(filter), ..AgentState::default() };
    resolve(Some(&state), snapshot, &HarnessSettings::default()).tool_names()
}

fn strings(names: &[&str]) -> Vec<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}

#[test]
fn replaces_same_name_tools_in_place_wraps_the_winner_then_applies_the_filter() {
    let (_, [coding, _, _]) = coding_registry();
    let local = create_registry();
    let calls = Arc::new(Mutex::new(Vec::<&str>::new()));
    let venv = Extension::new("venv").tool(described(&tool("bash"), "venv bash"));
    let suffix = |text: &'static str| {
        move |inner: Arc<ToolRegistration>| {
            let description = format!("{}{text}", inner.tool.description);
            Ok(Arc::new(described(&inner, &description)))
        }
    };
    let sink = calls.clone();
    let timing = Extension::new("timing")
        .wrap(wrap_tool("bash", suffix(" (timed)")))
        .wrap(wrap_tool("bash", suffix(" [2]")))
        // No `grep` is selected: the wrapper does nothing and reports nothing.
        .wrap(wrap_tool("grep", move |_| {
            sink.lock().unwrap().push("grep");
            Ok(Arc::new(tool("grep")))
        }));
    local.install(coding).unwrap();
    local.install(venv).unwrap();
    local.install(timing).unwrap();
    let (agent, reports) = resolve_reporting(None, &local.snapshot());
    let expected = [("read", "read tool"), ("bash", "venv bash (timed) [2]"), ("edit", "edit tool")];
    assert_eq!(tools(&agent), expected.map(|(name, text)| (name.to_owned(), text.to_owned())));
    assert!(reports.is_empty());
    assert!(calls.lock().unwrap().is_empty());
    // A list keeps exactly these names in its order, a repeated name at its first position.
    let list = ToolFilter::List(strings(&["edit", "missing", "read", "edit"]));
    assert_eq!(filtered(list, &local.snapshot()), ["edit", "read"]);
    assert_eq!(filtered(ToolFilter::Remove { remove: strings(&["bash"]) }, &local.snapshot()), ["read", "edit"]);
}

#[test]
fn drops_a_tool_or_section_whose_wrapper_throws_or_renames_it_and_reports_the_failure() {
    let (_, [coding, _, _]) = coding_registry();
    let local = create_registry();
    let broken = Extension::new("broken")
        .wrap(wrap_tool("read", |_| Err(DurableError::error("wrapper failed"))))
        .wrap(wrap_tool("edit", |inner| {
            let mut renamed = (*inner).clone();
            renamed.tool.name = "renamed".into();
            Ok(Arc::new(renamed))
        }))
        .wrap(wrap_section("cwd", |_| Err(DurableError::error("section wrapper failed"))));
    local.install(coding).unwrap();
    local.install(broken).unwrap();
    let (agent, reports) = resolve_reporting(None, &local.snapshot());
    assert_eq!(agent.tool_names(), ["bash"]);
    assert_eq!(agent.sections.iter().map(|s| s.key.as_str()).collect::<Vec<_>>(), ["preamble"]);
    assert_eq!(reports, ["wrapper failed", "Wrapper renamed edit to renamed", "section wrapper failed"]);
}

/// Render every section of `agent` with a bare input.
async fn rendered(agent: &Agent) -> Vec<(String, Option<String>)> {
    let read = create_session(Arc::new(MemoryStorage::new()), SessionOptions::default());
    let mut out = Vec::new();
    for section in &agent.sections {
        let input = PromptInput {
            conversation_id: ConversationId(1),
            agent: agent.clone(),
            env: None,
            shown: BTreeMap::new(),
            read: read.clone(),
        };
        out.push((section.key.clone(), (section.render)(input, BACKGROUND_CONTEXT.clone()).await.unwrap()));
    }
    out
}

#[tokio::test]
async fn orders_sections_by_extension_replaces_same_keys_in_place_and_renders_instructions_last_and_unwrapped() {
    let (_, [coding, skills, _]) = coding_registry();
    let local = create_registry();
    let exclaim = |inner: Arc<PromptSection>| {
        let (key, tag) = (inner.key.clone(), inner.tag);
        let render = move |input, context| {
            let inner = inner.clone();
            async move { Ok((inner.render)(input, context).await?.map(|text| format!("{text}!"))) }
        };
        Ok(section(key, render, tag))
    };
    let override_ = Extension::new("override")
        .section(section("preamble", |_, _| async { Ok(Some("You review.".to_owned())) }, Some(false)))
        .wrap(wrap_section("cwd", exclaim))
        // Instructions are not wrapped.
        .wrap(wrap_section("instructions", |_| Err(DurableError::error("never"))));
    for extension in [coding, skills, override_.into_arc()] {
        local.install(extension).unwrap();
    }
    let state = AgentState { instructions: Some("Be terse.".into()), ..AgentState::default() };
    let (agent, reports) = resolve_reporting(Some(&state), &local.snapshot());
    let expected = [("preamble", "You review."), ("cwd", "/repo!"), ("skills", "S"), ("instructions", "Be terse.")];
    assert_eq!(rendered(&agent).await, expected.map(|(key, text)| (key.to_owned(), Some(text.to_owned()))));
    assert_eq!(agent.sections.last().unwrap().tag, None);
    assert!(reports.is_empty());
}

struct Marker(&'static str);

/// Each hook's extension and marker.
fn markers(agent: &Agent, task: &str) -> Vec<(String, &'static str)> {
    let hooks = agent_hooks(agent, task);
    hooks.iter().map(|hooks| (hooks.extension.clone(), hooks.downcast_ref::<Marker>().unwrap().0)).collect()
}

#[test]
fn collects_hooks_of_the_selected_extensions_in_extension_order_and_applies_field_defaults() {
    let local = create_registry();
    let a = Extension::new("a")
        .hook(hook(&tool_task(), Marker("first")))
        .hook(hook(&generation_task(), Marker("on_yield")));
    local.install(a).unwrap();
    local.install(Extension::new("b").hook(hook(&tool_task(), Marker("second")))).unwrap();
    let none = HarnessSettings::default();
    let select = |names: &[&str]| AgentState {
        extensions: Some(ExtensionSelection::List(strings(names))),
        ..AgentState::default()
    };
    let (a, b) = (("a".to_owned(), "first"), ("b".to_owned(), "second"));
    assert_eq!(markers(&resolve(None, &local.snapshot(), &none), "pi.tool"), [a.clone(), b.clone()]);
    assert_eq!(markers(&resolve(Some(&select(&["b", "a"])), &local.snapshot(), &none), "pi.tool"), [b, a]);
    assert!(markers(&resolve(Some(&select(&["b"])), &local.snapshot(), &none), "pi.generation").is_empty());

    let defaults = resolve(None, &local.snapshot(), &none);
    assert_eq!(
        (defaults.model.clone(), defaults.thinking_level, defaults.cwd.clone()),
        (None, ThinkingLevel::Off, None)
    );
    let state = AgentState {
        model: Some(ModelRef::new("p", "m")),
        thinking_level: Some(ThinkingLevel::High),
        cwd: Some("/w".into()),
        ..AgentState::default()
    };
    let set = resolve(Some(&state), &local.snapshot(), &none);
    assert_eq!(
        (set.model, set.thinking_level, set.cwd),
        (Some(ModelRef::new("p", "m")), ThinkingLevel::High, Some("/w".into()))
    );
}
