//! Prompting, queues, and the transcript system prompt (upstream `agent-session-prompt`,
//! `agent-session-queue`, `agent-session-concurrent`, and `agent-session-retry-events` tests).

use std::sync::Arc;
use std::time::Duration;

use pi_ai::providers::faux::FauxResponse;
use pi_ai::{ContentBlock, Message, StopReason};
use pi_coding_agent::agent_session::{PromptDisposition, PromptOptions, StreamingBehavior};
use pi_coding_agent::model_resolver::ScopedModel;
use pi_coding_agent::session::SessionManager;
use serde_json::json;

use crate::harness::{Options, event_order, harness, open_session, roles, text, wait_until};

#[tokio::test]
async fn runs_tools_persists_and_resumes_sessions() {
    let h = harness(Options {
        responses: vec![
            FauxResponse::tool_call("w1", "write", json!({"path": "notes.txt", "content": "meeting on Friday"})),
            FauxResponse::tool_call("r1", "read", json!({"path": "notes.txt"})),
            FauxResponse::text("The meeting is on Friday."),
            FauxResponse::text("Still Friday."),
        ],
        files: vec![("AGENTS.md", "Project rule: be brief.")],
        ..Options::default()
    })
    .await;
    let disposition = h.session.prompt("Write notes.txt, then read it back.", Vec::new()).await.unwrap();
    assert_eq!(disposition, PromptDisposition::Started);

    assert_eq!(std::fs::read_to_string(h.cwd.join("notes.txt")).unwrap(), "meeting on Friday");
    let requests = h.requests();
    let system = requests[0].system_prompt.clone().unwrap();
    assert!(system.contains("Project rule: be brief.") && system.contains("- read: "));
    let tools: Vec<&str> = requests[0].tools.iter().map(|tool| tool.name.as_str()).collect();
    assert_eq!(tools, ["read", "bash", "edit", "write"]);
    assert!(text(requests[2].messages.last().unwrap()).contains("meeting on Friday"));
    let messages = h.session.messages();
    assert_eq!(roles(&messages), ["system", "user", "assistant", "toolResult", "assistant", "toolResult", "assistant"]);
    let Message::System(leading) = &messages[0] else { panic!("leading system message") };
    let sections = leading.sections.as_ref().unwrap();
    assert!(sections.contains_key("tools") && sections.contains_key("project_context"));
    assert!(leading.tools_added.as_ref().is_some_and(|tools| tools.len() == 4));

    let file = h.session.session_file().unwrap();
    let resumed = open_session(&h.services, SessionManager::open(&file).unwrap()).await;
    assert_eq!(resumed.model().unwrap().id, "tiny");
    assert_eq!(resumed.messages().len(), 7);
    resumed.prompt("Which day?", Vec::new()).await.unwrap();
    let last = h.requests().last().unwrap().clone();
    assert_eq!(last.messages.len(), 7, "the unchanged prompt adds no system update");
}

#[tokio::test]
async fn emits_upstream_event_order_for_prompts_and_tool_turns() {
    let h = harness(Options {
        responses: vec![
            FauxResponse::text("hello"),
            FauxResponse::tool_call("c1", "ls", json!({})),
            FauxResponse::text("done"),
        ],
        ..Options::default()
    })
    .await;
    h.session.prompt("hi", Vec::new()).await.unwrap();
    let single = [
        "agent_start",
        "turn_start",
        "message_start:system",
        "message_end:system",
        "message_start:user",
        "message_end:user",
        "message_start:assistant",
        "message_update",
        "message_end:assistant",
        "turn_end",
        "agent_end",
        "agent_settled",
    ];
    assert_eq!(event_order(&h.events()), single);

    h.events.lock().unwrap().clear();
    h.session.set_active_tools_by_name(&["read".into(), "ls".into()]);
    h.session.prompt("list", Vec::new()).await.unwrap();
    let order = event_order(&h.events());
    assert_eq!(
        order,
        [
            "agent_start",
            "turn_start",
            "message_start:system",
            "message_end:system",
            "message_start:user",
            "message_end:user",
            "message_start:assistant",
            "message_update",
            "message_end:assistant",
            "tool_execution_start:ls",
            "tool_execution_end:ls",
            "message_start:toolResult",
            "message_end:toolResult",
            "turn_end",
            "turn_start",
            "message_start:assistant",
            "message_update",
            "message_end:assistant",
            "turn_end",
            "agent_end",
            "agent_settled",
        ]
    );
    let update = h.events().into_iter().find(|event| event["type"] == "message_end").unwrap();
    let system = &update["message"];
    assert_eq!(system["role"], "system");
    let removed: Vec<&str> =
        system["toolsRemoved"].as_array().unwrap().iter().map(|tool| tool["name"].as_str().unwrap()).collect();
    let added: Vec<&str> =
        system["toolsAdded"].as_array().unwrap().iter().map(|tool| tool["name"].as_str().unwrap()).collect();
    assert_eq!((removed, added), (vec!["bash", "edit", "write"], vec!["ls"]));
}

/// A 1x1 PNG.
const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";

