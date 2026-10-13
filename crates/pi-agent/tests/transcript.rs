//! Tool loadout declarations and transcript state, ported from upstream `agent.test.ts`
//! and `agent-loop.test.ts`.

mod support;

use pi_agent::transcript::{declared_tools, hide_tool_declarations};
use pi_agent::{Agent, AgentContext, AgentHooks, AgentInitialState, SharedTool, run_agent_loop};
use pi_ai::{Message, SystemMessage, Tool, ToolReference, UserContent};
use serde_json::{Value, json};
use support::*;
use tokio_util::sync::CancellationToken;

fn tool(name: &str) -> SharedTool {
    FnTool::new(name, json!({"type": "object", "properties": {}}), |_, _| Ok(pi_agent::AgentToolResult::text("ok")))
}

fn system(value: Value) -> Message {
    serde_json::from_value(value).unwrap()
}

fn names(tools: &[Tool]) -> Vec<String> {
    tools.iter().map(|tool| tool.name.clone()).collect()
}

/// `+added` and `-removed` for every system message in a request.
fn declarations(messages: &[Message]) -> Vec<String> {
    messages
        .iter()
        .filter_map(|message| match message {
            Message::System(system) => Some(system),
            _ => None,
        })
        .flat_map(|system| {
            let added: Vec<String> = system.tools_added.iter().flatten().map(|tool| tool.name.clone()).collect();
            let removed: Vec<String> = system.tools_removed.iter().flatten().map(|tool| tool.name.clone()).collect();
            [format!("+{}", added.join(",")), format!("-{}", removed.join(","))]
        })
        .collect()
}

fn agent_with(stream_fn: pi_agent::StreamFn, initial: AgentInitialState) -> Agent {
    let agent = Agent::with_initial_state(AgentInitialState { model: Some(model()), ..initial });
    agent.set_hooks(AgentHooks { stream_fn: Some(stream_fn), ..AgentHooks::default() });
    agent
}

#[test]
fn initial_state_seeds_the_leading_system_message() {
    let agent = Agent::with_initial_state(AgentInitialState {
        system_prompt: Some("You are helpful.".into()),
        tools: vec![tool("echo")],
        thinking_level: pi_ai::ThinkingLevel::Low,
        ..Default::default()
    });
    let state = agent.state();
    let Some(Message::System(initial)) = state.messages.first() else { panic!("expected a system message") };
    assert_eq!(initial.content.text(), "You are helpful.");
    assert_eq!(initial.timestamp, 0);
    assert_eq!(names(initial.tools_added.as_ref().unwrap()), ["echo"]);
    assert_eq!(state.current_system_prompt(), "You are helpful.");
    assert_eq!(state.thinking_level, pi_ai::ThinkingLevel::Low);
    assert_eq!(state.model.id, "unknown");

    let plain = Agent::with_initial_state(AgentInitialState::default());
    assert!(plain.state().messages.is_empty());
}

#[tokio::test]
async fn provider_context_comes_only_from_the_transcript() {
    let (requests, stream_fn) = sequence(vec![]);
    let initial = system(json!({"role": "system", "content": "Transcript prompt", "toolsAdded": [], "timestamp": 1}));
    let mut context = AgentContext::default();
    let (_, sink) = recorder();
    run_agent_loop(
        vec![initial.clone(), Message::user("Hello")],
        &mut context,
        &config(stream_fn),
        sink,
        CancellationToken::new(),
    )
    .await;
    let request = requests.lock().unwrap()[0].context.clone();
    assert_eq!(request.system_prompt, None);
    assert!(request.tools.is_empty());
    assert_eq!(request.messages[0], initial);
}

#[tokio::test]
async fn declares_tool_loadout_changes_before_the_next_request() {
    let (requests, stream_fn) = sequence(vec![]);
    let agent = agent_with(
        stream_fn,
        AgentInitialState {
            system_prompt: Some("You are helpful.".into()),
            tools: vec![tool("first")],
            ..Default::default()
        },
    );
    agent.prompt("one", vec![]).await.unwrap();
    agent.update_state(|state| state.tools = vec![tool("second")]);
    agent.prompt("two", vec![]).await.unwrap();
    agent.prompt("three", vec![]).await.unwrap();
    let seen: Vec<Vec<String>> =
        requests.lock().unwrap().iter().map(|request| declarations(&request.context.messages)).collect();
    assert_eq!(
        seen,
        [vec!["+first", "-"], vec!["+first", "-", "+second", "-first"], vec!["+first", "-", "+second", "-first"]]
    );
    let state = agent.state();
    let update = state.messages.iter().find_map(|message| match message {
        Message::System(system) if system.tools_removed.is_some() => Some(system.clone()),
        _ => None,
    });
    let update = update.expect("tool update message");
    assert_eq!(update.content.text(), "");
    assert_eq!(names(update.tools_added.as_ref().unwrap()), ["second"]);
    assert_eq!(update.tools_removed, Some(vec![ToolReference { name: "first".into() }]));
    assert_eq!(update.tools_added.as_ref().unwrap()[0].description, "test tool");
}

