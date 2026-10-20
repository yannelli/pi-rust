//! The remaining upstream "task phases" cases.

use std::sync::LazyLock;

use super::support::task_support::{open_tasks, open_tasks_with};
use pi_ai::ThinkingLevel;
use pi_chord::with_cancel;
use pi_durable::documents::*;
use pi_durable::tasks::TaskOwnership;
use serde::{Deserialize, Serialize};

use super::*;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
struct Progress {
    lines: Vec<String>,
}

static PROGRESS_DOC: LazyLock<DocToken<Progress, TaskScope>> =
    LazyLock::new(|| define_doc(DocSpec::new("test.task-progress", 1, TaskScope, Progress::default)).unwrap());

pub fn checkpoint(phase: &str) -> Option<TaskState> {
    running(json!({ "phase": phase }))
}

fn waiting(phase: &str, on: Vec<TaskId>) -> Option<TaskState> {
    let checkpoint = Json::from(json!({ "phase": phase }));
    Some(TaskState::Waiting { checkpoint, on, policy: JoinPolicy::AllSettled })
}

/// Poll the task until its status is `status`.
pub async fn until_status(harness: &Harness, id: TaskId, status: TaskStatus) {
    support::task_support::eventually(|| {
        let harness = harness.clone();
        async move { harness.get_task(id, &context()).await.unwrap().unwrap().state.status() == status }
    })
    .await;
}

#[tokio::test]
async fn commits_results_with_entries_atomically_keeps_memos_until_terminal_and_retires_task_documents() {
    let child = one_step("test.child", |_, runtime, context| async move {
        runtime.commit(|_, _| async { Ok(completed(Value::Null)) }, &context).await
    });
    let child_id = Arc::new(Mutex::new(None));
    let progress_seen = Arc::new(Mutex::new(None));
    let (spawned, child_task, seen) = (child_id.clone(), child.clone(), progress_seen.clone());
    let writer = define_task(
        TaskDefinition::new("test.writer", 1, |_| Ok(Json::from(json!({ "phase": "write" }))))
            .phase("write", move |task, runtime, context| {
                let (spawned, child) = (spawned.clone(), child_task.clone());
                async move {
                    assert_eq!(runtime.memo("choice").await?, None);
                    let (a, b) = tokio::join!(
                        runtime.memo_or("choice", Json::from("a"), &context),
                        runtime.memo_or("choice", Json::from("b"), &context)
                    );
                    assert_eq!((a?, b?), (Json::from("a"), Json::from("a")));
                    assert_eq!(runtime.memo("choice").await?, Some(Json::from("a")));
                    // Upstream also checks inherited object properties; a Rust map has none, so `toString` is plain.
                    assert_eq!(runtime.memo("toString").await?, None);
                    assert_eq!(runtime.memo_or("toString", Json::from("own"), &context).await?, Json::from("own"));
                    let change = move |tx: Tx, _| async move {
                        tx.doc(&PROGRESS_DOC, task.id).await?.edit(|p| p.lines.push("wrote".into()))?;
                        // Task creation defaults to the task's own conversation.
                        let options = TaskOptions {
                            ownership: TaskOwnership::Conversation,
                            conversation_id: None,
                            background: false,
                            abandon_on_restart: false,
                            trace: Default::default(),
                            links: Default::default(),
                        };
                        *spawned.lock().unwrap() = Some(tx.create_task(child, Json::Null, options).await?);
                        Ok(checkpoint("answer"))
                    };
                    runtime.commit(change, &context).await
                }
            })
            .phase("answer", move |task, runtime, context| {
                let seen = seen.clone();
                async move {
                    let memos = task.memos.clone().map(|memos| Json::object(memos).to_serde());
                    assert_eq!(memos, Some(json!({ "choice": "a", "toString": "own" })));
                    let id = task.id;
                    let read = move |tx: Tx, _| async move {
                        *seen.lock().unwrap() = Some(tx.doc(&PROGRESS_DOC, id).await?.get()?.lines);
                        Ok(None)
                    };
                    runtime.commit(read, &context).await?;
                    let answer = |tx: Tx, current: TaskRecord| async move {
                        let draft = EntryDraft { model: Some(Vec::new()), ..EntryDraft::new("answer") };
                        let entry = tx.append_entry(current.conversation_id, draft).await?;
                        Ok(completed(json!({ "entryId": entry.id })))
                    };
                    runtime.commit(answer, &context).await
                }
            })
            .abort(|_, _, _| async { Ok(()) }),
    );
    let (harness, root, _) = open_root(vec![writer.clone(), child]).await;
    let ownerless = CreateOptions { ownership: ConversationOwnership::Ownerless, ..CreateOptions::default() };
    let conversation = harness.create_conversation(ownerless, &context()).await.unwrap();
    let id = start(&conversation, &writer, false).await;
    harness.resume().unwrap();
    let receipt = harness.wait_for_task(id, &context()).await.unwrap();
    assert!(receipt.memos.is_none());
    let TaskOutcome::Completed { result: Some(result) } = receipt.state.outcome().unwrap().clone() else {
        panic!("expected completion");
    };
    let entry_id: EntryId = serde_json::from_value(result.to_serde()["entryId"].clone()).unwrap();
    let entry = conversation.commit(move |tx: Tx| async move { tx.entry(entry_id).await }, &context()).await.unwrap();
    let entry = entry.unwrap();
    assert_eq!((entry.kind.as_str(), entry.conversation_id), ("answer", conversation.id));
    assert_eq!(*progress_seen.lock().unwrap(), Some(vec!["wrote".to_owned()]));
    assert!(harness.snapshot(&PROGRESS_DOC, id, &context()).await.unwrap().is_none());
    assert_eq!(harness.get_task(id, &context()).await.unwrap(), Some(receipt));
    let child_id = child_id.lock().unwrap().unwrap();
    assert_eq!(harness.wait_for_task(child_id, &context()).await.unwrap().conversation_id, conversation.id);
    assert_ne!(root.id, conversation.id);
    harness.close(&context()).await.unwrap();
}

