//! Port of upstream `test/runtime.test.ts`: events, approval, queues, and cancellation.

mod common;

use std::sync::{Arc, Mutex};

use common::*;
use pi_ai::Message;
use pi_embedded::{
    EmbeddedHost, EventPayload, FnTool, InvocationPhase, Lifecycle, LifecycleListener, LifecycleState,
    MemorySessionStore, SessionStatus, SessionStore, ToolEffect,
};
use serde_json::json;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn persists_streamed_events_before_publishing_ordered_versioned_events_and_usage() {
    let store = Arc::new(MemorySessionStore::new());
    let runtime = restore_runtime(host(store.clone(), script(vec![answer()], None).stream));
    let options = pi_embedded::CreateSessionOptions { id: "session".into(), model: model(), system_prompt: None };
    let session = runtime.create(options).await.unwrap();
    let persisted = Arc::new(Mutex::new(Vec::new()));
    let (sink, reader) = (persisted.clone(), store.clone());
    session.subscribe(move |event| {
        let saved = futures::executor::block_on(reader.load(&event.session_id)).unwrap().unwrap();
        sink.lock().unwrap().push((event.clone(), saved.sequence));
    });
    session.submit("hello").await.unwrap();
    let persisted = persisted.lock().unwrap().clone();
    for (index, (event, saved)) in persisted.iter().enumerate() {
        assert_eq!(event.sequence, index as u64 + 1);
        assert_eq!((event.version, event.session_id.as_str()), (1, "session"));
        assert_eq!(*saved, event.sequence, "checkpoint precedes delivery");
    }
    let agent_types: Vec<String> = persisted
        .iter()
        .filter_map(|(event, _)| match &event.payload {
            EventPayload::Agent { event } => {
                Some(serde_json::to_value(event).unwrap()["type"].as_str().unwrap().to_string())
            }
            _ => None,
        })
        .collect();
    assert!(agent_types.contains(&"message_update".to_string()));
    assert_eq!(persisted.iter().filter(|(e, _)| matches!(e.payload, EventPayload::Usage { .. })).count(), 1);
    let snapshot = session.snapshot();
    assert_eq!((snapshot.status, snapshot.needs_response), (SessionStatus::Idle, false));
    assert_eq!((snapshot.usage[0].input, snapshot.usage[0].output), (10, 2));
    let listed = runtime.list().await.unwrap();
    assert_eq!(
        serde_json::to_value(listed).unwrap(),
        json!([{"id": "session", "status": "idle", "revision": snapshot.revision}])
    );
    session.dispose().await.unwrap();
    assert!(session.submit("closed").await.unwrap_err().to_string().contains("disposed"));
}

#[tokio::test]
async fn pauses_for_approval_and_persists_tool_progress_and_results() {
    let store = Arc::new(MemorySessionStore::new());
    let executions = Arc::new(Mutex::new(0));
    let (count, reader) = (executions.clone(), store.clone());
    let tool = FnTool::new(declaration("tool", ToolEffect::Mutation), move |_, context| {
        let (count, reader) = (count.clone(), reader.clone());
        async move {
            let checkpoint = reader.load(&context.session_id).await?.unwrap();
            assert_eq!(checkpoint.invocations[0].phase, InvocationPhase::Executing);
            assert_eq!(checkpoint.invocations[0].approved, Some(true));
            *count.lock().unwrap() += 1;
            context.progress(result("halfway")).await?;
            Ok(result("completed"))
        }
    });
    let mut host = host(store, script(vec![call("call-1", "tool"), answer()], None).stream);
    host.tools = vec![Arc::new(tool)];
    let session = open(host, "session").await;
    let events = record(&session, Some(true));
    session.submit("edit").await.unwrap();
    assert_eq!(*executions.lock().unwrap(), 1);
    let kinds: Vec<&str> = events
        .lock()
        .unwrap()
        .iter()
        .filter_map(|event| match event.payload {
            EventPayload::Approval { .. } => Some("approval"),
            EventPayload::ApprovalDecision { .. } => Some("approval_decision"),
            EventPayload::ToolProgress { .. } => Some("tool_progress"),
            _ => None,
        })
        .collect();
    assert_eq!(kinds, ["approval", "approval_decision", "tool_progress"]);
    let snapshot = session.snapshot();
    let results = tool_results(&snapshot.messages);
    assert_eq!((results[0].tool_call_id.as_str(), results[0].is_error), ("call-1", false));
    assert_eq!(results[0].content, result("completed").content);
    assert_eq!(snapshot.invocations[0].phase, InvocationPhase::Completed);
    assert_eq!(snapshot.invocations[0].result, Some(result("completed")));
}

