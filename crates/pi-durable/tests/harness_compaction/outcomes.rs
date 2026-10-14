//! The upstream "compaction outcomes" cases.

use pi_ai::ThinkingLevel;
use pi_durable::harness::compaction::{CompactionDecision, CompactionHooks, CompactionSelection, compaction_task};
use pi_durable::harness::usage::{PROVIDER_DOC, USAGE_DOC};
use pi_durable::models::DeferredOption;

use super::chat::*;

pub type Decision = pi_durable::DurableResult<Option<CompactionDecision>>;

/// Install a beforeCompact hook under `name`.
pub fn before_compact(
    chat: &Chat,
    name: &str,
    decide: impl Fn(CompactionSelection) -> Decision + Send + Sync + 'static,
) {
    let hooks = CompactionHooks {
        before_compact: Some(Arc::new(move |selection, _, _| {
            let decision = decide(selection);
            Box::pin(async move { decision })
        })),
    };
    chat.setup.registry.install(Extension::new(name).hook(hook(&compaction_task(), hooks))).unwrap();
}

#[tokio::test]
async fn completes_without_a_summary_when_there_is_nothing_to_compact() {
    let chat = open(OpenOptions::default()).await;
    chat.turn("hi", "hello").await;
    let task = chat.compact(None).await;
    assert_eq!(chat.outcome(task).await, json!({ "status": "completed", "result": {} }));
    assert!(chat.faux.summary_requests().is_empty());
    assert!(chat.live().await.get("compactions").is_none());
    chat.close().await;
}

#[tokio::test]
async fn asks_before_compact_the_first_decision_wins_a_throw_is_reported_and_skipped() {
    let chat = open(OpenOptions::default()).await;
    let seen = Arc::new(Mutex::new(Vec::new()));
    before_compact(&chat, "broken", |_| Err(pi_durable::DurableError::error("hook broke")));
    let sink = seen.clone();
    before_compact(&chat, "supplies", move |selection| {
        sink.lock().unwrap().push(selection);
        Ok(Some(CompactionDecision::Summary("FROM HOOK".into())))
    });
    before_compact(&chat, "declines", |_| Ok(Some(CompactionDecision::Decline)));
    chat.history().await;
    let task = chat.compact(Some("why")).await;
    assert_eq!(chat.outcome(task).await["status"], "completed");
    assert!(chat.faux.summary_requests().is_empty());
    let messages = chat.root.context(&ctx(), None).await.unwrap().messages;
    assert!(first_user_text(&messages).contains("<summary>\nFROM HOOK\n</summary>"));
    assert_eq!(chat.setup.reports(), ["hook broke"]);
    let selection = seen.lock().unwrap()[0].clone();
    let reason = serde_json::to_value(selection.reason).unwrap();
    assert_eq!((reason, selection.instructions.as_deref()), (json!("manual"), Some("why")));
    let entry_kinds: Vec<&str> = selection.entries.iter().map(|record| record.kind.as_str()).collect();
    assert_eq!(entry_kinds, ["pi.user", "pi.system", "pi.assistant", "pi.user", "pi.assistant"]);
    let texts: Vec<String> = selection
        .messages
        .iter()
        .filter(|message| !matches!(message, Message::System(_)))
        .map(|message| user_text(Some(message)))
        .collect();
    assert_eq!(texts, [text("u1", 100), text("a1", 100), text("u2", 100), text("a2", 100)]);
    chat.close().await;
}

#[tokio::test]
async fn completes_without_a_summary_when_a_hook_declines() {
    let chat = open(OpenOptions::default()).await;
    before_compact(&chat, "declines", |_| Ok(Some(CompactionDecision::Decline)));
    chat.history().await;
    let task = chat.compact(None).await;
    assert_eq!(chat.outcome(task).await, json!({ "status": "completed", "result": {} }));
    assert!(chat.faux.summary_requests().is_empty());
    chat.close().await;
}

#[tokio::test]
async fn fails_with_no_model_without_a_configured_model() {
    let chat = open(OpenOptions::default()).await;
    chat.history().await;
    chat.root.configure(AgentChange { model: Change::Clear, ..AgentChange::default() }, &ctx()).await.unwrap();
    let task = chat.compact(None).await;
    let outcome = chat.outcome(task).await;
    assert_eq!((&outcome["status"], &outcome["error"]["detail"]["reason"]), (&json!("failed"), &json!("no_model")));
    assert!(chat.live().await.get("compactions").is_none());
    chat.close().await;
}

/// Input tokens a compaction added to the ledger.
async fn compaction_input(chat: &Chat) -> u64 {
    let input = || async {
        let usage = chat.harness.snapshot(&USAGE_DOC, chat.root.id, &ctx()).await.unwrap().unwrap().to_serde();
        usage["models"]["faux/faux-1"]["input"].as_u64().unwrap()
    };
    let before = input().await;
    let task = chat.compact(None).await;
    chat.result(task).await;
    input().await - before
}

fn thinking(level: ThinkingLevel) -> AgentChange {
    AgentChange { thinking_level: Change::Set(level), ..AgentChange::default() }
}

