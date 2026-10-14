//! The upstream "compaction estimates and interactions" and "compaction events and live status" cases.

use pi_durable::harness::compaction::{CompactionDecision, CompactionHooks, compaction_task};
use pi_durable::session::Tx;
use pi_durable::tasks::TaskOptions;
use pi_durable::{ContextEdit, ContextEditAction, ConversationOwnership, EntryDraft};

use super::chat::*;
use super::outcomes::before_compact;
use super::support::task_support::{completed, eventually};

fn ownerless() -> CreateOptions {
    CreateOptions { ownership: ConversationOwnership::Ownerless, ..CreateOptions::default() }
}

async fn ignores_usage_measured_before_a_summary_placed_mid_run(fixed_clock: bool) {
    let setup = compaction_setup(Some(2000));
    if fixed_clock {
        *setup.now.lock().unwrap() = Some(1_000.0);
    }
    let faux = script(&setup);
    let chat = open_with(window(2000), setup, faux).await;
    let (tool_gate, tool_reached) = (Deferred::default(), Deferred::default());
    let (gate, reached) = (tool_gate.clone(), tool_reached.clone());
    let slow = define_tool("slow", "slow", json!({ "type": "object" }), move |_, _, _| {
        let (gate, reached) = (gate.clone(), reached.clone());
        async move {
            reached.resolve();
            gate.wait().await;
            let output = vec![pi_ai::ContentBlock::text(text("result", 200))];
            Ok(ToolExecutionResult { output: Some(output), ..Default::default() })
        }
    });
    chat.setup.registry.install(Extension::new("tool:slow").tool(slow)).unwrap();
    let tools = AgentChange { tools: Change::Set(ToolFilter::List(vec!["slow".into()])), ..AgentChange::default() };
    chat.root.configure(tools, &ctx()).await.unwrap();
    for (user, reply) in [("u1", "a1"), ("u2", "a2"), ("u3", "a3")] {
        chat.turn(&text(user, 220), &text(reply, 220)).await;
    }
    // Background at 900, blocking at 1500: the tool call's usage plus its result would cross 1500.
    chat.set_policy(CompactionOverrides { background_tokens: Some(600), ..background() });
    // The request starts a background compaction; its tool call's usage measures the whole context.
    let call = vec![faux_tool_call_with_id("slow", json!({}), "s1")];
    chat.faux.agent(faux_assistant_message(call, FauxMessageOptions::stop_reason(StopReason::ToolUse)));
    chat.faux.agent(answer("done"));
    let (summary_gate, summary_reached) = (Deferred::default(), Deferred::default());
    chat.faux.summary(gated(&summary_gate, summary(), Some(&summary_reached)));
    let input = chat.submit(&text("u4", 50)).await;
    tool_reached.wait().await;
    summary_reached.wait().await;
    let background = chat.compaction_tasks().await.remove(0);
    summary_gate.resolve();
    // Queued: the run is busy in its tool round.
    let placed = chat.placed(background.id).await;
    assert_eq!(placed.status(&ctx()).await.unwrap().status, SubmissionStatus::Queued);
    tool_gate.resolve();
    assert_eq!(input.wait(&ctx()).await.unwrap().status, SubmissionStatus::Done);
    // The summary landed at postTools; the successor saw a small context and did not compact again.
    assert_eq!(chat.faux.summary_requests().len(), 1);
    assert!(chat.compaction_tasks().await.is_empty());
    assert!(first_user_text(&chat.faux.last_agent_request()).contains("SUMMARY"));
    chat.close().await;
}

#[tokio::test]
async fn ignores_usage_measured_before_a_summary_placed_mid_run_real_clock() {
    ignores_usage_measured_before_a_summary_placed_mid_run(false).await;
}

#[tokio::test]
async fn ignores_usage_measured_before_a_summary_placed_mid_run_fixed_clock() {
    ignores_usage_measured_before_a_summary_placed_mid_run(true).await;
}

