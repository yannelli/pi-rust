//! `--max-turns` and `--max-cost` (pi-rust run limits) over the faux provider.

use std::sync::Arc;

use pi_ai::providers::faux::FauxResponse;
use pi_coding_agent::agent_session::{PromptDisposition, RUN_LIMIT_EXIT_CODE, RunLimitKind, RunLimits};
use pi_coding_agent::app::{AppOptions, create_app_runtime};
use pi_coding_agent::cli::OutputMode;
use pi_coding_agent::print_mode::run_print_mode;
use pi_coding_agent::session::SessionManager;
use serde_json::json;

use crate::harness::{Harness, Options, harness_with, open_session_with, roles, text};

fn read_call(id: &str) -> FauxResponse {
    FauxResponse::tool_call(id, "read", json!({"path": "notes.txt"}))
}

/// Five tool-calling responses, then a final answer.
fn tool_script() -> Vec<FauxResponse> {
    let mut responses: Vec<FauxResponse> = (1..=5).map(|turn| read_call(&format!("r{turn}"))).collect();
    responses.push(FauxResponse::text("All read."));
    responses
}

async fn limited(limits: RunLimits, responses: Vec<FauxResponse>, cost: serde_json::Value) -> Harness {
    let options = Options { responses, files: vec![("notes.txt", "hello")], tiny: cost, ..Options::default() };
    let h = harness_with(options, |create| create.run_limits = limits).await;
    assert_eq!(h.session.model().unwrap().id, "tiny", "models.json loads");
    h
}

/// The roles of the transcript stored in the session file.
fn reopen_roles(h: &Harness) -> Vec<String> {
    let file = h.session.session_file().unwrap();
    let manager = SessionManager::open(&file).unwrap();
    let messages = manager.build_session_context().messages;
    roles(&messages).into_iter().map(String::from).collect()
}

#[tokio::test]
async fn max_turns_stops_a_tool_loop_after_n_turns_and_the_session_resumes() {
    let limits = RunLimits { max_turns: Some(2), max_cost: None };
    let h = limited(limits, tool_script(), json!({})).await;
    let disposition = h.session.prompt("Read notes.txt over and over.", Vec::new()).await.unwrap();
    assert_eq!(disposition, PromptDisposition::Started);
    assert_eq!(h.requests().len(), 2, "the third request is never sent");

    let stop = h.session.run_limit_stop().unwrap();
    assert_eq!((stop.kind, stop.turns), (RunLimitKind::Turns, 2));
    assert_eq!(stop.message(), "stopped after 2 turns (--max-turns)");
    let events = h.events_of("run_limit");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["limit"], "maxTurns");
    assert_eq!((events[0]["max"].as_u64(), events[0]["turns"].as_u64()), (Some(2), Some(2)));
    assert_eq!(h.events_of("agent_settled").last().unwrap()["aborted"], true);
    let messages = h.session.messages();
    assert_eq!(roles(&messages)[1..], ["user", "assistant", "toolResult", "assistant", "toolResult"]);
    assert!(h.session.is_idle());

    // The stored session reloads, and a new prompt counts its turns from zero.
    assert_eq!(reopen_roles(&h)[1..], ["user", "assistant", "toolResult", "assistant", "toolResult"]);
    let manager = SessionManager::open(&h.session.session_file().unwrap()).unwrap();
    let resumed = open_session_with(&h.services, manager, |create| create.run_limits = limits).await;
    resumed.prompt("Go on.", Vec::new()).await.unwrap();
    let requests = h.requests();
    assert_eq!(requests.len(), 4);
    assert_eq!(text(requests[2].messages.last().unwrap()), "Go on.");
    assert_eq!(resumed.run_limit_stop().unwrap().turns, 2);
}

#[tokio::test]
async fn a_run_that_ends_by_itself_within_the_limit_is_not_stopped() {
    let limits = RunLimits { max_turns: Some(2), max_cost: Some(1000.0) };
    let responses = vec![read_call("r1"), FauxResponse::text("Done.")];
    let h = limited(limits, responses, json!({})).await;
    h.session.prompt("Read notes.txt once.", Vec::new()).await.unwrap();
    assert_eq!(h.requests().len(), 2);
    assert!(h.session.run_limit_stop().is_none());
    assert!(h.events_of("run_limit").is_empty());
    assert_eq!(h.events_of("agent_settled").last().unwrap()["aborted"], false);
}

#[tokio::test]
async fn max_cost_stops_once_the_session_cost_reaches_the_limit() {
    // One dollar per input token: the first response alone exceeds the limit.
    let cost = json!({"cost": {"input": 1_000_000.0, "output": 0.0, "cacheRead": 0.0, "cacheWrite": 0.0}});
    let limits = RunLimits { max_turns: None, max_cost: Some(0.5) };
    let h = limited(limits, tool_script(), cost).await;
    h.session.prompt("Read notes.txt over and over.", Vec::new()).await.unwrap();
    assert_eq!(h.requests().len(), 1);
    let stop = h.session.run_limit_stop().unwrap();
    assert_eq!(stop.kind, RunLimitKind::Cost);
    assert!(stop.cost >= 0.5);
    assert!(stop.message().ends_with("session cost (--max-cost 0.5)"), "{}", stop.message());
    let session_cost = h.session.session_manager().session_stats().cost;
    assert_eq!(stop.cost, session_cost, "the cost is the footer total");
    let event = h.events_of("run_limit").pop().unwrap();
    assert_eq!((event["limit"].as_str(), event["max"].as_f64()), (Some("maxCost"), Some(0.5)));

    // A session at the limit sends no further prompt.
    let disposition = h.session.prompt("And again.", Vec::new()).await.unwrap();
    assert_eq!(disposition, PromptDisposition::Handled);
    assert_eq!(h.requests().len(), 1);
    assert_eq!(h.events_of("run_limit").len(), 2);
}

#[tokio::test]
async fn print_mode_exits_3_and_skips_the_remaining_messages() {
    let limits = RunLimits { max_turns: Some(1), max_cost: None };
    let h = limited(RunLimits::default(), tool_script(), json!({})).await;
    let options = AppOptions { run_limits: limits, ..h.app_options() };
    let manager = SessionManager::create(&h.cwd, Some(&h.agent_dir.join("sessions")));
    let runtime = Arc::new(create_app_runtime(options, manager).await.unwrap());
    let session = runtime.session();
    assert_eq!(session.model().unwrap().id, "tiny");
    let first = Some("Read notes.txt.".to_string());
    let code = run_print_mode(&runtime, OutputMode::Text, first, Vec::new(), vec!["Second.".into()]).await;
    assert_eq!(code, RUN_LIMIT_EXIT_CODE);
    assert_eq!(h.requests().len(), 1, "the second message is not sent");
    assert_eq!(session.run_limit_stop().unwrap().message(), "stopped after 1 turn (--max-turns)");
}