#[tokio::test]
async fn records_denied_approval_without_executing_the_tool() {
    let executions = Arc::new(Mutex::new(0));
    let mut tool = declaration("tool", ToolEffect::Read);
    tool.approval = true;
    let mut host =
        host(Arc::new(MemorySessionStore::new()), script(vec![call("call-1", "tool"), answer()], None).stream);
    host.tools = vec![counted(tool, executions.clone())];
    let session = open(host, "session").await;
    record(&session, Some(false));
    session.submit("do it").await.unwrap();
    assert_eq!(*executions.lock().unwrap(), 0);
    assert!(tool_results(&session.snapshot().messages)[0].is_error);
    assert_eq!(session.snapshot().status, SessionStatus::Idle);
}

#[tokio::test]
async fn reviews_the_coerced_arguments_that_the_tool_handler_receives() {
    let response = assistant(vec![tool_call("coerced", "tool", json!({"line": "42", "enabled": "false"}))]);
    let executed = Arc::new(Mutex::new(None));
    let mut declared = declaration("tool", ToolEffect::Mutation);
    declared.parameters = json!({
        "type": "object", "properties": {"line": {"type": "integer"}, "enabled": {"type": "boolean"}},
        "required": ["line", "enabled"], "additionalProperties": false
    });
    let sink = executed.clone();
    let tool = FnTool::new(declared, move |args, _| {
        *sink.lock().unwrap() = Some(args);
        async { Ok(result("completed")) }
    });
    let mut host = host(Arc::new(MemorySessionStore::new()), script(vec![response, answer()], None).stream);
    host.tools = vec![Arc::new(tool)];
    let session = open(host, "session").await;
    let events = record(&session, Some(true));
    session.submit("review and run").await.unwrap();
    let reviewed = events.lock().unwrap().iter().find_map(|event| match &event.payload {
        EventPayload::Approval { invocation } => Some(invocation.args.clone()),
        _ => None,
    });
    let expected = json!({"line": 42, "enabled": false}).as_object().cloned();
    assert_eq!(reviewed, expected);
    assert_eq!(*executed.lock().unwrap(), expected);
    assert_eq!(Some(session.snapshot().invocations[0].args.clone()), expected);
}

#[tokio::test]
async fn stops_the_tool_batch_after_an_uncertain_mutation_and_requires_reconciliation() {
    let later = Arc::new(Mutex::new(0));
    let response = assistant(vec![tool_call("mutation", "edit", json!({})), tool_call("later", "later", json!({}))]);
    let edit = FnTool::new(declaration("edit", ToolEffect::Mutation), |_, _| async {
        anyhow::bail!("connection lost after write")
    });
    let mut host = host(Arc::new(MemorySessionStore::new()), script(vec![response], None).stream);
    host.tools = vec![Arc::new(edit), counted(declaration("later", ToolEffect::Read), later.clone())];
    let session = open(host, "session").await;
    record(&session, Some(true));
    session.submit("edit and continue").await.unwrap();
    assert_eq!(*later.lock().unwrap(), 0);
    let snapshot = session.snapshot();
    let mutation = snapshot.invocations.iter().find(|item| item.id == "mutation").unwrap();
    assert_eq!(mutation.phase, InvocationPhase::Uncertain);
    assert!(session.resume().await.unwrap_err().to_string().contains("Reconcile uncertain"));
}

struct ManualLifecycle(Arc<Mutex<Option<LifecycleListener>>>);

impl Lifecycle for ManualLifecycle {
    fn subscribe(&self, listener: LifecycleListener) -> Box<dyn FnOnce() + Send> {
        *self.0.lock().unwrap() = Some(listener);
        Box::new(|| {})
    }
}

#[tokio::test]
async fn checkpoints_suspension_triggered_by_host_lifecycle_events() {
    let responses = script(vec![answer()], Some(CancellationToken::new()));
    let listener = Arc::new(Mutex::new(None));
    let mut host = host(Arc::new(MemorySessionStore::new()), responses.stream.clone());
    host.lifecycle = Some(Arc::new(ManualLifecycle(listener.clone())));
    let session = open(host, "session").await;
    let paused = CancellationToken::new();
    let signal = paused.clone();
    session.subscribe(move |event| {
        if matches!(event.payload, EventPayload::Session { status: SessionStatus::Paused, .. }) {
            signal.cancel();
        }
    });
    let run = session.submit("first");
    responses.started.cancelled().await;
    let notify = listener.lock().unwrap().clone().unwrap();
    notify(LifecycleState::Background);
    run.await.unwrap();
    paused.cancelled().await;
    let snapshot = session.snapshot();
    assert_eq!((snapshot.status, snapshot.needs_response), (SessionStatus::Paused, true));
}

