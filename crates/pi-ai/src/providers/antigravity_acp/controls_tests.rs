//! Session controls before and between prompts against an in-process agent with a request log: modern and
//! legacy modes, confirmation, busy turns, restarts, and close against a queued open.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::task::Poll;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Notify;

use super::runtime::set_test_agent;
use super::tests::SERIAL;
use super::{API, AntigravityAcp, close_session, open_session, session_info, set_session_option};
use crate::providers::acp::connection::Connection;
use crate::providers::mock_server::{collect, test_model};
use crate::registry::{ApiProvider, StreamOptions};
use crate::types::{Context, Message, Model, ThinkingLevel, Tool};

/// Requests the agent received, in order: method and params.
static LOG: LazyLock<Mutex<Vec<(String, Value)>>> = LazyLock::new(Mutex::default);
/// Ends a prompt whose text contains "wait".
static RELEASE: LazyLock<Notify> = LazyLock::new(Notify::new);
/// Numbers the sessions of every agent connection of a test.
static SESSIONS: AtomicUsize = AtomicUsize::new(0);

#[derive(Clone)]
struct Agent {
    model: String,
    thought: String,
    approval: String,
    mode: String,
}

fn config(agent: &Agent) -> Value {
    let options = |values: &[&str]| -> Vec<Value> { values.iter().map(|v| json!({"value": v, "name": v})).collect() };
    json!([
        {"id": "model", "category": "model", "type": "select", "currentValue": agent.model,
         "options": options(&["gemini-3.8-flash", "gemini-3.1-pro"])},
        {"id": "thought", "category": "thought_level", "type": "select", "currentValue": agent.thought,
         "options": options(&["low", "high"])},
        {"id": "approval", "name": "Approval", "category": "permission", "type": "select",
         "currentValue": agent.approval, "options": options(&["ask", "auto", "stubborn", "gated"])},
    ])
}

fn modes(agent: &Agent) -> Value {
    json!({"currentModeId": agent.mode, "availableModes": [{"id": "plan", "name": "Plan"}, {"id": "code", "name": "Code"}]})
}

async fn agent(stream: tokio::io::DuplexStream) {
    let (read, write) = tokio::io::split(stream);
    let write = Arc::new(tokio::sync::Mutex::new(write));
    let send = move |message: Value| {
        let write = write.clone();
        async move { write.lock().await.write_all(format!("{message}\n").as_bytes()).await.unwrap() }
    };
    let mut lines = BufReader::new(read).lines();
    let mut state =
        Agent { model: "gemini-3.8-flash".into(), thought: "low".into(), approval: "ask".into(), mode: "plan".into() };
    while let Ok(Some(line)) = lines.next_line().await {
        let message: Value = serde_json::from_str(&line).unwrap();
        let (Some(method), id) = (message["method"].as_str(), message["id"].clone()) else { continue };
        let params = message["params"].clone();
        LOG.lock().unwrap().push((method.to_string(), params.clone()));
        let reply = |result: Value| json!({"jsonrpc": "2.0", "id": id, "result": result});
        match method {
            "initialize" => {
                send(reply(json!({"protocolVersion": 1, "agentCapabilities": {
                "promptCapabilities": {"embeddedContext": true, "image": true},
                "_meta": {"antigravity": {"systemPrompt": {"append": true}}}}})))
                .await
            }
            "session/new" => {
                let number = SESSIONS.fetch_add(1, Ordering::SeqCst) + 1;
                let session =
                    json!({"sessionId": format!("s{number}"), "configOptions": config(&state), "modes": modes(&state)});
                send(reply(session)).await;
            }
            "session/set_config_option" => {
                let value = params["value"].as_str().unwrap().to_string();
                let gated = value == "gated";
                match params["configId"].as_str().unwrap() {
                    "model" => state.model = value,
                    "thought" => state.thought = value,
                    _ if value == "stubborn" => {}
                    _ => state.approval = value,
                }
                let (send, answer) = (send.clone(), reply(json!({"configOptions": config(&state)})));
                tokio::spawn(async move {
                    if gated {
                        RELEASE.notified().await;
                    }
                    send(answer).await;
                });
            }
            "session/set_mode" => {
                state.mode = params["modeId"].as_str().unwrap().to_string();
                send(reply(json!({}))).await;
            }
            "session/prompt" => {
                let (send, wait) = (send.clone(), params["prompt"].to_string().contains("wait"));
                tokio::spawn(async move {
                    if wait {
                        RELEASE.notified().await;
                    }
                    send(json!({"jsonrpc": "2.0", "id": id, "result": {"stopReason": "end_turn"}})).await;
                });
            }
            _ => {}
        }
    }
}