#[tokio::test]
async fn rebaselines_the_system_prompt_over_kept_system_deltas() {
    let chat = open(OpenOptions::default()).await;
    let mood = Arc::new(Mutex::new("cheerful".to_owned()));
    let current = mood.clone();
    let section =
        section("mood", move |_, _| std::future::ready(Ok(Some(current.lock().unwrap().clone()))), Some(false));
    chat.setup.registry.install(Extension::new("section:mood").section(section)).unwrap();
    chat.turn(&text("u1", 100), &text("a1", 100)).await;
    chat.turn(&text("u2", 100), &text("a2", 100)).await;
    *mood.lock().unwrap() = "terse".into();
    chat.turn(&text("u3", 100), &text("a3", 100)).await;
    chat.set_policy(CompactionOverrides { keep_recent_tokens: Some(250), ..manual() });
    chat.faux.summary(summary());
    let task = chat.compact(None).await;
    chat.result(task).await;
    // The kept range holds the delta for the terse mood; the next request has one complete baseline.
    let view = chat.root.context(&ctx(), None).await.unwrap();
    assert!(view.entries.iter().any(|record| record.kind == "pi.system"));
    chat.turn("next", "ok").await;
    let request = chat.faux.last_agent_request();
    let systems: Vec<&Message> = request.iter().filter(|message| matches!(message, Message::System(_))).collect();
    assert_eq!(systems.len(), 1);
    let sections = serde_json::to_value(systems[0]).unwrap()["sections"].clone();
    assert_eq!(sections, json!({ "preamble": "You are helpful.", "mood": "terse" }));
    chat.close().await;
}

#[tokio::test]
async fn summarizes_a_replaced_entrys_replacement_and_shows_it_to_the_hook() {
    let chat = open(OpenOptions::default()).await;
    chat.history().await;
    let u1 = all_entries(&chat.root).await[0].id;
    let redacted =
        pi_ai::Message::User(pi_ai::UserMessage { content: pi_ai::UserContent::Text("REDACTED".into()), timestamp: 0 });
    let edit = ContextEdit { target: u1, action: ContextEditAction::Replace, messages: Some(vec![redacted]) };
    let entry = EntryDraft { edits: Some(vec![edit]), ..EntryDraft::new("app.redact") };
    chat.root.submit(SubmissionDraft::write(entry), &ctx()).await.unwrap();
    let messages = Arc::new(Mutex::new(Vec::new()));
    let sink = messages.clone();
    before_compact(&chat, "observes", move |selection| {
        *sink.lock().unwrap() = selection.messages;
        Ok(None)
    });
    chat.faux.summary(summary());
    let task = chat.compact(None).await;
    chat.result(task).await;
    assert_eq!(first_user_text(&messages.lock().unwrap()), "REDACTED");
    assert!(user_text(chat.faux.summary_requests()[0].messages.get(1)).contains("[User]: REDACTED"));
    chat.close().await;
}

#[tokio::test]
async fn compacts_a_fork_whose_cut_falls_on_a_parent_entry() {
    let chat = open(OpenOptions::default()).await;
    chat.history().await;
    let entries = all_entries(&chat.root).await;
    let fork = chat.root.fork(entries.last().unwrap().id, ownerless(), &ctx()).await.unwrap();
    chat.faux.summary(summary());
    let task = fork.compact(None, &ctx()).await.unwrap();
    assert_eq!(chat.placed(task).await.wait(&ctx()).await.unwrap().status, SubmissionStatus::Done);
    let u3 = entries.iter().find(|record| user_text(record.model.as_ref().and_then(|m| m.first())).starts_with("u3"));
    let view = fork.context(&ctx(), None).await.unwrap();
    let head = view.head.clone().unwrap();
    assert_eq!((head.kind.as_str(), head.head, head.conversation_id), ("pi.compaction", Some(u3.unwrap().id), fork.id));
    let rest: Vec<String> = view.messages[1..].iter().map(|message| user_text(Some(message))).collect();
    assert_eq!(rest, [text("u3", 100), text("a3", 100)]);
    // The parent is untouched.
    assert!(!chat.kinds().await.contains(&"pi.compaction".to_owned()));
    // The fork's view keeps the parent entries the summary kept.
    let state = fork.view_state(&ctx()).await.unwrap();
    let mounted = state.value().get("entries").unwrap().to_serde();
    assert_eq!(Json::from(mounted), Json::from(serde_json::to_value(&view.entries).unwrap()));
    state.dispose().unwrap();
    chat.close().await;
}

