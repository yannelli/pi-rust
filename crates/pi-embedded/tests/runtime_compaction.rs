//! Port of upstream `test/runtime.test.ts`: compaction and workspace tools.

mod common;

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use common::*;
use pi_ai::{ContentBlock, Message, Model, StopReason, SystemMessage, UserContent, UserMessage};
use pi_embedded::{
    Compaction, Compactor, CreateSessionOptions, EmbeddedRuntime, EventPayload, InvocationPhase, MemorySessionStore,
    Reconciliation, SearchMatch, SessionStatus, SessionStore, ToolEffect, ToolInvocation, WorkspaceAdapter,
    WorkspaceDocument, create_workspace_tools,
};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

/// Records compacted contexts; waits on `gate` (or cancellation) when set; fails when `fail`.
#[derive(Clone, Default)]
struct TestCompactor {
    seen: Arc<Mutex<Vec<Vec<Message>>>>,
    summary: String,
    gate: Option<CancellationToken>,
    started: CancellationToken,
    fail: bool,
}

#[async_trait]
impl Compactor for TestCompactor {
    async fn compact(&self, messages: Vec<Message>, _: Model, cancel: CancellationToken) -> anyhow::Result<Compaction> {
        self.seen.lock().unwrap().push(messages);
        self.started.cancel();
        if self.fail {
            anyhow::bail!("compaction provider unavailable");
        }
        if let Some(gate) = &self.gate {
            tokio::select! { _ = gate.cancelled() => {}, _ = cancel.cancelled() => {} }
        }
        Ok(Compaction { summary: self.summary.clone(), usage: None })
    }
}

fn user(text: &str, timestamp: i64) -> Message {
    Message::User(UserMessage { content: UserContent::Text(text.into()), timestamp })
}

fn compactor(summary: &str) -> TestCompactor {
    TestCompactor { summary: summary.into(), ..TestCompactor::default() }
}

#[tokio::test]
async fn compacts_older_turns_while_retaining_the_latest_tool_and_result_group() {
    let responses = script(vec![answer(), call("call-1", "tool"), answer(), answer()], None);
    let compactor = compactor("earlier conversation");
    let mut host = host(Arc::new(MemorySessionStore::new()), responses.stream.clone());
    host.tools = vec![counted(declaration("tool", ToolEffect::Read), Arc::default())];
    host.compactor = Some(Arc::new(compactor.clone()));
    let runtime = EmbeddedRuntime::new(host).unwrap();
    let options =
        CreateSessionOptions { id: "session".into(), model: model(), system_prompt: Some("Be useful".into()) };
    let session = runtime.create(options).await.unwrap();
    session.submit("older turn").await.unwrap();
    session.submit("latest turn").await.unwrap();
    let before = session.snapshot();
    session.compact().await.unwrap();
    let seen = compactor.seen.lock().unwrap()[0].clone();
    assert!(seen.iter().any(|m| matches!(m, Message::User(u) if u.content.text() == "older turn")));
    assert!(tool_results(&seen).is_empty());
    let after = session.snapshot();
    assert_eq!(after.messages, before.messages);
    let retained = &after.messages[after.context_start..];
    assert!(matches!(&retained[0], Message::User(u) if u.content.text() == "latest turn"));
    assert!(tool_results(retained).iter().any(|r| r.tool_call_id == "call-1"));
    session.submit("continue").await.unwrap();
    let requests = responses.requests.lock().unwrap();
    let last = &requests.last().unwrap().1.messages;
    let Message::System(summary) = &last[0] else { panic!("expected the summary system message") };
    assert!(summary.content.text().contains("earlier conversation"));
    assert_eq!(tool_results(last).len(), 1);
}