#[tokio::test]
async fn checkpoints_steering_and_follow_ups_and_drains_each_once_in_order() {
    let gate = CancellationToken::new();
    let responses = script(vec![answer(), answer(), answer()], Some(gate.clone()));
    let store = Arc::new(MemorySessionStore::new());
    let session = open(host(store.clone(), responses.stream.clone()), "session").await;
    let run = session.submit("first");
    responses.started.cancelled().await;
    session.steer("steer").await.unwrap();
    session.follow_up("later").await.unwrap();
    let saved = serde_json::to_value(store.load("session").await.unwrap().unwrap()).unwrap();
    assert_eq!(saved["steering"][0]["content"], "steer");
    assert_eq!(saved["followUps"][0]["content"], "later");
    assert_eq!(saved["steering"][0]["role"], "user");
    gate.cancel();
    run.await.unwrap();
    assert_eq!(responses.requests.lock().unwrap().len(), 3);
    assert_eq!(user_texts(&session), ["first", "steer", "later"]);
    let snapshot = session.snapshot();
    assert!(snapshot.steering.is_empty() && snapshot.follow_ups.is_empty());
    assert_eq!(snapshot.status, SessionStatus::Idle);
}

#[tokio::test]
async fn cancels_generation_and_restores_queued_input_after_suspension() {
    let responses = script(vec![answer()], Some(CancellationToken::new()));
    let store = Arc::new(MemorySessionStore::new());
    let session = open(host(store.clone(), responses.stream.clone()), "session").await;
    let run = session.submit("first");
    responses.started.cancelled().await;
    session.follow_up("queued").await.unwrap();
    session.cancel().await.unwrap();
    run.await.unwrap();
    let snapshot = session.snapshot();
    assert_eq!((snapshot.status, snapshot.needs_response), (SessionStatus::Interrupted, true));
    assert_eq!(snapshot.follow_ups.len(), 1);
    let aborted = snapshot.messages.iter().find_map(|message| match message {
        Message::Assistant(assistant) => Some(assistant.stop_reason),
        _ => None,
    });
    assert_eq!(aborted, Some(pi_ai::StopReason::Aborted));
    session.suspend().await.unwrap();
    let next = script(vec![answer(), answer()], None);
    let restored = restore_runtime(host(store.clone(), next.stream.clone())).restore("session").await.unwrap();
    assert_eq!(restored.snapshot().status, SessionStatus::Paused);
    restored.resume().await.unwrap();
    assert_eq!(restored.snapshot().status, SessionStatus::Idle);
    assert!(restored.snapshot().follow_ups.is_empty());
    let requests = next.requests.lock().unwrap();
    let replayed_abort = requests[0].1.messages.iter().any(|message| {
        matches!(message, Message::Assistant(assistant) if assistant.stop_reason == pi_ai::StopReason::Aborted)
    });
    assert!(!replayed_abort);
}

#[tokio::test]
async fn advertises_host_capabilities_and_uses_the_selected_model_on_the_next_request() {
    let gate = CancellationToken::new();
    let responses = script(vec![answer(), answer()], Some(gate.clone()));
    let mut read = declaration("read", ToolEffect::Read);
    read.capabilities = vec!["workspace.read".into()];
    let mut shell = declaration("shell", ToolEffect::Read);
    shell.capabilities = vec!["subprocess".into()];
    let count = Arc::new(Mutex::new(0));
    let mut host: EmbeddedHost = host(Arc::new(MemorySessionStore::new()), responses.stream.clone());
    host.capabilities = vec!["workspace.read".into()];
    host.tools = vec![counted(read, count.clone()), counted(shell, count)];
    let session = open(host, "session").await;
    let run = session.submit("first");
    responses.started.cancelled().await;
    session.select_model(pi_ai::Model { id: "second".into(), ..model() }).await.unwrap();
    session.follow_up("next").await.unwrap();
    gate.cancel();
    run.await.unwrap();
    let requests = responses.requests.lock().unwrap();
    let ids: Vec<&str> = requests.iter().map(|(model, _)| model.id.as_str()).collect();
    assert_eq!(ids, ["test", "second"]);
    let tools: Vec<String> =
        pi_ai::transcript::current_tools(&requests[0].1.messages).into_iter().map(|tool| tool.name).collect();
    assert_eq!(tools, ["read"]);
    assert_eq!(session.snapshot().model.id, "second");
}