/// A task that waits on the tasks in `on`, then resumes with `resume`.
fn waiter_task<F, Fut>(
    name: &str,
    on: Arc<Mutex<Vec<TaskId>>>,
    waited: impl Fn() + Send + Sync + 'static,
    resume: F,
) -> Task
where
    F: Fn(TaskRecord, TaskRuntime, pi_chord::Context) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = DurableResult<()>> + Send + 'static,
{
    define_task(
        TaskDefinition::new(name, 1, |_| Ok(Json::from(json!({ "phase": "wait" }))))
            .phase("wait", move |_, runtime, context| {
                waited();
                let on = on.lock().unwrap().clone();
                async move { runtime.commit(move |_, _| async move { Ok(waiting("resume", on)) }, &context).await }
            })
            .phase("resume", resume)
            .abort(|_, runtime, context| async move {
                runtime.commit(|_, _| async { Ok(aborted_with("test")) }, &context).await
            }),
    )
}

/// A one-phase task that waits for `gate` and completes with null.
pub fn gated(name: &str, gate: Deferred) -> Task {
    one_step(name, move |_, runtime, context| {
        let gate = gate.clone();
        async move {
            gate.wait().await;
            runtime.commit(|_, _| async { Ok(completed(Value::Null)) }, &context).await
        }
    })
}