async fn abort_compaction(operation: &str, status: SessionStatus) {
    let store = Arc::new(MemorySessionStore::new());
    let compactor = TestCompactor { gate: Some(CancellationToken::new()), ..compactor("cancelled summary") };
    let mut host = host(store.clone(), script(vec![answer(), answer()], None).stream);
    host.compactor = Some(Arc::new(compactor.clone()));
    let session = open(host, "session").await;
    session.submit("older request").await.unwrap();
    session.submit("latest request").await.unwrap();
    let before = session.snapshot();
    let events = record(&session, None);
    let compacting = tokio::spawn({
        let session = session.clone();
        async move { session.compact().await }
    });
    compactor.started.cancelled().await;
    let concurrent = session.submit("concurrent request").await.unwrap_err();
    assert!(concurrent.to_string().contains("Session is running"));
    assert_eq!(store.load("session").await.unwrap().unwrap().status, SessionStatus::Running);
    match operation {
        "cancel" => session.cancel().await.unwrap(),
        "suspend" => session.suspend().await.unwrap(),
        _ => session.dispose().await.unwrap(),
    }
    compacting.await.unwrap().unwrap();
    let after = session.snapshot();
    assert_eq!((after.status, after.context_start), (status, before.context_start));
    assert_eq!((after.messages, after.context_summary), (before.messages.clone(), before.context_summary));
    assert!(!events.lock().unwrap().iter().any(|e| matches!(e.payload, EventPayload::Compaction { .. })));
    assert_eq!(store.load("session").await.unwrap().unwrap().status, status);
    if operation == "suspend" {
        let restored = restore_runtime(host_for(store)).restore("session").await.unwrap();
        assert_eq!(restored.snapshot().status, SessionStatus::Paused);
        assert_eq!(restored.snapshot().messages, before.messages);
        restored.resume().await.unwrap();
        assert_eq!(restored.snapshot().status, SessionStatus::Idle);
    }
}

fn host_for(store: Arc<MemorySessionStore>) -> pi_embedded::EmbeddedHost {
    host(store, script(Vec::new(), None).stream)
}

#[tokio::test]
async fn aborts_manual_compaction_on_cancel() {
    abort_compaction("cancel", SessionStatus::Interrupted).await;
}

#[tokio::test]
async fn aborts_manual_compaction_on_suspend() {
    abort_compaction("suspend", SessionStatus::Paused).await;
}

#[tokio::test]
async fn aborts_manual_compaction_on_dispose() {
    abort_compaction("dispose", SessionStatus::Disposed).await;
}

async fn retain_summary(pause: bool) {
    let store = Arc::new(MemorySessionStore::new());
    let gate = CancellationToken::new();
    let compactor = TestCompactor { gate: Some(gate.clone()), ..compactor("completed summary") };
    let mut host = host(store.clone(), script(vec![answer(), answer(), answer()], None).stream);
    host.compactor = Some(Arc::new(compactor.clone()));
    let session = open(host, "session").await;
    session.submit("older request").await.unwrap();
    session.submit("latest request").await.unwrap();
    if pause {
        session.suspend().await.unwrap();
    }
    let status = session.snapshot().status;
    let before = session.snapshot();
    let compacting = tokio::spawn({
        let session = session.clone();
        async move { session.compact().await }
    });
    compactor.started.cancelled().await;
    assert_eq!(session.snapshot().status, SessionStatus::Running);
    gate.cancel();
    compacting.await.unwrap().unwrap();
    let after = session.snapshot();
    assert_eq!(
        (after.status, after.context_start, after.context_summary.as_deref()),
        (status, 2, Some("completed summary"))
    );
    assert_eq!(after.messages, before.messages);
    let saved = store.load("session").await.unwrap().unwrap();
    assert_eq!((saved.status, saved.context_start), (status, 2));
    session.submit("continue after compaction").await.unwrap();
    assert_eq!(session.snapshot().status, SessionStatus::Idle);
}

#[tokio::test]
async fn retains_a_completed_manual_summary_and_releases_execution_from_idle() {
    retain_summary(false).await;
}

#[tokio::test]
async fn retains_a_completed_manual_summary_and_releases_execution_from_paused() {
    retain_summary(true).await;
}

#[tokio::test]
async fn rejects_an_ordinary_compactor_failure_and_releases_execution_without_changing_context() {
    let compactor = TestCompactor { fail: true, ..compactor("unused") };
    let mut host = host(Arc::new(MemorySessionStore::new()), script(vec![answer(), answer(), answer()], None).stream);
    host.compactor = Some(Arc::new(compactor));
    let session = open(host, "session").await;
    session.submit("older request").await.unwrap();
    session.submit("latest request").await.unwrap();
    let before = session.snapshot();
    let error = session.compact().await.unwrap_err();
    assert!(error.to_string().contains("compaction provider unavailable"));
    let after = session.snapshot();
    assert_eq!((after.status, after.context_start), (before.status, before.context_start));
    assert_eq!((after.messages, after.context_summary), (before.messages, before.context_summary));
    session.submit("continue after compactor failure").await.unwrap();
    assert_eq!(session.snapshot().status, SessionStatus::Idle);
}

