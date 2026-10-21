//! Examples 21, 22, and 24: a late joiner of a running tool, a foreground subagent, and child tasks.

use std::collections::BTreeSet;
use std::sync::Mutex;
use std::time::Duration;

use pi_ai::{Message, StopReason};
use pi_durable::harness::agent::configure;
use pi_durable::harness::watch_events;
use pi_durable::storage::sqlite::open_sqlite_storage;
use pi_durable::tasks::TaskOptions;
use pi_durable::testing::faux::{
    FauxMessageOptions, FauxResponseStep, FauxTokenSize, RegisterFauxProviderOptions, faux_assistant_message,
    faux_tool_call_with_id,
};
use support::chat::answer;

use super::*;

type Lines = Arc<Mutex<Vec<String>>>;

fn tool_call(name: &str, args: Value, id: &str) -> FauxResponseStep {
    let call = faux_tool_call_with_id(name, args, id);
    faux_assistant_message(vec![call], FauxMessageOptions::stop_reason(StopReason::ToolUse)).into()
}

fn pause(ms: u64) -> tokio::time::Sleep {
    tokio::time::sleep(Duration::from_millis(ms))
}

#[tokio::test]
async fn ex21_late_join_sees_running_output_in_the_view_and_the_event_snapshot() {
    let pacing = RegisterFauxProviderOptions {
        tokens_per_second: Some(40.0),
        token_size: Some(FauxTokenSize { min: Some(1), max: Some(1) }),
        ..Default::default()
    };
    let setup = chat_setup(pacing);
    let count = define_tool("count", "Counts to ten", support::chat::no_parameters(), |_, api, _| async move {
        for n in 1..=10 {
            api.output(&format!("{n}\n"))?;
            pause(30).await;
        }
        Ok(ToolExecutionResult::default())
    });
    support::chat::add_tool(&setup, count);
    setup.faux.set_responses([
        tool_call("count", json!({}), "call-1"),
        answer("Counted to ten, and this answer streams slowly."),
    ]);
    let (harness, root) = open_chat(memory(), &setup).await;
    let submission = root.submit(SubmissionDraft::input("Count to ten, then tell me."), &ctx()).await.unwrap();
    // Join while the tool runs: the view holds its slot and the output so far.
    let view = root.view_state(&ctx()).await.unwrap();
    let slot = |view: &Json| view.get("docs")?.get("pi.live")?.get("tools")?.as_array()?.first().map(Json::to_serde);
    let state = view.clone();
    wait_for(move || {
        let ready = slot(&state.value()).is_some_and(|slot| slot["status"] == "running" && slot["output"] != "");
        async move { ready }
    })
    .await;
    let kinds: Vec<String> = view
        .value()
        .get("entries")
        .unwrap()
        .to_serde()
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["kind"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(kinds, ["pi.user", "pi.system", "pi.assistant"]);
    let stream = watch_events(&harness, root.id, &ctx()).await.unwrap();
    let tools = stream.snapshot.get("tools").unwrap().to_serde();
    assert_eq!((&tools[0]["name"], &tools[0]["status"]), (&json!("count"), &json!("running")));
    let output = Arc::new(Mutex::new(tools[0]["output"].as_str().unwrap_or_default().to_owned()));
    let (lines, sink, joined): (Lines, _, _) = (Lines::default(), output.clone(), output.clone());
    let log = lines.clone();
    stream
        .start(move |events, _| {
            for event in events.iter().map(Json::to_serde) {
                let mut output = sink.lock().unwrap();
                match (event["type"].as_str().unwrap_or_default(), &event["output"]) {
                    ("tool_execution_update", change) if change.is_object() => {
                        // Rebuild the output from the joiner's copy: `set`, or trim then append.
                        if let Some(set) = change["set"].as_str() {
                            *output = set.to_owned();
                        } else {
                            let trim = change["trimStart"].as_u64().unwrap_or(0) as usize;
                            *output = format!("{}{}", &output[trim..], change["append"].as_str().unwrap_or(""));
                        }
                    }
                    (kind, _) => log.lock().unwrap().push(kind.to_owned()),
                }
            }
            async { Ok(()) }
        })
        .unwrap();
    submission.wait(&ctx()).await.unwrap();
    harness.wait_for_idle(&ctx()).await.unwrap();
    // Events arrive asynchronously; the end event follows the last output update.
    let ended = || lines.lock().unwrap().iter().any(|kind| kind == "tool_execution_end");
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while !ended() && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    stream.stop().await;
    view.dispose().unwrap();
    // Events come from commits: output written in the commit that ends the tool has no update event, as upstream.
    let expected: String = (1..=10).map(|n| format!("{n}\n")).collect();
    let joined = joined.lock().unwrap().clone();
    assert!(joined.starts_with("1\n") && expected.starts_with(&joined), "{joined:?}");
    assert!(lines.lock().unwrap().iter().any(|kind| kind == "tool_execution_end"));
    harness.close(&ctx()).await.unwrap();
}

/// The text of an assistant answer entry, read through the tool's transaction.
async fn answer_text(api: &ToolExecutionApi, answer: EntryId, context: &Context) -> DurableResult<String> {
    let entry = api.commit(move |tx: Tx| async move { tx.entry(answer).await }, context).await?;
    Ok(text_of(entry.and_then(|entry| entry.model).unwrap_or_default().first()).unwrap_or_default())
}

fn subagent_extension() -> Extension {
    let parameters = json!({ "type": "object", "properties": { "task": { "type": "string" } }, "required": ["task"] });
    let description = "Delegate a self-contained task to a subagent and get its answer back.";
    let tool = define_tool("subagent", description, parameters, |args, api, call| async move {
        let task = args.get("task").and_then(Value::as_str).unwrap_or_default().to_owned();
        let owner = api.task_id;
        // A replayed call finds the child it created before.
        let child = api
            .commit(
                move |tx: Tx| async move {
                    let query = ConversationQuery { owner_task_id: Some(owner), ..Default::default() };
                    if let Some(existing) = tx.scan_conversations(query, 1, None).await?.items.first() {
                        return Ok(existing.id);
                    }
                    let created = tx.create_conversation(ConversationOwnership::Task { task_id: owner }).await?;
                    let remove = ExtensionSelection::Edit { add: None, remove: Some(vec!["subagent".into()]) };
                    configure(&tx, created.id, &AgentChange { extensions: Change::Set(remove), ..Default::default() })
                        .await?;
                    Ok(created.id)
                },
                &call,
            )
            .await?;
        let details = Json::from(json!({ "conversationId": child }));
        api.details(details.clone(), &call).await?;
        let handle = api.conversation(child, &call).await?.unwrap();
        let request = SubmissionDraft::Input {
            request_id: Some(format!("subagent:{owner}")),
            content: pi_ai::UserContent::Text(task),
            when_busy: None,
            entry: None,
        };
        let settled = handle.submit(request, &call).await?.wait(&call).await?;
        let Some(answer) = settled.answer.filter(|_| settled.status == SubmissionStatus::Done) else {
            return Err(DurableError::error(format!("Subagent failed: {:?}", settled.status)));
        };
        let text = answer_text(&api, answer, &call).await?;
        Ok(ToolExecutionResult { details: Some(details), ..ToolExecutionResult::text(text) })
    });
    Extension::new("subagent").tool(tool.with_replay(ToolReplay::Safe))
}

/// Attach to a conversation's events, logging answers and tool starts, and attach to children it reports.
fn attach(
    harness: Harness,
    id: ConversationId,
    indent: String,
    lines: Lines,
    attached: Arc<Mutex<BTreeSet<ConversationId>>>,
) -> futures::future::BoxFuture<'static, ()> {
    Box::pin(async move {
        attached.lock().unwrap().insert(id);
        let stream = watch_events(&harness, id, &ctx()).await.unwrap();
        let listener = move |events: Vec<Json>, _| {
            let (harness, indent, lines, attached) = (harness.clone(), indent.clone(), lines.clone(), attached.clone());
            async move {
                for event in events.iter().map(Json::to_serde) {
                    match event["type"].as_str().unwrap_or_default() {
                        "message_end" if event["entry"]["kind"] == "pi.assistant" => {
                            let message: Message = serde_json::from_value(event["entry"]["model"][0].clone()).unwrap();
                            let text = text_of(Some(&message)).unwrap_or_default();
                            if !text.is_empty() {
                                lines.lock().unwrap().push(format!("{indent}assistant: {text}"));
                            }
                        }
                        "tool_execution_start" => lines.lock().unwrap().push(format!(
                            "{indent}tool {}({})",
                            event["toolName"].as_str().unwrap(),
                            event["args"]
                        )),
                        "tool_execution_update" => {
                            let child = event["details"]["conversationId"].as_u64().map(ConversationId);
                            if let Some(child) = child.filter(|child| !attached.lock().unwrap().contains(child)) {
                                attach(harness.clone(), child, format!("{indent}  "), lines.clone(), attached.clone())
                                    .await;
                            }
                        }
                        _ => {}
                    }
                }
                Ok(())
            }
        };
        stream.start(listener).unwrap();
        std::mem::forget(stream); // Lives until the Harness closes, like the upstream script's streams.
    })
}