fn install() {
    LOG.lock().unwrap().clear();
    SESSIONS.store(0, Ordering::SeqCst);
    set_test_agent(Some(Arc::new(|| {
        let (client, agent_side) = tokio::io::duplex(1 << 20);
        tokio::spawn(agent(agent_side));
        let (read, write) = tokio::io::split(client);
        Connection::new(super::LABEL, read, write)
    })));
}

fn methods() -> Vec<String> {
    LOG.lock()
        .unwrap()
        .iter()
        .map(|(method, params)| match method.as_str() {
            "session/set_config_option" => format!("set {}={}", params["configId"].as_str().unwrap(), params["value"]),
            "session/set_mode" => format!("mode {}", params["modeId"]),
            other => other.to_string(),
        })
        .collect()
}

fn model() -> Model {
    let mut model = test_model(API, "google-antigravity", "acp://antigravity");
    model.id = "gemini-3.1-pro".into();
    model.thinking_level_map =
        Some([("low".to_string(), Some("low".to_string())), ("high".to_string(), Some("high".to_string()))].into());
    model
}

fn context(tool: &str, text: &str) -> Context {
    let tool = Tool::new(tool, "A tool", json!({"type": "object"}));
    Context { system_prompt: Some("Be brief".into()), messages: vec![Message::user(text)], tools: vec![tool] }
}

async fn turn(pi_session: &str, context: &Context) -> Option<String> {
    let options = StreamOptions {
        api_key: Some("antigravity-acp".into()),
        session_id: Some(pi_session.into()),
        reasoning: Some(ThinkingLevel::High),
        ..StreamOptions::default()
    };
    collect(AntigravityAcp.stream(&model(), context, options)).await.1.error_message
}

#[tokio::test]
async fn the_first_prompt_reuses_the_session_opened_for_its_context() {
    let _serial = SERIAL.lock().await;
    install();
    let mut stale = model();
    stale.thinking_level_map =
        Some([("low", "low"), ("medium", "medium")].map(|(k, v)| (k.to_string(), Some(v.to_string()))).into());
    let info = open_session("reuse", &context("read", "hi"), &stale, ThinkingLevel::Medium).await.unwrap();
    assert_eq!((info.acp_session.as_str(), info.pi_model.as_str()), ("s1", "gemini-3.1-pro"));
    let thought = info.selectors.iter().find(|selector| selector.category == "thought_level").unwrap();
    let offered: Vec<&str> = thought.choices.iter().map(|choice| choice.id.as_str()).collect();
    assert_eq!((offered, thought.current.as_deref()), (vec!["low", "high"], Some("high")), "medium clamps to high");
    assert_eq!(methods(), ["initialize", "session/new", "set model=\"gemini-3.1-pro\"", "set thought=\"high\""]);
    assert_eq!(turn("reuse", &context("read", "hi")).await, None);
    assert_eq!(methods()[4..], ["session/prompt"], "no second session/new and no repeated set");
    let prompt = LOG.lock().unwrap().last().unwrap().1.clone();
    assert_eq!(
        (prompt["sessionId"].clone(), prompt["prompt"].clone()),
        (json!("s1"), json!([{"type": "text", "text": "\n\nhi"}]))
    );
    close_session("reuse").await;
}

#[tokio::test]
async fn confirmed_modes_and_settings_apply_before_the_prompt_of_a_restarted_session() {
    let _serial = SERIAL.lock().await;
    install();
    open_session("modes", &context("read", "hi"), &model(), ThinkingLevel::High).await.unwrap();
    let info = set_session_option("modes", "approval", false, "auto").await.unwrap();
    let approval = info.selectors.iter().find(|selector| selector.id == "approval").unwrap();
    assert_eq!(approval.current.as_deref(), Some("auto"));
    let info = set_session_option("modes", "mode", true, "code").await.unwrap();
    assert!(info.selectors.iter().any(|s| s.legacy_mode && s.current.as_deref() == Some("code")));
    let sent = methods().len();
    let kept = set_session_option("modes", "approval", false, "stubborn").await.unwrap_err();
    assert!(kept.contains("kept Approval at auto"), "{kept}");
    let unknown = set_session_option("modes", "mode", true, "yolo").await.unwrap_err();
    assert!(unknown.contains("does not offer"), "{unknown}");
    assert_eq!(methods().len(), sent + 1, "an unoffered value is not sent");
    assert_eq!(turn("modes", &context("write", "hi")).await, None, "a new tool restarts the session");
    let after: Vec<String> = methods().split_off(sent + 1);
    assert_eq!(
        after,
        [
            "initialize",
            "session/new",
            "set approval=\"auto\"",
            "mode \"code\"",
            "set model=\"gemini-3.1-pro\"",
            "set thought=\"high\"",
            "session/prompt"
        ]
    );
    close_session("modes").await;
}