async fn compact_failed_newest(stop_reason: StopReason) {
    let older = vec![
        user("older request", 1),
        Message::Assistant(assistant(vec![ContentBlock::text("completed older answer")])),
    ];
    let mut newest = assistant(vec![ContentBlock::text("partial newest answer")]);
    newest.stop_reason = stop_reason;
    let latest = vec![user("unfinished request", 2), Message::Assistant(newest)];
    let store = saved([older.clone(), latest.clone()].concat(), InvocationPhase::Pending).await;
    let compactor = compactor("older turn summary");
    let responses = script(vec![answer()], None);
    let mut host = host(store, responses.stream.clone());
    host.compactor = Some(Arc::new(compactor.clone()));
    let session = restore_runtime(host).restore("session").await.unwrap();
    session.compact().await.unwrap();
    assert_eq!(*compactor.seen.lock().unwrap(), vec![older]);
    let snapshot = session.snapshot();
    assert_eq!(snapshot.messages[snapshot.context_start..], latest[..]);
    session.resume().await.unwrap();
    let requests = responses.requests.lock().unwrap();
    let sent = &requests[0].1.messages;
    assert_eq!(sent.len(), 2);
    let Message::System(summary) = &sent[0] else { panic!("expected summary") };
    assert_eq!(summary.content.text(), "\nConversation summary:\nolder turn summary");
    assert_eq!(sent[1], latest[0]);
}

#[tokio::test]
async fn compacts_the_complete_older_turn_when_the_newest_response_is_aborted() {
    compact_failed_newest(StopReason::Aborted).await;
}

#[tokio::test]
async fn compacts_the_complete_older_turn_when_the_newest_response_is_error() {
    compact_failed_newest(StopReason::Error).await;
}

#[tokio::test]
async fn preserves_text_array_system_content_and_named_section_updates_after_compaction() {
    let system = |content: UserContent, sections: Value, timestamp| {
        Message::System(SystemMessage {
            content,
            sections: sections.as_object().cloned(),
            tools_added: None,
            tools_removed: None,
            timestamp,
        })
    };
    let blocks =
        UserContent::Blocks(vec![ContentBlock::text("Base instruction"), ContentBlock::text("Second instruction")]);
    let messages = vec![
        system(blocks, json!({"project": "Original project constraint", "removed": "Removed constraint"}), 1),
        user("older request", 2),
        Message::Assistant(answer()),
        system(UserContent::Text(String::new()), json!({"project": "Use unsaved buffers", "removed": null}), 3),
        user("latest request", 4),
    ];
    let store = saved(messages.clone(), InvocationPhase::Pending).await;
    let responses = script(vec![answer()], None);
    let mut host = host(store, responses.stream.clone());
    host.compactor = Some(Arc::new(compactor("older conversation")));
    let session = restore_runtime(host).restore("session").await.unwrap();
    session.compact().await.unwrap();
    session.resume().await.unwrap();
    let requests = responses.requests.lock().unwrap();
    let Message::System(first) = &requests[0].1.messages[0] else { panic!("expected system") };
    let expected =
        "Base instruction\nSecond instruction\n\nUse unsaved buffers\nConversation summary:\nolder conversation";
    assert_eq!(first.content.text(), expected);
    assert_eq!(requests[0].1.messages[1], messages[4]);
}

struct EditorWorkspace {
    document: Mutex<WorkspaceDocument>,
    edits: Mutex<Vec<(String, String)>>,
}

#[async_trait]
impl WorkspaceAdapter for EditorWorkspace {
    async fn read(&self, _: &str, _: CancellationToken) -> anyhow::Result<WorkspaceDocument> {
        Ok(self.document.lock().unwrap().clone())
    }

    async fn search(&self, query: &str, _: CancellationToken) -> anyhow::Result<Vec<SearchMatch>> {
        let document = self.document.lock().unwrap();
        Ok(vec![SearchMatch { path: document.path.clone(), line: 1, text: format!("{query}: {}", document.text) }])
    }