#[tokio::test]
async fn retries_a_retryable_error_with_the_pinned_request_and_counts_every_attempt_once() {
    let single = open(OpenOptions::default()).await;
    single.history().await;
    single.root.configure(thinking(ThinkingLevel::High), &ctx()).await.unwrap();
    single.faux.summary(summary());
    let once = compaction_input(&single).await;
    assert!(once > 0);
    single.close().await;

    let chat = open(OpenOptions::default()).await;
    chat.history().await;
    chat.root.configure(thinking(ThinkingLevel::High), &ctx()).await.unwrap();
    chat.setup.settings.lock().unwrap().stream = Some(ConversationStreamOptions {
        timeout_ms: Some(1234),
        deferred: Some(DeferredOption::Enabled(true)),
        ..Default::default()
    });
    let (root, settings) = (chat.root.clone(), chat.setup.settings.clone());
    chat.faux.summary(Step::Run(Arc::new(move |_| {
        let (root, settings) = (root.clone(), settings.clone());
        Box::pin(async move {
            // Changed while the request runs: the retry still uses the pinned request.
            root.configure(thinking(ThinkingLevel::Low), &ctx()).await.unwrap();
            settings.lock().unwrap().stream =
                Some(ConversationStreamOptions { timeout_ms: Some(1), ..Default::default() });
            Ok(failure("overloaded"))
        })
    })));
    chat.faux.summary(summary());
    let twice = compaction_input(&chat).await;
    let requests = chat.faux.summary_requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(twice, 2 * once);
    let provider = chat.harness.snapshot(&PROVIDER_DOC, chat.root.id, &ctx()).await.unwrap().unwrap().to_serde();
    for request in requests {
        let options = &request.options;
        assert_eq!((options.reasoning, options.timeout_ms), (Some(ThinkingLevel::High), Some(1234)));
        assert_eq!(options.cache_retention, Some(CacheRetention::None));
        assert_eq!(options.session_id.as_deref(), provider["sessionId"].as_str());
        assert!(options.deferred.is_none());
    }
    chat.close().await;
}

#[tokio::test]
async fn adds_no_usage_when_a_hook_declines_or_supplies_the_summary() {
    for decision in [CompactionDecision::Decline, CompactionDecision::Summary("HOOK".into())] {
        let chat = open(OpenOptions::default()).await;
        before_compact(&chat, "decides", move |_| Ok(Some(decision.clone())));
        chat.history().await;
        assert_eq!(compaction_input(&chat).await, 0);
        chat.close().await;
    }
}

#[tokio::test]
async fn caps_max_tokens_at_the_models_output_limit_and_sends_no_tools() {
    let chat = open(OpenOptions::default()).await;
    let read = define_tool("read", "read", json!({ "type": "object" }), |_, _, _| async {
        Ok(ToolExecutionResult { output: Some(Vec::new()), ..Default::default() })
    });
    chat.setup.registry.install(Extension::new("tool:read").tool(read)).unwrap();
    let tools = AgentChange { tools: Change::Set(ToolFilter::List(vec!["read".into()])), ..AgentChange::default() };
    chat.root.configure(tools, &ctx()).await.unwrap();
    chat.history().await;
    chat.setup.settings.lock().unwrap().compaction =
        Some(CompactionOverrides { reserve_tokens: Some(2000), ..manual() });
    chat.faux.summary(summary());
    let task = chat.compact(None).await;
    chat.result(task).await;
    let request = chat.faux.summary_requests()[0].clone();
    // 0.8 * 2000 = 1600, above the model's 900.
    assert_eq!(request.options.max_tokens, Some(900));
    assert_eq!(request.messages.len(), 2);
    let adds_tools = |message: &Message| matches!(message, Message::System(system) if system.tools_added.is_some());
    assert!(!request.messages.iter().any(adds_tools));
    chat.close().await;
}

async fn fails_with_model_error(response: AssistantMessage, message: &str, requests: usize) {
    let chat = open(OpenOptions::default()).await;
    chat.history().await;
    for _ in 0..3 {
        chat.faux.summary(response.clone());
    }
    let task = chat.compact(None).await;
    let outcome = chat.outcome(task).await;
    assert_eq!(outcome["status"], "failed");
    let error = &outcome["error"];
    assert_eq!((&error["message"], &error["detail"]["reason"]), (&json!(message), &json!("model_error")));
    // The retry policy allows two retries after the first request.
    assert_eq!(chat.faux.summary_requests().len(), requests);
    assert!(chat.live().await.get("compactions").is_none());
    assert!(!chat.kinds().await.contains(&"pi.compaction".to_owned()));
    chat.close().await;
}

#[tokio::test]
async fn fails_with_model_error_on_retries_run_out() {
    fails_with_model_error(failure("overloaded"), "Summarization failed: overloaded", 3).await;
}

#[tokio::test]
async fn fails_with_model_error_on_a_non_retryable_error() {
    fails_with_model_error(failure("bad request"), "Summarization failed: bad request", 1).await;
}

#[tokio::test]
async fn fails_with_model_error_on_a_length_stop() {
    let length = faux_assistant_message("partial", FauxMessageOptions::stop_reason(StopReason::Length));
    fails_with_model_error(length, "Summarization hit the token limit; the summary is incomplete", 1).await;
}

#[tokio::test]
async fn fails_with_model_error_on_a_tool_call() {
    let call = vec![faux_tool_call_with_id("read", json!({}), "r1")];
    let calling = faux_assistant_message(call, FauxMessageOptions::stop_reason(StopReason::Stop));
    fails_with_model_error(calling, "Summarization attempted to call a tool", 1).await;
}

#[tokio::test]
async fn fails_with_model_error_on_empty_text() {
    fails_with_model_error(answer("  "), "Summarization produced no text", 1).await;
}