#[tokio::test]
async fn preserves_images_and_rejects_prompts_while_streaming() {
    let h = harness(Options {
        responses: vec![FauxResponse::text("x".repeat(400)), FauxResponse::text("after steer")],
        delay: Some(Duration::from_millis(2)),
        ..Options::default()
    })
    .await;
    let image = ContentBlock::Image { data: PNG.into(), mime_type: "image/png".into() };
    let session = h.session.clone();
    let running = tokio::spawn(async move { session.prompt("look", vec![image]).await });
    wait_until(|| h.session.is_streaming() && !h.events_of("message_update").is_empty()).await;

    let error = h.session.prompt("again", Vec::new()).await.unwrap_err();
    assert_eq!(
        error.to_string(),
        "Agent is already processing. Specify streamingBehavior ('steer' or 'followUp') to queue the message."
    );
    let queued = PromptOptions { streaming_behavior: Some(StreamingBehavior::Steer), ..PromptOptions::default() };
    assert_eq!(h.session.prompt_with("steered", queued).await.unwrap(), PromptDisposition::Queued);
    assert_eq!(h.session.steering_messages(), ["steered"]);
    assert_eq!(h.session.pending_message_count(), 1);
    running.await.unwrap().unwrap();

    let requests = h.requests();
    let Some(Message::User(user)) = requests[0].messages.iter().find(|m| matches!(m, Message::User(_))) else {
        panic!("user message")
    };
    let pi_ai::UserContent::Blocks(blocks) = &user.content else { panic!("blocks") };
    assert!(blocks.iter().any(|block| matches!(block, ContentBlock::Image { .. })));
    assert_eq!(text(requests[1].messages.last().unwrap()), "steered");
    let updates = h.events_of("queue_update");
    assert_eq!(updates.first().unwrap()["steering"], json!(["steered"]));
    assert_eq!(updates.last().unwrap()["steering"], json!([]));
    assert_eq!(h.session.pending_message_count(), 0);
}

#[tokio::test]
async fn delivers_follow_ups_after_the_run_and_aborts_cleanly() {
    let h = harness(Options {
        responses: vec![FauxResponse::text("first"), FauxResponse::text("second"), FauxResponse::Hang],
        delay: Some(Duration::from_millis(5)),
        ..Options::default()
    })
    .await;
    let session = h.session.clone();
    let running = tokio::spawn(async move { session.prompt("one", Vec::new()).await });
    wait_until(|| h.session.is_streaming()).await;
    h.session.follow_up("two", Vec::new());
    running.await.unwrap().unwrap();
    let users: Vec<String> = h.session.messages().iter().filter(|m| m.role() == "user").map(text).collect();
    assert_eq!(users, ["one", "two"]);

    let session = h.session.clone();
    let hanging = tokio::spawn(async move { session.prompt("three", Vec::new()).await });
    wait_until(|| h.requests().len() == 3).await;
    h.session.abort().await;
    hanging.await.unwrap().unwrap();
    assert!(h.session.is_idle());
    let Some(Message::Assistant(last)) = h.session.messages().pop() else { panic!("assistant") };
    assert_eq!(last.stop_reason, StopReason::Aborted);
    assert_eq!(h.events().last().unwrap(), &json!({"type": "agent_settled", "aborted": true}));
    let entries = h.entries();
    assert!(entries.iter().any(|entry| entry["message"]["stopReason"] == "aborted"));
}

#[tokio::test]
async fn expands_skills_and_prompt_templates() {
    let skill = "---\nname: deploy\ndescription: Deploy the app\n---\nRun the deploy script.";
    let h = harness(Options {
        responses: vec![FauxResponse::text("ok"), FauxResponse::text("ok")],
        files: vec![(".pi/skills/deploy/SKILL.md", skill), (".pi/prompts/review.md", "Review $1 carefully.")],
        ..Options::default()
    })
    .await;
    h.session.prompt("/skill:deploy now", Vec::new()).await.unwrap();
    h.session.prompt("/review main.rs", Vec::new()).await.unwrap();
    let requests = h.requests();
    let first = text(requests[0].messages.last().unwrap());
    assert!(first.contains("Run the deploy script.") && first.contains("now"), "{first}");
    assert_eq!(text(requests[1].messages.last().unwrap()), "Review main.rs carefully.");
    assert!(requests[0].system_prompt.as_deref().unwrap().contains("deploy"));
}

#[tokio::test]
async fn switches_models_and_thinking_levels_with_events() {
    let h = harness(Options::default()).await;
    let session = Arc::clone(&h.session);
    assert_eq!(session.thinking_level().as_str(), "medium");
    session.set_thinking_level(pi_ai::ThinkingLevel::High, false);
    assert_eq!(h.events_of("thinking_level_changed"), [json!({"type": "thinking_level_changed", "level": "high"})]);
    assert_eq!(session.cycle_thinking_level(false).unwrap().as_str(), "off", "xhigh is not offered, so it wraps");

    let other = session.model_runtime().get_model("local", "other").unwrap();
    session.set_model(other.clone(), false).await.unwrap();
    assert_eq!(session.model().unwrap().id, "other");
    assert_eq!(session.thinking_level().as_str(), "off", "a model without reasoning clamps to off");
    let mut missing = other;
    missing.provider = "nowhere".into();
    let error = session.set_model(missing, false).await.unwrap_err();
    assert_eq!(error.to_string(), "No API key for nowhere/other");
    let runtime = session.model_runtime().clone();
    let scoped = ["tiny", "other"]
        .map(|id| ScopedModel { model: runtime.get_model("local", id).unwrap(), thinking_level: None });
    session.set_scoped_models(scoped.to_vec());
    let cycled = session.cycle_model(false, false).unwrap();
    assert_eq!(cycled.model.id, "tiny");
    let entries = h.entries();
    let kinds: Vec<&str> = entries.iter().filter_map(|entry| entry["type"].as_str()).collect();
    assert!(kinds.iter().filter(|kind| **kind == "model_change").count() >= 3);
    assert!(kinds.contains(&"thinking_level_change"));
}
