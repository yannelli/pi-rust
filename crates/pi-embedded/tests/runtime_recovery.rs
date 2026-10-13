//! Port of upstream `test/runtime.test.ts`: restoration, repair, reconciliation, and storage.

mod common;

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use common::*;
use pi_ai::{ContentBlock, Message, UserContent, UserMessage};
use pi_embedded::{
    InvocationPhase, MemorySessionStore, Reconciliation, SessionSnapshot, SessionStatus, SessionStore, SessionSummary,
    ToolEffect,
};
use serde_json::json;

fn user(text: &str, timestamp: i64) -> Message {
    Message::User(UserMessage { content: UserContent::Text(text.into()), timestamp })
}

#[tokio::test]
async fn requests_approval_again_for_a_restored_pending_mutation() {
    let store = saved(vec![Message::Assistant(call("call-1", "tool"))], InvocationPhase::Pending).await;
    edit_saved(&store, |snapshot| snapshot.invocations[0].approved = Some(true)).await;
    let executions = Arc::new(Mutex::new(0));
    let mut host = host(store, script(vec![answer()], None).stream);
    host.tools = vec![counted(declaration("tool", ToolEffect::Mutation), executions.clone())];
    let session = restore_runtime(host).restore("session").await.unwrap();
    assert_eq!(session.snapshot().invocations[0].approved, None);
    let events = record(&session, Some(true));
    session.resume().await.unwrap();
    let approvals = events
        .lock()
        .unwrap()
        .iter()
        .filter(|e| matches!(e.payload, pi_embedded::EventPayload::Approval { .. }))
        .count();
    assert_eq!(approvals, 1);
    assert_eq!(*executions.lock().unwrap(), 1);
}

#[tokio::test]
async fn restores_a_partial_response_as_interrupted_and_resumes_without_replaying_it() {
    let store = saved(vec![user("hello", 1)], InvocationPhase::Pending).await;
    edit_saved(&store, |snapshot| snapshot.partial = Some(assistant(vec![ContentBlock::text("partial")]))).await;
    let responses = script(vec![answer()], None);
    let session = restore_runtime(host(store, responses.stream.clone())).restore("session").await.unwrap();
    let snapshot = session.snapshot();
    assert_eq!((snapshot.status, snapshot.needs_response), (SessionStatus::Interrupted, true));
    assert!(snapshot.partial.is_none());
    let Some(Message::Assistant(last)) = snapshot.messages.last() else { panic!("expected assistant") };
    assert_eq!((last.stop_reason, last.text()), (pi_ai::StopReason::Aborted, "partial".to_string()));
    session.resume().await.unwrap();
    assert_eq!(responses.requests.lock().unwrap()[0].1.messages.len(), 1);
}

#[tokio::test]
async fn repairs_a_pending_tool_before_requesting_another_response() {
    let store = saved(vec![Message::Assistant(call("call-1", "tool"))], InvocationPhase::Pending).await;
    let executions = Arc::new(Mutex::new(0));
    let responses = script(vec![answer()], None);
    let mut host = host(store, responses.stream.clone());
    host.tools = vec![counted(declaration("tool", ToolEffect::Read), executions.clone())];
    let session = restore_runtime(host).restore("session").await.unwrap();
    session.resume().await.unwrap();
    assert_eq!(*executions.lock().unwrap(), 1);
    assert_eq!(tool_results(&responses.requests.lock().unwrap()[0].1.messages).len(), 1);
}

#[tokio::test]
async fn restores_a_durable_completed_result_without_repeating_its_tool() {
    let store = saved(vec![Message::Assistant(call("call-1", "tool"))], InvocationPhase::Completed).await;
    let executions = Arc::new(Mutex::new(0));
    let mut host = host(store, script(vec![answer()], None).stream);
    host.tools = vec![counted(declaration("tool", ToolEffect::Read), executions.clone())];
    let session = restore_runtime(host).restore("session").await.unwrap();
    session.resume().await.unwrap();
    assert_eq!(*executions.lock().unwrap(), 0);
    assert_eq!(tool_results(&session.snapshot().messages)[0].content, result("saved outcome").content);
}