#[tokio::test]
async fn settles_stale_in_a_fork_reset_while_it_summarizes() {
    let chat = open(OpenOptions::default()).await;
    chat.history().await;
    let entries = all_entries(&chat.root).await;
    let fork = chat.root.fork(entries.last().unwrap().id, ownerless(), &ctx()).await.unwrap();
    let (gate, reached) = (Deferred::default(), Deferred::default());
    chat.faux.summary(gated(&gate, summary(), Some(&reached)));
    let task = fork.compact(None, &ctx()).await.unwrap();
    reached.wait().await;
    fork.reset(None, &ctx()).await.unwrap();
    gate.resolve();
    let status = chat.placed(task).await.status(&ctx()).await.unwrap();
    assert_eq!((status.status, status.reason.as_deref()), (SubmissionStatus::Unanswered, Some("stale")));
    let head = fork.context(&ctx(), None).await.unwrap().head.unwrap();
    assert_eq!(head.kind, "pi.reset");
    chat.close().await;
}

#[tokio::test]
async fn places_an_older_queued_summary_and_the_current_one_together_when_idle_admission_drains_the_inbox() {
    let chat = open(OpenOptions::default()).await;
    chat.history().await;
    let (gate, reached) = (Deferred::default(), Deferred::default());
    chat.faux.agent(gated(&gate, failure("bad request"), Some(&reached)));
    let failed = chat.submit("fails").await;
    reached.wait().await;
    chat.faux.summary(answer("OLDER"));
    let older = chat.compact(None).await;
    chat.result(older).await;
    gate.resolve();
    failed.wait(&ctx()).await.unwrap();
    // Idle now, with the older summary still queued; the current one queues behind it and a final boundary runs.
    chat.faux.summary(answer("CURRENT"));
    let current = chat.compact(None).await;
    chat.result(current).await;
    let mut statuses = Vec::new();
    for task in [older, current] {
        statuses.push(chat.placed(task).await.status(&ctx()).await.unwrap().status);
    }
    assert_eq!(statuses, [SubmissionStatus::Done, SubmissionStatus::Done]);
    assert!(chat.first_text().await.contains("CURRENT"));
    chat.close().await;
}

/// A task that waits for `gate`, then completes.
pub fn child_task(gate: &Deferred) -> Task {
    let gate = gate.clone();
    define_task(
        TaskDefinition::new("test.child", 1, |_| Ok(Json::from(json!({ "phase": "run" }))))
            .phase("run", move |_, runtime, task_context| {
                let gate = gate.clone();
                async move {
                    gate.wait().await;
                    runtime.commit(|_, _| async { Ok(completed(Value::Null)) }, &task_context).await
                }
            })
            .abort(|_, _, _| async { Ok(()) }),
    )
}

#[tokio::test]
async fn places_a_hooks_summary_and_holds_while_work_the_hook_created_runs() {
    let chat = open(OpenOptions::default()).await;
    let child_gate = Deferred::default();
    let child = child_task(&child_gate);
    chat.setup.registry.install(Extension::new("tasks").task(child.clone())).unwrap();
    let root = chat.root.clone();
    let hooks = CompactionHooks {
        before_compact: Some(Arc::new(move |_, api: HookApi, hook_context| {
            let (root, child) = (root.clone(), child.clone());
            Box::pin(async move {
                let (owner, id) = (api.task_id(), root.id);
                let create = move |tx: Tx| async move {
                    let options = TaskOptions { conversation_id: Some(id), ..TaskOptions::owned_by(owner) };
                    tx.create_task(child, Json::from(json!({})), options).await
                };
                root.commit(create, &hook_context).await?;
                Ok(Some(CompactionDecision::Summary("HOOK".into())))
            })
        })),
    };
    chat.setup.registry.install(Extension::new("hooks").hook(hook(&compaction_task(), hooks))).unwrap();
    chat.history().await;
    let task = chat.compact(None).await;
    let harness = chat.harness.clone();
    eventually(|| {
        let harness = harness.clone();
        async move {
            let record = harness.get_task(task, &ctx()).await.unwrap().unwrap();
            record.state.status() == pi_durable::TaskStatus::Completing
        }
    })
    .await;
    // The summary and the status removal landed at the hold.
    assert_eq!(chat.kinds().await.last().map(String::as_str), Some("pi.compaction"));
    assert!(chat.live().await.get("compactions").is_none());
    child_gate.resolve();
    assert_eq!(chat.outcome(task).await["status"], "completed");
    chat.close().await;
}

