//! Owned conversations from tools and supervisors.

use std::sync::{Arc, LazyLock, Mutex};

use pi_ai::Message;
use pi_chord::{Context, Json};
use pi_durable::documents::{DocSpec, DocToken, LatestConversation, LatestFork, define_doc};
use pi_durable::harness::live::LIVE_DOC;
use pi_durable::harness::*;
use pi_durable::session::Tx;
use pi_durable::storage::MemoryStorage;
use pi_durable::storage::sqlite::open_sqlite_storage;
use pi_durable::tasks::TaskOptions;
use pi_durable::testing::faux::FauxResponseStep;
use pi_durable::*;
use serde_json::{Value, json};
use tokio::task::JoinHandle;

use super::support::chat::{add_tool, answer, call, calls, no_parameters};
use super::support::harness_support::{ChatSetup, all_entries, chat_setup, faux_model, open_chat};
use super::support::task_support::{aborted, completed};
use super::support::{Deferred, context, flush};
use super::{Board, checkpoint, hold_task};

fn tool<F, Fut>(name: &str, execute: F) -> ToolRegistration
where
    F: Fn(pi_ai::JsonObject, ToolExecutionApi, Context) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = DurableResult<ToolExecutionResult>> + Send + 'static,
{
    define_tool(name, name, no_parameters(), execute)
}

/// Create a conversation owned by the calling tool's task, configured with the faux model when `model` is set.
async fn owned_conversation(api: &ToolExecutionApi, model: bool, context: &Context) -> DurableResult<ConversationId> {
    let owner = api.task_id;
    let create = move |tx: Tx| async move {
        let created = tx.create_conversation(ConversationOwnership::Task { task_id: owner }).await?;
        if model {
            configure(&tx, created.id, &AgentChange::model(faux_model())).await?;
        }
        Ok(created.id)
    };
    api.commit(create, context).await
}

async fn cancelled(context: &Context) -> DurableResult<ToolExecutionResult> {
    let signal = context.abort_signal().unwrap();
    aborted(signal.clone()).await;
    Err(signal.reason().map_or_else(DurableError::aborted, DurableError::from))
}

fn status_text(status: SubmissionStatus) -> String {
    serde_json::to_value(status).unwrap().as_str().unwrap().to_owned()
}

/// A faux response held until cancellation; `reached` resolves when the request is sent.
struct Gated {
    step: FauxResponseStep,
    reached: Deferred,
}

fn gated() -> Gated {
    let reached = Deferred::default();
    let signal = reached.clone();
    let step = FauxResponseStep::async_factory(move |_, options, _, _| {
        let (reached, cancel) = (signal.clone(), options.signal.clone().unwrap());
        Box::pin(async move {
            reached.resolve();
            // Never released in these cases: the request ends by cancellation.
            aborted(cancel.clone()).await;
            Err(cancel.reason().map_or_else(|| "aborted".to_owned(), |reason| reason.to_string()))
        })
    });
    Gated { step, reached }
}

fn child_input() -> SubmissionDraft {
    SubmissionDraft::input("child task")
}

async fn user_entries(harness: &Harness, id: ConversationId) -> usize {
    let child = harness.conversation(id, &context()).await.unwrap().unwrap();
    all_entries(&child).await.iter().filter(|entry| entry.kind == "pi.user").count()
}

type Slot<T> = Arc<Mutex<Option<T>>>;