#[tokio::test]
async fn merges_tool_changes_into_a_pending_system_message() {
    let (requests, stream_fn) = sequence(vec![]);
    let agent = agent_with(
        stream_fn,
        AgentInitialState { system_prompt: Some("You are helpful.".into()), ..Default::default() },
    );
    agent.update_state(|state| state.tools = vec![tool("echo")]);
    let pending =
        system(json!({"role": "system", "content": "", "sections": {"skills": "<skills>x</skills>"}, "timestamp": 1}));
    agent.prompt_messages(vec![pending, Message::user("hi")]).await.unwrap();
    let request = requests.lock().unwrap()[0].context.clone();
    assert_eq!(request.messages.iter().filter(|message| matches!(message, Message::System(_))).count(), 2);
    let Message::System(merged) = &agent.state().messages[1] else { panic!("expected a system message") };
    assert_eq!(merged.sections.as_ref().unwrap()["skills"], "<skills>x</skills>");
    assert_eq!(names(merged.tools_added.as_ref().unwrap()), ["echo"]);
    assert_eq!((merged.tools_removed.clone(), merged.timestamp), (None, 1));
}

#[tokio::test]
async fn pending_declarations_are_rewritten_to_match_the_executable_set() {
    let (_, stream_fn) = sequence(vec![]);
    let agent = agent_with(
        stream_fn,
        AgentInitialState {
            system_prompt: Some("You are helpful.".into()),
            tools: vec![tool("first")],
            ..Default::default()
        },
    );
    // The message claims to add `second` and remove `first`; the executable set wins.
    let pending = system(json!({
        "role": "system", "content": "", "sections": {"note": "<note>x</note>"},
        "toolsAdded": [{"name": "second", "description": "test tool", "parameters": {"type": "object", "properties": {}}}],
        "toolsRemoved": [{"name": "first"}], "timestamp": 1
    }));
    agent.prompt_messages(vec![pending, Message::user("hi")]).await.unwrap();
    let state = agent.state();
    let expected = SystemMessage {
        content: UserContent::Text(String::new()),
        sections: Some(json!({"note": "<note>x</note>"}).as_object().unwrap().clone()),
        tools_added: None,
        tools_removed: None,
        timestamp: 1,
    };
    assert_eq!(state.messages[1], Message::System(expected));
    assert_eq!(names(&declared_tools(&state.messages)), ["first"]);
}

#[test]
fn reset_restores_the_transcript_baseline() {
    let agent = Agent::with_initial_state(AgentInitialState {
        system_prompt: Some("You are helpful.".into()),
        tools: vec![tool("echo")],
        messages: vec![Message::user("old")],
        ..Default::default()
    });
    agent.follow_up(Message::user("queued"));
    agent.reset().unwrap();
    let state = agent.state();
    assert_eq!(state.messages.len(), 1);
    let Message::System(initial) = &state.messages[0] else { panic!("expected a system message") };
    assert_eq!(initial.content.text(), "You are helpful.");
    assert_eq!(names(initial.tools_added.as_ref().unwrap()), ["echo"]);
    assert!(!agent.has_queued_messages());
}

#[test]
fn hidden_declarations_are_projected_out_of_requests() {
    let messages = vec![
        system(json!({"role": "system", "content": "p", "toolsAdded": [
            {"name": "read", "description": "d", "parameters": {}}, {"name": "codemode", "description": "d", "parameters": {}}
        ], "timestamp": 0})),
        system(json!({"role": "system", "content": "", "toolsRemoved": [{"name": "read"}], "timestamp": 1})),
        Message::user("hi"),
    ];
    let projected = hide_tool_declarations(messages.clone(), &["read".to_string()]);
    assert_eq!(declarations(&projected), ["+codemode", "-", "+", "-"]);
    assert_eq!(hide_tool_declarations(messages.clone(), &[]), messages);
}

#[tokio::test]
async fn legacy_contexts_with_a_system_prompt_send_prompt_and_tools_beside_messages() {
    let (requests, stream_fn) = sequence(vec![]);
    let mut context = AgentContext { system_prompt: Some("p".into()), tools: vec![tool("echo")], ..Default::default() };
    let (_, sink) = recorder();
    let added =
        run_agent_loop(vec![Message::user("hi")], &mut context, &config(stream_fn), sink, CancellationToken::new())
            .await;
    assert_eq!(roles(&added), ["user", "assistant"]);
    let request = requests.lock().unwrap()[0].context.clone();
    assert_eq!(request.system_prompt.as_deref(), Some("p"));
    assert_eq!(names(&request.tools), ["echo"]);
}