#[tokio::test]
async fn ex22_subagent_foreground_delegates_to_a_child_conversation_and_follows_it() {
    let setup = chat_setup(Default::default());
    setup.registry.install(subagent_extension()).unwrap();
    let delegate = tool_call("subagent", json!({ "task": "Name three prime numbers." }), "call-1");
    setup.faux.set_responses([delegate, answer("2, 3, and 5."), answer("The subagent says: 2, 3, and 5.")]);
    let (harness, root) = open_chat(memory(), &setup).await;
    let lines = Lines::default();
    attach(harness.clone(), root.id, String::new(), lines.clone(), Arc::default()).await;
    let prompt = "Use the subagent tool to find three prime numbers, then tell me what it said.";
    root.submit(SubmissionDraft::input(prompt), &ctx()).await.unwrap().wait(&ctx()).await.unwrap();
    harness.wait_for_idle(&ctx()).await.unwrap();
    let done = lines.clone();
    wait_for(move || {
        let ready = done.lock().unwrap().len() == 3;
        async move { ready }
    })
    .await;
    let expected = [
        r#"tool subagent({"task":"Name three prime numbers."})"#,
        "  assistant: 2, 3, and 5.",
        "assistant: The subagent says: 2, 3, and 5.",
    ];
    assert_eq!(*lines.lock().unwrap(), expected);
    harness.close(&ctx()).await.unwrap();
}