#[tokio::test]
async fn gives_a_tool_an_invocation_bound_handle_whose_submissions_stay_durable_after_the_call_ends() {
    let setup = chat_setup(Default::default());
    let (handle, missing, submission): (Slot<ConversationHandle>, Slot<bool>, Slot<Submission>) = Default::default();
    let (handle_sink, missing_sink, submission_sink) = (handle.clone(), missing.clone(), submission.clone());
    add_tool(
        &setup,
        tool("delegate", move |_, api, context| {
            let (handle_sink, missing_sink, submission_sink) =
                (handle_sink.clone(), missing_sink.clone(), submission_sink.clone());
            async move {
                let child = owned_conversation(&api, true, &context).await?;
                let absent = api.conversation(ConversationId::from(99_999), &context).await?;
                *missing_sink.lock().unwrap() = Some(absent.is_none());
                let handle = api.conversation(child, &context).await?.unwrap();
                *handle_sink.lock().unwrap() = Some(handle.clone());
                let submitted = handle.submit(child_input().with_request_id("child"), &context).await?;
                *submission_sink.lock().unwrap() = Some(submitted.clone());
                let settled = submitted.wait(&context).await?;
                Ok(ToolExecutionResult::text(status_text(settled.status)))
            }
        }),
    );
    setup.faux.set_responses([call("delegate", "c1"), answer("child answer"), answer("done")]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    root.submit(SubmissionDraft::input("go"), &context()).await.unwrap().wait(&context()).await.unwrap();
    assert_eq!(*missing.lock().unwrap(), Some(true));
    // The call ended: the handle and its submission reject, while the submission record remains.
    let handle = handle.lock().unwrap().clone().unwrap();
    let submission = submission.lock().unwrap().clone().unwrap();
    let ended = |error: DurableError| assert!(error.to_string().contains("invocation has ended"), "{error}");
    ended(handle.submit(SubmissionDraft::input("again"), &context()).await.err().unwrap());
    ended(handle.wait_for_idle(&context()).await.unwrap_err());
    ended(handle.abort(&context(), false).await.unwrap_err());
    ended(submission.wait(&context()).await.unwrap_err());
    // Nothing was admitted or marked after the call ended.
    assert_eq!(user_entries(&harness, handle.id).await, 1);
    let inspection = harness.inspect(&context()).await.unwrap();
    assert!(inspection.tasks.iter().all(|task| task.record.conversation_id != handle.id));
    let record = harness.submission(submission.id, &context()).await.unwrap().unwrap();
    assert_eq!(record.status(&context()).await.unwrap().status, SubmissionStatus::Done);
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn rejects_a_handle_operation_queued_on_the_line_when_the_invocation_ends_before_it_runs() {
    let setup = chat_setup(Default::default());
    let (ready, go, queued) = (Deferred::default(), Deferred::default(), Deferred::default());
    let ids: Slot<(ConversationId, TaskId)> = Slot::default();
    let submitting: Slot<JoinHandle<DurableResult<Submission>>> = Slot::default();
    let (ready_signal, go_gate, queued_signal) = (ready.clone(), go.clone(), queued.clone());
    let (ids_sink, submitting_sink) = (ids.clone(), submitting.clone());
    add_tool(
        &setup,
        tool("delegate", move |_, api, context| {
            let (ready, go, queued) = (ready_signal.clone(), go_gate.clone(), queued_signal.clone());
            let (ids_sink, submitting_sink) = (ids_sink.clone(), submitting_sink.clone());
            async move {
                let child = owned_conversation(&api, false, &context).await?;
                let handle = api.conversation(child, &context).await?.unwrap();
                *ids_sink.lock().unwrap() = Some((child, api.task_id));
                ready.resolve();
                go.wait().await;
                // Called while the invocation is alive, with a context that is not the call's: the handle binds it to
                // the invocation. It reaches the line only after the abort mark.
                let late =
                    tokio::spawn(async move { handle.submit(SubmissionDraft::input("late"), &super::context()).await });
                *submitting_sink.lock().unwrap() = Some(late);
                queued.resolve();
                cancelled(&context).await
            }
        }),
    );
    setup.faux.set_responses([call("delegate", "c1")]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    root.submit(SubmissionDraft::input("go"), &context()).await.unwrap();
    ready.wait().await;
    let (child, task_id) = ids.lock().unwrap().unwrap();
    // Hold the Session line, queue the abort mark behind it, then let the tool queue its submit.
    let release = Deferred::default();
    let gate = release.clone();
    let holder = root.clone();
    let hold_line = move |_| async move {
        gate.wait().await;
        Ok(())
    };
    let holding = tokio::spawn(async move { holder.commit(hold_line, &context()).await });
    flush().await;
    let aborter = harness.clone();
    let aborting = tokio::spawn(async move { aborter.abort_task(task_id, &context()).await });
    flush().await;
    go.resolve();
    queued.wait().await;
    flush().await;
    release.resolve();
    holding.await.unwrap().unwrap();
    aborting.await.unwrap().unwrap();
    let submitting = submitting.lock().unwrap().take().unwrap();
    assert!(submitting.await.unwrap().is_err());
    let child_conversation = harness.conversation(child, &context()).await.unwrap().unwrap();
    assert!(all_entries(&child_conversation).await.is_empty());
    let inspection = harness.inspect(&context()).await.unwrap();
    assert!(inspection.submissions.iter().all(|record| record.conversation_id != child));
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn aborts_a_subagent_run_with_its_parent_s_conversation_rejecting_the_waiting_tool() {
    let setup = chat_setup(Default::default());
    let child: Slot<ConversationId> = Slot::default();
    let tool_wait = Deferred::default();
    let (child_sink, wait_signal) = (child.clone(), tool_wait.clone());
    add_tool(
        &setup,
        tool("delegate", move |_, api, context| {
            let (child_sink, wait_signal) = (child_sink.clone(), wait_signal.clone());
            async move {
                let child = owned_conversation(&api, true, &context).await?;
                *child_sink.lock().unwrap() = Some(child);
                let handle = api.conversation(child, &context).await?.unwrap();
                let submission = handle.submit(child_input(), &context).await?;
                match submission.wait(&context).await {
                    Ok(settled) => Ok(ToolExecutionResult::text(status_text(settled.status))),
                    Err(error) => {
                        assert!(!error.to_string().is_empty());
                        wait_signal.resolve();
                        Err(error)
                    }
                }
            }
        }),
    );
    let child_run = gated();
    setup.faux.set_responses([call("delegate", "c1"), child_run.step]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    let input = root.submit(SubmissionDraft::input("go"), &context()).await.unwrap();
    child_run.reached.wait().await;
    root.abort(&context(), false).await.unwrap();
    let record = input.wait(&context()).await.unwrap();
    assert_eq!((record.status, record.reason.as_deref()), (SubmissionStatus::Unanswered, Some("aborted")));
    tool_wait.wait().await;
    let child = child.lock().unwrap().unwrap();
    harness.conversation(child, &context()).await.unwrap().unwrap().wait_for_idle(&context()).await.unwrap();
    let live = serde_json::to_value(harness.snapshot(&LIVE_DOC, child, &context()).await.unwrap().unwrap()).unwrap();
    assert_eq!(live.as_object().unwrap().keys().collect::<Vec<_>>(), ["runFailure"]);
    assert_eq!(live["runFailure"]["reason"], json!("aborted"));
    let inspection = harness.inspect(&context()).await.unwrap();
    assert!(inspection.submissions.iter().all(|record| record.conversation_id != child));
    harness.wait_for_idle(&context()).await.unwrap();
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn lets_a_running_tool_abort_its_owned_child_and_continue() {
    let setup = chat_setup(Default::default());
    let child_run = gated();
    let reached = child_run.reached.clone();
    add_tool(
        &setup,
        tool("delegate", move |_, api, context| {
            let reached = reached.clone();
            async move {
                let child = owned_conversation(&api, true, &context).await?;
                let handle = api.conversation(child, &context).await?.unwrap();
                let submission = handle.submit(child_input(), &context).await?;
                reached.wait().await;
                handle.abort(&context, false).await?;
                let settled = submission.wait(&context).await?;
                Ok(ToolExecutionResult::text(format!("child {}", status_text(settled.status))))
            }
        }),
    );
    setup.faux.set_responses([call("delegate", "c1"), child_run.step, answer("done")]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    let settled = root.submit(SubmissionDraft::input("go"), &context()).await.unwrap().wait(&context()).await.unwrap();
    assert_eq!(settled.status, SubmissionStatus::Done);
    harness.close(&context()).await.unwrap();
}

fn sqlite_path(directory: &tempfile::TempDir) -> String {
    directory.path().join("session.sqlite").to_string_lossy().into_owned()
}

async fn open_sqlite(path: &str, setup: &ChatSetup) -> (Harness, Conversation) {
    open_chat(Arc::new(open_sqlite_storage(path).await.unwrap()), setup).await
}

fn tool_result_errors(entries: &[EntryRecord]) -> Vec<bool> {
    let results = entries.iter().filter(|entry| entry.kind == "pi.tool-result");
    results
        .filter_map(|entry| match entry.model.as_ref()?.first()? {
            Message::ToolResult(result) => Some(result.is_error),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn aborts_the_children_of_a_tool_call_that_throws_or_is_interrupted_while_the_run_continues() {
    let directory = tempfile::tempdir().unwrap();
    let path = sqlite_path(&directory);
    let children = Arc::new(Mutex::new(Vec::<TaskId>::new()));
    let tool_running = Deferred::default();
    let setup = chat_setup(Default::default());
    let hold = hold_task(&Board::default());
    setup.registry.install(Extension::new("task:test.hold").task(hold.clone())).unwrap();
    let (sink, running) = (children.clone(), tool_running.clone());
    let parameters = json!({ "type": "object", "properties": { "mode": { "type": "string" } } });
    let spawn =
        define_tool("spawn", "Starts owned work, then throws or hangs", parameters, move |args, api, context| {
            let (hold, sink, running) = (hold.clone(), sink.clone(), running.clone());
            async move {
                let (owner, name) = (api.task_id, format!("child.{}", api.call_id));
                let create = move |tx: Tx| async move {
                    let created = tx.create_conversation(ConversationOwnership::Task { task_id: owner }).await?;
                    tx.create_task(hold, Json::from(json!({ "name": name })), TaskOptions::conversation(created.id))
                        .await
                };
                let created = api.commit(create, &context).await?;
                sink.lock().unwrap().push(created);
                if args.get("mode") == Some(&Value::from("throw")) {
                    return Err(DurableError::error("spawn failed"));
                }
                running.resolve();
                cancelled(&context).await
            }
        });
    add_tool(&setup, spawn);
    let mode = |mode: &str, id: &str| calls(&[("spawn", json!({ "mode": mode }), id)]);
    setup.faux.set_responses([mode("throw", "c1"), answer("after throw"), mode("hang", "c2")]);
    let (harness, root) = open_sqlite(&path, &setup).await;
    let first = root.submit(SubmissionDraft::input("one"), &context()).await.unwrap().wait(&context()).await.unwrap();
    assert_eq!(first.status, SubmissionStatus::Done);
    let first_child = children.lock().unwrap()[0];
    assert_eq!(super::settles(&harness, first_child).await, "aborted");

    // The second call hangs until the process stops; on reopen it is interrupted.
    let second = root.submit(SubmissionDraft::input("two"), &context()).await.unwrap().id;
    tool_running.wait().await;
    harness.close(&context()).await.unwrap();
    setup.faux.set_responses([answer("after interrupt")]);
    let (harness, _) = open_sqlite(&path, &setup).await;
    harness.resume().unwrap();
    let second_child = children.lock().unwrap()[1];
    assert_eq!(super::settles(&harness, second_child).await, "aborted");
    let settled = harness.submission(second, &context()).await.unwrap().unwrap().wait(&context()).await.unwrap();
    assert_eq!(settled.status, SubmissionStatus::Done);
    let query = TaskQuery { kind: Some("pi.tool".into()), ..TaskQuery::default() };
    let tools =
        harness.commit(move |tx| async move { tx.scan_tasks(query, 10, None).await }, &context()).await.unwrap();
    let outcomes: Vec<_> = tools.items.iter().map(|task| task.state.outcome().map(TaskOutcome::status)).collect();
    assert_eq!(outcomes, [Some("failed"), Some("failed")]);
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn reruns_a_replay_safe_subagent_tool_after_a_restart_with_the_same_child_and_submission() {
    let directory = tempfile::tempdir().unwrap();
    let path = sqlite_path(&directory);
    let children = Arc::new(Mutex::new(Vec::<ConversationId>::new()));
    let setup = chat_setup(Default::default());
    let sink = children.clone();
    let subagent = tool("subagent", move |_, api, context| {
        let sink = sink.clone();
        async move {
            let owner = api.task_id;
            let find = move |tx: Tx| async move {
                let query = ConversationQuery { owner_task_id: Some(owner), ..ConversationQuery::default() };
                if let Some(existing) = tx.scan_conversations(query, 1, None).await?.items.first() {
                    return Ok(existing.id);
                }
                let created = tx.create_conversation(ConversationOwnership::Task { task_id: owner }).await?;
                configure(&tx, created.id, &AgentChange::model(faux_model())).await?;
                Ok(created.id)
            };
            let child = api.commit(find, &context).await?;
            sink.lock().unwrap().push(child);
            let request = child_input().with_request_id(format!("subagent:{}", api.task_id));
            let submission = api.conversation(child, &context).await?.unwrap().submit(request, &context).await?;
            let settled = submission.wait(&context).await?;
            Ok(ToolExecutionResult::text(status_text(settled.status)))
        }
    });
    add_tool(&setup, subagent.with_replay(ToolReplay::Safe));
    let child_run = gated();
    setup.faux.set_responses([call("subagent", "c1"), child_run.step]);
    let (harness, root) = open_sqlite(&path, &setup).await;
    let input = root.submit(SubmissionDraft::input("go"), &context()).await.unwrap().id;
    child_run.reached.wait().await;
    harness.close(&context()).await.unwrap();

    setup.faux.set_responses([answer("child answer"), answer("done")]);
    let (harness, root) = open_sqlite(&path, &setup).await;
    let settled = harness.submission(input, &context()).await.unwrap().unwrap().wait(&context()).await.unwrap();
    assert_eq!(settled.status, SubmissionStatus::Done);
    let children = children.lock().unwrap().clone();
    assert_eq!((children.len(), children[1]), (2, children[0]));
    assert_eq!(user_entries(&harness, children[0]).await, 1);
    assert_eq!(tool_result_errors(&all_entries(&root).await), [false]);
    harness.close(&context()).await.unwrap();
}

static CHILDREN: LazyLock<DocToken<Json, LatestConversation>> = LazyLock::new(|| {
    let spec = DocSpec::new(
        "test.children",
        1,
        LatestConversation(LatestFork::Initial),
        || Json::from(json!({})),
        |_, _, _| Ok(false),
    );
    define_doc(spec).unwrap()
});

fn supervisor_task() -> Task {
    define_task(
        TaskDefinition::new("test.supervisor", 1, |_| Ok(checkpoint("run")))
            .phase("run", |task, runtime, context| async move {
                let parent: ConversationId = serde_json::from_value(task.input["parent"].to_serde()).unwrap();
                let children = runtime.snapshot(&CHILDREN, parent, &context).await?.unwrap();
                let id: ConversationId = serde_json::from_value(children["child"].to_serde()).unwrap();
                let child = runtime.conversation(id, &context).await?.unwrap();
                let submission =
                    child.submit(SubmissionDraft::input("work").with_request_id("stable"), &context).await?;
                let settled = submission.wait(&context).await?;
                if settled.status != SubmissionStatus::Done || settled.kind != SubmissionType::Input {
                    return Err(DurableError::error(status_text(settled.status)));
                }
                let result = json!({ "answer": settled.answer });
                runtime.commit(move |_, _| async move { Ok(completed(result)) }, &context).await
            })
            .abort(|_, runtime, context| async move {
                runtime.commit(|_, _| async { Ok(super::aborted_state()) }, &context).await
            }),
    )
}

#[tokio::test]
async fn lets_a_background_supervisor_resubmit_after_a_restart_without_submitting_twice() {
    let directory = tempfile::tempdir().unwrap();
    let path = sqlite_path(&directory);
    let setup = chat_setup(Default::default());
    let supervisor = supervisor_task();
    setup.registry.install(Extension::new("task:test.supervisor").task(supervisor.clone())).unwrap();
    let first = gated();
    setup.faux.set_responses([first.step, answer("answer")]);
    let (harness, root) = open_sqlite(&path, &setup).await;
    let parent = root.id;
    let stage = move |tx: Tx| async move {
        let input = Json::from(json!({ "parent": parent }));
        let options = TaskOptions { background: true, ..TaskOptions::conversation(parent) };
        let supervisor = tx.create_task(supervisor, input, options).await?;
        let created = tx.create_conversation(ConversationOwnership::Task { task_id: supervisor }).await?;
        configure(&tx, created.id, &AgentChange::model(faux_model())).await?;
        tx.doc(&CHILDREN, parent).await?.set(&Json::from(json!({ "child": created.id })))?;
        Ok((supervisor, created.id))
    };
    let (supervisor, child) = root.commit(stage, &context()).await.unwrap();
    harness.resume().unwrap();
    // The submission is admitted and its generation requested; then the process stops.
    first.reached.wait().await;
    harness.close(&context()).await.unwrap();

    let (harness, root) = open_sqlite(&path, &setup).await;
    harness.resume().unwrap();
    assert_eq!(super::settles(&harness, supervisor).await, "completed");
    assert_eq!(user_entries(&harness, child).await, 1);
    // The root never waited for the background supervisor.
    root.wait_for_idle(&context()).await.unwrap();
    harness.close(&context()).await.unwrap();
}