#[tokio::test]
async fn resumes_a_waiting_task_once_every_task_in_on_is_terminal_whatever_the_outcome() {
    let gate = Deferred::default();
    let order = Arc::new(Mutex::new(Vec::<&str>::new()));
    let on = Arc::new(Mutex::new(Vec::new()));
    let outcomes = Arc::new(Mutex::new(Vec::new()));
    let (resume_order, resume_on, seen) = (order.clone(), on.clone(), outcomes.clone());
    let wait_order = order.clone();
    let waited = move || wait_order.lock().unwrap().push("wait");
    let waiter = waiter_task("test.waiter", on.clone(), waited, move |_, runtime, context| {
        resume_order.lock().unwrap().push("resume");
        let (on, seen) = (resume_on.lock().unwrap().clone(), seen.clone());
        async move {
            let outcomes = runtime.outcomes(&on, &context).await?;
            let statuses = outcomes.iter().map(|o| serde_json::to_value(o).unwrap()["status"].clone()).collect();
            *seen.lock().unwrap() = statuses;
            runtime.commit(|_, _| async { Ok(completed(Value::Null)) }, &context).await
        }
    });
    let (first_order, held) = (order.clone(), gate.clone());
    let first = one_step("test.first", move |_, runtime, context| {
        first_order.lock().unwrap().push("first");
        let held = held.clone();
        async move {
            held.wait().await;
            runtime.commit(|_, _| async { Ok(completed(Value::Null)) }, &context).await
        }
    });
    let fault_order = order.clone();
    let faulting = one_step("test.faulting", move |_, _, _| {
        fault_order.lock().unwrap().push("faulting");
        async { Err(DurableError::error("fails")) }
    });
    let (harness, root, _) = open_root(vec![first.clone(), faulting.clone(), waiter.clone()]).await;
    let ids = vec![start(&root, &first, false).await, start(&root, &faulting, false).await];
    *on.lock().unwrap() = ids.clone();
    let waiter_id = start(&root, &waiter, false).await;
    harness.resume().unwrap();
    let wait_order = order.clone();
    eventually(move || wait_order.lock().unwrap().len() == 3).await;
    let mut sorted = order.lock().unwrap().clone();
    sorted.sort();
    assert_eq!(sorted, ["faulting", "first", "wait"]);
    until_status(&harness, waiter_id, TaskStatus::Waiting).await;
    let state = harness.get_task(waiter_id, &context()).await.unwrap().unwrap().state;
    assert!(matches!(state, TaskState::Waiting { on, .. } if on == ids));
    gate.resolve();
    harness.wait_for_task(waiter_id, &context()).await.unwrap();
    assert_eq!(order.lock().unwrap().last(), Some(&"resume"));
    assert_eq!(*outcomes.lock().unwrap(), [json!("completed"), json!("faulted")]);
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn stamps_started_at_at_the_first_run_and_ended_at_when_terminal_keeping_started_at_through_a_wait() {
    let clock = Arc::new(Mutex::new(1_000.0));
    let gate = Deferred::default();
    let on = Arc::new(Mutex::new(Vec::new()));
    let waiter = waiter_task(
        "test.timed-waiter",
        on.clone(),
        || {},
        |_, runtime, context| async move { runtime.commit(|_, _| async { Ok(completed(Value::Null)) }, &context).await },
    );
    let held = gated("test.timed-held", gate.clone());
    let now = clock.clone();
    let tasks = vec![waiter.clone(), held.clone()];
    let opened = open_tasks_with(Arc::new(MemoryStorage::new()), tasks, Arc::new(create_registry()), |options| {
        options.with_now(move || *now.lock().unwrap())
    })
    .await;
    let harness = opened.harness;
    let root = harness.root(&context(), CreateOptions::default()).await.unwrap();
    let held_id = start(&root, &held, false).await;
    *on.lock().unwrap() = vec![held_id];
    let waiter_id = start(&root, &waiter, false).await;
    let times = || async {
        let inspection = harness.inspect(&context()).await.unwrap();
        inspection.tasks.iter().map(|t| (t.record.id, t.record.started_at, t.record.ended_at)).collect::<Vec<_>>()
    };
    assert_eq!(times().await, [(held_id, None, None), (waiter_id, None, None)]);
    *clock.lock().unwrap() = 2_000.0;
    harness.resume().unwrap();
    until_status(&harness, waiter_id, TaskStatus::Waiting).await;
    assert_eq!(times().await, [(held_id, Some(2_000.0), None), (waiter_id, Some(2_000.0), None)]);
    // The waiter runs again at 5_000; its start stays the first run.
    *clock.lock().unwrap() = 5_000.0;
    gate.resolve();
    let waiter_record = harness.wait_for_task(waiter_id, &context()).await.unwrap();
    let held_record = harness.get_task(held_id, &context()).await.unwrap().unwrap();
    assert_eq!((held_record.started_at, held_record.ended_at), (Some(2_000.0), Some(5_000.0)));
    assert_eq!((waiter_record.started_at, waiter_record.ended_at), (Some(2_000.0), Some(5_000.0)));
    harness.close(&context()).await.unwrap();
}

fn tool_names(snapshot: &RegistrySnapshot) -> Vec<String> {
    snapshot.tools().iter().map(|(_, tool)| tool.name().to_owned()).collect()
}

#[tokio::test]
async fn keeps_one_registry_snapshot_per_phase_and_refreshes_it_at_the_phase_boundary() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let (phase_gate, entered) = (Deferred::default(), Deferred::default());
    let (gate, signal_entered, sink_a) = (phase_gate.clone(), entered.clone(), seen.clone());
    let sink_b = seen.clone();
    let snapshots = define_task(
        TaskDefinition::new("test.snapshots", 1, |_| Ok(Json::from(json!({ "phase": "a" }))))
            .phase("a", move |_, runtime, context| {
                let (gate, entered, sink) = (gate.clone(), signal_entered.clone(), sink_a.clone());
                async move {
                    let before = runtime.registry();
                    entered.resolve();
                    gate.wait().await;
                    let now = runtime.registry();
                    sink.lock().unwrap().push(("a", tool_names(&now), now.ptr_eq(&before)));
                    runtime.commit(|_, _| async { Ok(checkpoint("b")) }, &context).await
                }
            })
            .phase("b", move |_, runtime, context| {
                sink_b.lock().unwrap().push(("b", tool_names(&runtime.registry()), true));
                async move { runtime.commit(|_, _| async { Ok(completed(Value::Null)) }, &context).await }
            })
            .abort(|_, _, _| async { Ok(()) }),
    );
    let opened = open_tasks(Arc::new(MemoryStorage::new()), vec![snapshots.clone()]).await;
    let harness = opened.harness;
    let root = harness.root(&context(), CreateOptions::default()).await.unwrap();
    let id = start(&root, &snapshots, false).await;
    harness.resume().unwrap();
    entered.wait().await;
    let late = define_tool("late", "late", json!({ "type": "object" }), |_, _, _| async {
        Ok(ToolExecutionResult::default())
    });
    opened.registry.install(Extension::new("tool:late").tool(late)).unwrap();
    phase_gate.resolve();
    harness.wait_for_task(id, &context()).await.unwrap();
    assert_eq!(*seen.lock().unwrap(), [("a", vec![], true), ("b", vec!["late".to_owned()], true)]);
    harness.close(&context()).await.unwrap();
}