fn next(state: Value) -> Option<TaskState> {
    Some(serde_json::from_value(state).unwrap())
}

fn checkout_tasks(log: Lines, charged: Arc<Mutex<BTreeSet<String>>>) -> (Task, Task) {
    let card = |task: &TaskRecord| task.input.get("card").and_then(Json::as_str).unwrap_or_default().to_owned();
    let (refunds, aborts) = (charged.clone(), log.clone());
    let payment = define_task(
        TaskDefinition::new("example.payment", 1, |_| Ok(Json::from(json!({ "phase": "charge" }))))
            .phase("charge", move |task, runtime, context| {
                let charged = charged.clone();
                async move {
                    let card = card(&task);
                    if card.starts_with("expired") {
                        let failed = next(json!({ "status": "terminal", "outcome": { "status": "failed", "error": { "message": format!("{card} declined") } } }));
                        return runtime.commit(move |_, _| async move { Ok(failed) }, &context).await;
                    }
                    charged.lock().unwrap().insert(card.clone());
                    // The bank confirms after a moment: wait for it without holding an invocation.
                    let confirm = Json::from(json!({ "phase": "confirm" }));
                    runtime.sleep_until(runtime.now() + 100.0, confirm, &context).await
                }
            })
            .phase("confirm", move |task, runtime, context| async move {
                let card = card(&task);
                let done = next(json!({ "status": "terminal", "outcome": { "status": "completed", "result": { "card": card } } }));
                runtime.commit(move |_, _| async move { Ok(done) }, &context).await
            })
            .abort(move |task, runtime, context| {
                let card = card(&task);
                let refunded = refunds.lock().unwrap().remove(&card);
                aborts.lock().unwrap().push(format!("payment {card} aborted{}", if refunded { ", refunded" } else { "" }));
                async move { runtime.commit(|_, _| async { Ok(next(json!({ "status": "terminal", "outcome": { "status": "aborted" } }))) }, &context).await }
            }),
    );
    let (child, decided, aborted) = (payment.clone(), log.clone(), log);
    let checkout = define_task(
        TaskDefinition::new("example.checkout", 1, |_| Ok(Json::from(json!({ "phase": "pay" }))))
            .phase("pay", move |task, runtime, context| {
                let payment = child.clone();
                async move {
                    let cards: Vec<String> = task.input.get("cards").unwrap().deserialize_into().unwrap();
                    let owner = task.id;
                    let pay = move |tx: Tx, _| async move {
                        let mut payments = Vec::new();
                        for card in cards {
                            let options = TaskOptions::owned_by(owner);
                            payments.push(
                                tx.create_task(payment.clone(), Json::from(json!({ "card": card })), options).await?,
                            );
                        }
                        let checkpoint = Json::from(json!({ "phase": "decide", "payments": payments }));
                        Ok(Some(TaskState::Waiting { checkpoint, on: payments, policy: JoinPolicy::FailFast }))
                    };
                    runtime.commit(pay, &context).await
                }
            })
            .phase("decide", move |task, runtime, context| {
                let log = decided.clone();
                async move {
                    let ids: Vec<TaskId> =
                        task.state.checkpoint().unwrap().get("payments").unwrap().deserialize_into().unwrap();
                    let outcomes = runtime.outcomes(&ids, &context).await?;
                    let statuses: Vec<String> = outcomes
                        .iter()
                        .map(|o| serde_json::to_value(o).unwrap()["status"].as_str().unwrap().to_owned())
                        .collect();
                    log.lock().unwrap().push(format!("payments: {}", statuses.join(", ")));
                    let outcome = if statuses.iter().all(|status| status == "completed") {
                        json!({ "status": "completed", "result": "order placed" })
                    } else {
                        json!({ "status": "failed", "error": { "message": "payment failed" } })
                    };
                    let done = next(json!({ "status": "terminal", "outcome": outcome }));
                    runtime.commit(move |_, _| async move { Ok(done) }, &context).await
                }
            })
            .abort(move |_, runtime, context| {
                aborted.lock().unwrap().push("checkout aborted".into());
                async move {
                    runtime
                        .commit(
                            |_, _| async {
                                Ok(next(json!({ "status": "terminal", "outcome": { "status": "aborted" } })))
                            },
                            &context,
                        )
                        .await
                }
            }),
    );
    (payment, checkout)
}