    async fn edit(
        &self,
        path: &str,
        expected: &str,
        text: &str,
        operation: &str,
        _: CancellationToken,
    ) -> anyhow::Result<WorkspaceDocument> {
        let mut document = self.document.lock().unwrap();
        assert_eq!(expected, document.revision);
        self.edits.lock().unwrap().push((expected.into(), operation.into()));
        *document = WorkspaceDocument { path: path.into(), text: text.into(), revision: "buffer-8".into() };
        Ok(document.clone())
    }

    async fn reconcile_edit(&self, _: ToolInvocation, _: CancellationToken) -> anyhow::Result<Reconciliation> {
        Ok(Reconciliation::NotApplied)
    }

    fn supports_diagnostics(&self) -> bool {
        true
    }

    async fn diagnostics(&self, path: &str, _: CancellationToken) -> anyhow::Result<Value> {
        Ok(json!([{"path": path, "message": "editor diagnostic"}]))
    }

    fn supports_navigation(&self) -> bool {
        true
    }

    async fn navigate(&self, path: &str, line: u64, _: CancellationToken) -> anyhow::Result<Value> {
        Ok(json!({"path": path, "line": line, "symbol": "openDocument"}))
    }
}

#[tokio::test]
async fn uses_host_document_contents_diagnostics_navigation_and_revision_checked_edits() {
    let document =
        WorkspaceDocument { path: "editor.ts".into(), text: "unsaved buffer".into(), revision: "buffer-7".into() };
    let workspace = Arc::new(EditorWorkspace { document: Mutex::new(document), edits: Mutex::default() });
    let tools = create_workspace_tools(workspace.clone());
    let mut limited = host(Arc::new(MemorySessionStore::new()), script(Vec::new(), None).stream);
    limited.capabilities = vec!["workspace.read".into()];
    limited.tools = tools.clone();
    let names: Vec<String> =
        EmbeddedRuntime::new(limited).unwrap().available_tools().iter().map(|t| t.declaration().name.clone()).collect();
    assert_eq!(names, ["read_file"]);
    let response = assistant(vec![
        tool_call("read", "read_file", json!({"path": "editor.ts"})),
        tool_call("search", "search_files", json!({"query": "buffer"})),
        tool_call("diagnostics", "diagnostics", json!({"path": "editor.ts"})),
        tool_call("navigate", "navigate", json!({"path": "editor.ts", "line": 3})),
        tool_call(
            "edit",
            "edit_file",
            json!({"path": "editor.ts", "expectedRevision": "buffer-7", "text": "reviewed edit"}),
        ),
    ]);
    let mut full = host(Arc::new(MemorySessionStore::new()), script(vec![response, answer()], None).stream);
    full.capabilities =
        ["read", "search", "edit", "diagnostics", "navigate"].map(|c| format!("workspace.{c}")).to_vec();
    full.tools = tools;
    let session = open(full, "editor").await;
    let events = record(&session, Some(true));
    session.submit("read unsaved contents and apply a reviewed edit").await.unwrap();
    let reviewed = events.lock().unwrap().iter().find_map(|event| match &event.payload {
        EventPayload::Approval { invocation } => Some(Value::Object(invocation.args.clone())),
        _ => None,
    });
    assert_eq!(reviewed, Some(json!({"path": "editor.ts", "expectedRevision": "buffer-7", "text": "reviewed edit"})));
    let snapshot = session.snapshot();
    let outcomes = tool_results(&snapshot.messages);
    assert_eq!(outcomes.len(), 5);
    let details = |id: &str| outcomes.iter().find(|r| r.tool_call_id == id).unwrap().details.clone().unwrap();
    assert_eq!(details("read"), json!({"path": "editor.ts", "text": "unsaved buffer", "revision": "buffer-7"}));
    assert_eq!(details("diagnostics"), json!([{"path": "editor.ts", "message": "editor diagnostic"}]));
    assert_eq!(details("navigate"), json!({"path": "editor.ts", "line": 3, "symbol": "openDocument"}));
    assert_eq!(workspace.document.lock().unwrap().text, "reviewed edit");
    assert_eq!(*workspace.edits.lock().unwrap(), vec![("buffer-7".to_string(), r#"["editor","edit"]"#.to_string())]);
}