// Upstream's case that removes the status of a faulted compaction patches `models.getModel` to throw. Rust
// `Models::get_model` returns an `Option` and cannot fail, so this fault is not reachable from a test.

#[tokio::test]
async fn reports_start_and_end_and_the_retry_backoff_in_a_late_joiners_snapshot() {
    let chat = open(OpenOptions::default()).await;
    chat.history().await;
    let retry =
        RetryOverrides { enabled: Some(true), max_retries: Some(2), base_delay_ms: Some(60_000), ..Default::default() };
    chat.setup.settings.lock().unwrap().retry = Some(retry);
    let (stream, events) = listen(&chat).await;
    chat.faux.summary(failure("overloaded"));
    let task = chat.compact(None).await;
    let probe = chat.clone();
    wait_for(move || {
        let chat = probe.clone();
        async move { !chat.live().await["compactions"][0]["retry"].is_null() }
    })
    .await;
    let late = watch_events(&chat.harness, chat.root.id, &ctx()).await.unwrap();
    let snapshot = Value::from(&late.snapshot);
    let compactions = &snapshot["compactions"];
    assert_eq!(compactions.as_array().map(Vec::len), Some(1));
    let status = &compactions[0];
    assert_eq!((&status["taskId"], &status["reason"]), (&json!(task), &json!("manual")));
    assert_eq!((&status["blocking"], &status["attempt"]), (&json!(false), &json!(1)));
    assert!(status["retry"]["at"].is_number());
    assert_eq!(status["retry"]["error"], "overloaded");
    late.stop().await;
    chat.harness.abort_task(task, &ctx()).await.unwrap();
    let seen = events.clone();
    wait_for(move || {
        let ended = event_types(&seen).iter().any(|kind| kind == "compaction_end");
        async move { ended }
    })
    .await;
    let compaction_events: Vec<Value> = events
        .lock()
        .unwrap()
        .iter()
        .filter(|event| event["type"].as_str().is_some_and(|kind| kind.starts_with("compaction_")))
        .cloned()
        .collect();
    assert_eq!(
        compaction_events,
        [
            json!({ "type": "compaction_start", "taskId": task, "reason": "manual", "blocking": false }),
            json!({ "type": "compaction_end", "taskId": task, "reason": "manual" }),
        ]
    );
    stream.stop().await;
    chat.close().await;
}

#[tokio::test]
async fn lists_concurrent_compactions_in_task_id_order() {
    let chat = open(OpenOptions::default()).await;
    chat.history().await;
    let (first, second) = (Deferred::default(), Deferred::default());
    chat.faux.summary(gated(&Deferred::default(), summary(), Some(&first)));
    chat.faux.summary(gated(&Deferred::default(), summary(), Some(&second)));
    let a = chat.compact(None).await;
    let b = chat.compact(None).await;
    first.wait().await;
    second.wait().await;
    let ids: Vec<Value> =
        chat.live().await["compactions"].as_array().unwrap().iter().map(|s| s["taskId"].clone()).collect();
    assert_eq!(ids, [json!(a), json!(b)]);
    chat.root.abort(&ctx(), false).await.unwrap();
    assert!(chat.live().await.get("compactions").is_none());
    chat.close().await;
}