fn outcome_status(task: &TaskRecord) -> String {
    serde_json::to_value(&task.state).unwrap()["outcome"]["status"].as_str().unwrap_or_default().to_owned()
}

#[tokio::test]
async fn ex24_child_tasks_fail_fast_cascade_aborts_and_continue_after_restart() {
    let (log, charged) = (Lines::default(), Arc::<Mutex<BTreeSet<String>>>::default());
    let (payment, checkout) = checkout_tasks(log.clone(), charged);
    let registry = Arc::new(create_registry());
    registry.install(Extension::new("checkout").task(payment).task(checkout.clone())).unwrap();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.sqlite").to_string_lossy().into_owned();
    let open = || async {
        let options = HarnessOptions::new(
            Arc::new(pi_durable::models::create_models()),
            registry.clone() as Arc<dyn RegistryReader>,
        );
        let harness =
            Harness::open(Arc::new(open_sqlite_storage(&path).await.unwrap()), options, &ctx()).await.unwrap();
        let root = harness.root(&ctx(), CreateOptions::default()).await.unwrap();
        (harness, root)
    };
    let start = |root: &Conversation, cards: &[&str]| {
        let (checkout, id, cards) = (checkout.clone(), root.id, json!(cards));
        let create = move |tx: Tx| async move {
            tx.create_task(checkout, Json::from(json!({ "cards": cards })), TaskOptions::conversation(id)).await
        };
        let root = root.clone();
        async move { root.commit(create, &ctx()).await.unwrap() }
    };
    let take = || std::mem::take(&mut *log.lock().unwrap());

    // One card is declined: fail-fast aborts and refunds the other payments before the checkout decides.
    let (harness, root) = open().await;
    let id = start(&root, &["visa-1", "expired-2", "visa-3", "visa-4"]).await;
    assert_eq!(outcome_status(&harness.wait_for_task(id, &ctx()).await.unwrap()), "failed");
    let declined = take();
    let refunded = |n: u32| format!("payment visa-{n} aborted, refunded");
    let decided = "payments: aborted, failed, aborted, aborted".to_owned();
    assert_eq!(declined, [refunded(1), refunded(3), refunded(4), decided]);

    // The customer cancels: the abort cascades to the payments, which refund.
    let id = start(&root, &["visa-5", "visa-6", "visa-7", "visa-8"]).await;
    harness.resume().unwrap();
    pause(20).await;
    let graph = harness.task_graph(&ctx()).await.unwrap();
    let tasks = graph.value().get("tasks").unwrap().to_serde();
    let waiting = tasks.as_object().unwrap().values().find(|node| node["kind"] == "example.checkout").unwrap();
    assert_eq!((&waiting["state"]["status"], waiting["state"]["on"].as_array().unwrap().len()), (&json!("waiting"), 4));
    graph.dispose().unwrap();
    harness.abort_task(id, &ctx()).await.unwrap();
    assert_eq!(outcome_status(&harness.wait_for_task(id, &ctx()).await.unwrap()), "aborted");
    let mut cancelled = take();
    cancelled.sort();
    let refunds: Vec<String> = (5..=8).map(|n| format!("payment visa-{n} aborted, refunded")).collect();
    assert_eq!(cancelled, [vec!["checkout aborted".to_owned()], refunds].concat());

    // The process stops while the payments run, and a new one continues them.
    let id = start(&root, &["visa-9", "visa-10", "visa-11", "visa-12"]).await;
    pause(20).await;
    harness.close(&ctx()).await.unwrap();
    let (harness, _) = open().await;
    assert_eq!(outcome_status(&harness.wait_for_task(id, &ctx()).await.unwrap()), "completed");
    harness.close(&ctx()).await.unwrap();
}