/// Task hooks of the agent-phases case: each handler reports its label.
struct PingHooks {
    label: &'static str,
}

async fn ping(runtime: &TaskRuntime, context: &pi_chord::Context) -> DurableResult<Vec<&'static str>> {
    Ok(runtime.hooks::<PingHooks>(context).await?.iter().map(|hooks| hooks.label).collect())
}

#[tokio::test]
async fn resolves_the_agent_at_first_use_in_a_phase_from_its_snapshot_keeps_it_for_the_phase_and_anew_at_the_next() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let gates: [Deferred; 4] = Default::default();
    let held = gates.clone();
    let (sink_a, sink_b) = (seen.clone(), seen.clone());
    let phases = define_task(
        TaskDefinition::new("test.agent-phases", 1, |_| Ok(Json::from(json!({ "phase": "a" }))))
            .phase("a", move |_, runtime, context| {
                let [before_use, after_use, waiting_before, waiting_after] = held.clone();
                let sink = sink_a.clone();
                async move {
                    waiting_before.resolve();
                    before_use.wait().await;
                    let first = runtime.agent(&context).await?;
                    let pings = ping(&runtime, &context).await?;
                    sink.lock().unwrap().push(("a", pings, first.thinking_level));
                    waiting_after.resolve();
                    after_use.wait().await;
                    // Agent is a value in Rust; an unchanged level and hook set stand in for upstream's identity check.
                    let second = runtime.agent(&context).await?;
                    let pings = ping(&runtime, &context).await?;
                    sink.lock().unwrap().push(("a", pings, second.thinking_level));
                    runtime.commit(|_, _| async { Ok(checkpoint("b")) }, &context).await
                }
            })
            .phase("b", move |_, runtime, context| {
                let sink = sink_b.clone();
                async move {
                    let agent = runtime.agent(&context).await?;
                    let pings = ping(&runtime, &context).await?;
                    sink.lock().unwrap().push(("b", pings, agent.thinking_level));
                    runtime.commit(|_, _| async { Ok(completed(Value::Null)) }, &context).await
                }
            })
            .abort(|_, _, _| async { Ok(()) }),
    );
    let opened = open_tasks(Arc::new(MemoryStorage::new()), vec![phases.clone()]).await;
    let add_hooks = |name: &str, label| {
        let extension = Extension::new(name).hook(hook(&phases, PingHooks { label }));
        opened.registry.install(extension).unwrap();
    };
    add_hooks("hooks:before", "before");
    let harness = opened.harness.clone();
    let root = harness.root(&context(), CreateOptions::default()).await.unwrap();
    let id = start(&root, &phases, false).await;
    harness.resume().unwrap();
    let level = |level| AgentChange { thinking_level: Change::Set(level), ..AgentChange::default() };
    // Configured during the phase but before its first use: the lazy resolution reads it.
    gates[2].wait().await;
    root.configure(level(ThinkingLevel::Low), &context()).await.unwrap();
    gates[0].resolve();
    // Configured and installed after the first use: not seen until the next phase.
    gates[3].wait().await;
    add_hooks("hooks:late", "late");
    root.configure(level(ThinkingLevel::High), &context()).await.unwrap();
    gates[1].resolve();
    harness.wait_for_task(id, &context()).await.unwrap();
    assert_eq!(
        *seen.lock().unwrap(),
        [
            ("a", vec!["before"], ThinkingLevel::Low),
            ("a", vec!["before"], ThinkingLevel::Low),
            ("b", vec!["before", "late"], ThinkingLevel::High),
        ]
    );
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn rejects_an_agent_wait_whose_caller_context_is_cancelled_without_affecting_the_phases_resolution() {
    let result = Arc::new(Mutex::new(None));
    let sink = result.clone();
    let agent = one_step("test.agent-cancel", move |_, runtime, context| {
        let sink = sink.clone();
        async move {
            let (caller, controller) = with_cancel(&context);
            controller.abort(Some(ChordError::error("caller gone")));
            let cancelled = runtime.agent(&caller).await.err().map(|error| error.to_string());
            let resolved = runtime.agent(&context).await?.thinking_level;
            *sink.lock().unwrap() = Some((cancelled, resolved));
            runtime.commit(|_, _| async { Ok(completed(Value::Null)) }, &context).await
        }
    });
    let (harness, root, _) = open_root(vec![agent.clone()]).await;
    let id = start(&root, &agent, false).await;
    harness.resume().unwrap();
    harness.wait_for_task(id, &context()).await.unwrap();
    let (cancelled, resolved) = result.lock().unwrap().clone().unwrap();
    assert!(cancelled.unwrap().contains("caller gone"));
    assert_eq!(resolved, ThinkingLevel::Off);
    harness.close(&context()).await.unwrap();
}

// Upstream's case of a failed agent resolution whose only caller stopped waiting guards against an unhandled
// promise rejection from a settings getter that throws. Rust settings are plain values and an unawaited shared
// future reports nothing, so the case has no Rust counterpart.