#[tokio::test]
async fn thinking_is_set_by_the_agent_and_a_turn_refuses_changes() {
    let _serial = SERIAL.lock().await;
    install();
    open_session("busy", &context("read", "hi"), &model(), ThinkingLevel::High).await.unwrap();
    let info = set_session_option("busy", "thought", false, "low").await.unwrap();
    let thought = info.selectors.iter().find(|selector| selector.category == "thought_level").unwrap();
    assert_eq!(thought.current.as_deref(), Some("low"));
    assert!(set_session_option("busy", "thought", false, "medium").await.unwrap_err().contains("does not offer"));
    let running = tokio::spawn(async { turn("busy", &context("read", "wait")).await });
    while !methods().iter().any(|method| method == "session/prompt") {
        tokio::task::yield_now().await;
    }
    let busy = set_session_option("busy", "approval", false, "auto").await.unwrap_err();
    assert_eq!(busy, "Wait until the agent turn ends");
    assert!(!methods().iter().any(|method| method.starts_with("set approval")), "nothing is sent during a turn");
    RELEASE.notify_one();
    assert_eq!(running.await.unwrap(), None);
    close_session("busy").await;
}

#[tokio::test]
async fn close_stops_a_queued_open_and_a_later_open_starts_a_new_session() {
    let _serial = SERIAL.lock().await;
    install();
    open_session("close", &context("read", "hi"), &model(), ThinkingLevel::High).await.unwrap();
    set_session_option("close", "approval", false, "auto").await.unwrap();
    let running = tokio::spawn(async { turn("close", &context("read", "wait")).await });
    while !methods().iter().any(|method| method == "session/prompt") {
        tokio::task::yield_now().await;
    }
    let (request, selected_model) = (context("read", "hi"), model());
    let queued = open_session("close", &request, &selected_model, ThinkingLevel::High);
    tokio::pin!(queued);
    assert!(matches!(futures::poll!(&mut queued), Poll::Pending));
    let closing = close_session("close");
    tokio::pin!(closing);
    assert!(matches!(futures::poll!(&mut closing), Poll::Pending));
    assert!(session_info("close").is_none(), "the slot is removed before the turn ends");
    RELEASE.notify_one();
    assert_eq!(running.await.unwrap(), None);
    assert!(queued.await.unwrap_err().contains("was closed"));
    closing.await;
    assert!(session_info("close").is_none());
    assert_eq!(
        methods().iter().filter(|method| *method == "session/new").count(),
        1,
        "the queued open started nothing"
    );
    let reopened = open_session("close", &context("read", "hi"), &model(), ThinkingLevel::High).await.unwrap();
    assert_eq!(reopened.acp_session, "s2");
    let approval = reopened.selectors.iter().find(|selector| selector.id == "approval").unwrap();
    assert_eq!(approval.current.as_deref(), Some("ask"), "the closed slot's choices are gone");
    close_session("close").await;
}

#[tokio::test]
async fn a_set_that_a_close_overtakes_reports_the_close_and_keeps_nothing() {
    let _serial = SERIAL.lock().await;
    install();
    open_session("race", &context("read", "hi"), &model(), ThinkingLevel::High).await.unwrap();
    let setting = tokio::spawn(set_session_option("race", "approval", false, "gated"));
    while !methods().iter().any(|method| method == r#"set approval="gated""#) {
        tokio::task::yield_now().await;
    }
    let closing = close_session("race");
    tokio::pin!(closing);
    assert!(matches!(futures::poll!(&mut closing), Poll::Pending));
    assert!(session_info("race").is_none(), "the slot is removed before the reply");
    RELEASE.notify_one();
    assert!(
        setting.await.unwrap().unwrap_err().contains("was closed"),
        "a set that the close overtook is not confirmed"
    );
    closing.await;
    assert!(session_info("race").is_none());
    let reopened = open_session("race", &context("read", "hi"), &model(), ThinkingLevel::High).await.unwrap();
    let approval = reopened.selectors.iter().find(|selector| selector.id == "approval").unwrap();
    assert_eq!(approval.current.as_deref(), Some("ask"), "nothing from the closed slot is reapplied");
    close_session("race").await;
}