async fn reconciliation(completed: bool) {
    let store = saved(vec![Message::Assistant(call("call-1", "tool"))], InvocationPhase::Executing).await;
    let executions = Arc::new(Mutex::new(0));
    let mut host = host(store, script(vec![answer()], None).stream);
    host.tools = vec![counted(declaration("tool", ToolEffect::Mutation), executions.clone())];
    let session = restore_runtime(host).restore("session").await.unwrap();
    assert_eq!(session.snapshot().invocations[0].phase, InvocationPhase::Uncertain);
    assert!(session.resume().await.unwrap_err().to_string().contains("Reconcile uncertain"));
    session.reconcile("call-1", Some(Reconciliation::Unknown)).await.unwrap();
    assert_eq!(session.snapshot().invocations[0].phase, InvocationPhase::Uncertain);
    let decision = match completed {
        true => Reconciliation::Completed { result: result("reconciled") },
        false => Reconciliation::NotApplied,
    };
    session.reconcile("call-1", Some(decision)).await.unwrap();
    record(&session, Some(true));
    session.resume().await.unwrap();
    assert_eq!(*executions.lock().unwrap(), if completed { 0 } else { 1 });
    assert_eq!(tool_results(&session.snapshot().messages).len(), 1);
}

#[tokio::test]
async fn requires_reconciliation_of_an_uncertain_mutation_completed() {
    reconciliation(true).await;
}

#[tokio::test]
async fn requires_reconciliation_of_an_uncertain_mutation_not_applied() {
    reconciliation(false).await;
}

/// Fails every checkpoint that records an executing invocation.
struct FailingStore(MemorySessionStore);

#[async_trait]
impl SessionStore for FailingStore {
    async fn load(&self, id: &str) -> anyhow::Result<Option<SessionSnapshot>> {
        self.0.load(id).await
    }

    async fn list(&self) -> anyhow::Result<Vec<SessionSummary>> {
        self.0.list().await
    }

    async fn save(&self, snapshot: &SessionSnapshot, revision: Option<u64>) -> anyhow::Result<()> {
        if snapshot.invocations.iter().any(|invocation| invocation.phase == InvocationPhase::Executing) {
            anyhow::bail!("storage unavailable");
        }
        self.0.save(snapshot, revision).await
    }
}

#[tokio::test]
async fn prevents_effects_when_the_durable_execution_checkpoint_fails() {
    let store = Arc::new(FailingStore(MemorySessionStore::new()));
    let effects = Arc::new(Mutex::new(0));
    let mut host = host(store.clone(), script(vec![call("call-1", "tool"), answer()], None).stream);
    host.tools = vec![counted(declaration("tool", ToolEffect::Read), effects.clone())];
    let session = open(host, "session").await;
    let error = session.submit("execute").await.unwrap_err();
    assert!(error.to_string().contains("storage unavailable"), "{error}");
    assert_eq!(*effects.lock().unwrap(), 0);
    assert_eq!(store.0.load("session").await.unwrap().unwrap().invocations[0].phase, InvocationPhase::Pending);
}

#[tokio::test]
async fn interrupts_the_run_without_ending_the_session_when_a_provider_reuses_a_tool_id() {
    let store = Arc::new(MemorySessionStore::new());
    let responses = script(vec![call("same", "tool"), call("same", "tool"), answer()], None);
    let mut host = host(store.clone(), responses.stream);
    host.tools = vec![counted(declaration("tool", ToolEffect::Read), Arc::default())];
    let session = open(host, "session").await;
    session.submit("first").await.unwrap();
    let snapshot = session.snapshot();
    assert_eq!((snapshot.status, snapshot.needs_response), (SessionStatus::Interrupted, true));
    assert_eq!(snapshot.error.as_deref(), Some("Provider reused tool invocation id"));
    assert_eq!(store.load("session").await.unwrap().unwrap().status, SessionStatus::Interrupted);
    session.resume().await.unwrap();
    let snapshot = session.snapshot();
    assert_eq!((snapshot.status, snapshot.needs_response), (SessionStatus::Idle, false));
}

#[tokio::test]
async fn snapshots_keep_upstream_json_shapes() {
    let store =
        saved(vec![user("hello", 1), Message::Assistant(call("call-1", "tool"))], InvocationPhase::Pending).await;
    let value = serde_json::to_value(store.load("session").await.unwrap().unwrap()).unwrap();
    assert_eq!(value["version"], 1);
    assert_eq!(value["status"], "running");
    assert_eq!(value["needsResponse"], true);
    assert_eq!(value["contextStart"], 0);
    assert_eq!(value["invocations"][0]["sessionId"], "session");
    assert_eq!(value["invocations"][0]["effect"], "mutation");
    assert_eq!(value["messages"][0], json!({"role": "user", "content": "hello", "timestamp": 1}));
    let reparsed: SessionSnapshot = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(serde_json::to_value(reparsed).unwrap(), value);
}
