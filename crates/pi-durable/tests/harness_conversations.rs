//! Port of upstream `test/harness-conversations.test.ts`. This file holds the root, conversation, and lifecycle
//! cases; `harness_conversations/agents.rs` holds forks, history paging, and the agent cases.

#[path = "harness_conversations/agents.rs"]
mod agents;
mod support;

use std::sync::{Arc, LazyLock, Mutex};

use pi_ai::{Message, ThinkingLevel, UserContent, UserMessage};
use pi_chord::Json;
use pi_durable::documents::*;
use pi_durable::harness::live::LIVE_DOC;
use pi_durable::harness::usage::PROVIDER_DOC;
use pi_durable::harness::*;
use pi_durable::session::Tx;
use pi_durable::storage::{MemoryStorage, open_sqlite_storage};
use pi_durable::tasks::TaskOptions;
use pi_durable::*;
use serde::{Deserialize, Serialize};
use serde_json::json;
use support::harness_support::*;
use support::{ControlledStorage, context};

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
struct Note {
    text: String,
}

static NOTE_DOC: LazyLock<DocToken<Note, RewindableConversation>> = LazyLock::new(|| {
    define_doc(DocSpec::new("test.note", 1, RewindableConversation(DocumentFork::AsOf), Note::default)).unwrap()
});

pub fn user(text: &str) -> Message {
    Message::User(UserMessage { content: UserContent::Text(text.into()), timestamp: 1 })
}

pub fn noop_tool(name: &str) -> ToolRegistration {
    define_tool(name, format!("{name} tool"), json!({ "type": "object" }), |_, _, _| async {
        Ok(ToolExecutionResult { output: Some(Vec::new()), ..ToolExecutionResult::default() })
    })
}

/// Open a Harness with a fresh registry holding the named tools.
pub async fn open_harness(storage: Arc<dyn Storage>, tools: &[&str]) -> (Harness, Arc<Registry>) {
    let registry = Arc::new(create_registry());
    if !tools.is_empty() {
        let extension = tools.iter().fold(Extension::new("tools"), |extension, name| extension.tool(noop_tool(name)));
        registry.install(extension).unwrap();
    }
    let models = chat_setup(Default::default()).models;
    let options = HarnessOptions::new(models, registry.clone() as Arc<dyn RegistryReader>);
    (Harness::open(storage, options, &context()).await.unwrap(), registry)
}

pub async fn append(conversation: &Conversation, text: &str) -> EntryRecord {
    let (id, model) = (conversation.id, vec![user(text)]);
    let draft = EntryDraft { model: Some(model), ..EntryDraft::new("message") };
    conversation.commit(move |tx: Tx| async move { tx.append_entry(id, draft).await }, &context()).await.unwrap()
}

/// Message texts of every entry, newest first, paging two at a time.
pub async fn texts(conversation: &Conversation) -> Vec<String> {
    let mut texts = Vec::new();
    let mut cursor = None;
    loop {
        let page = conversation.entries(EntryRange::default(), 2, cursor, &context()).await.unwrap();
        texts.extend(page.items.iter().filter_map(|entry| text_of(entry.model.as_ref()?.first())));
        cursor = page.next;
        if cursor.is_none() {
            return texts;
        }
    }
}

fn write_types(writes: &[StorageWrite]) -> Vec<String> {
    writes.iter().map(|write| serde_json::to_value(write).unwrap()["type"].as_str().unwrap().to_owned()).collect()
}

pub fn is_uuid_v7(value: &str) -> bool {
    let parts: Vec<&str> = value.split('-').collect();
    parts.iter().map(|part| part.len()).eq([8, 4, 4, 4, 12]) && parts[2].starts_with('7')
}

async fn provider_session(harness: &Harness, id: ConversationId) -> String {
    let value = harness.snapshot(&PROVIDER_DOC, id, &context()).await.unwrap().unwrap();
    value.get("sessionId").and_then(Json::as_str).unwrap().to_owned()
}

#[tokio::test]
async fn creates_the_root_lazily_with_its_agent_change_and_init_in_one_commit() {
    let storage = Arc::new(ControlledStorage::default());
    let (harness, _) = open_harness(storage.clone(), &["read", "bash"]).await;
    assert_eq!(storage.commit_count(), 0);
    let change = AgentChange { thinking_level: Change::Set(ThinkingLevel::High), ..AgentChange::default() };
    let options = CreateOptions::default().with_agent(change).with_init(|tx, id| {
        Box::pin(async move {
            // The agent change applied before init.
            let agent = tx.doc(&AGENT_DOC, id).await?.get()?;
            assert_eq!(agent.thinking_level, Some(ThinkingLevel::High));
            tx.doc(&NOTE_DOC, id).await?.set(&Note { text: "root note".into() })
        })
    });
    let root = harness.root(&context(), options).await.unwrap();
    assert_eq!(root.id, ROOT_CONVERSATION_ID);
    assert_eq!(storage.commit_count(), 1);
    // Conversation, five built-in documents, and the init note.
    let mut expected = vec!["conversation"];
    expected.extend(["document.create"; 6]);
    assert_eq!(write_types(&storage.commits()[0]), expected);
    assert_eq!(harness.snapshot(&LIVE_DOC, root.id, &context()).await.unwrap(), Some(Json::from(json!({}))));
    assert!(is_uuid_v7(&provider_session(&harness, root.id).await));
    let agent_doc = harness.snapshot(&AGENT_DOC, root.id, &context()).await.unwrap().unwrap();
    assert_eq!(agent_doc.to_serde(), json!({ "thinkingLevel": "high" }));
    let agent = root.agent(&context()).await.unwrap();
    assert_eq!(agent.thinking_level, ThinkingLevel::High);
    assert_eq!(agent.tool_names(), ["read", "bash"]);
    let note = harness.snapshot(&NOTE_DOC, root.id, &context()).await.unwrap().unwrap();
    assert_eq!(note.to_serde(), json!({ "text": "root note" }));

    let change = AgentChange { thinking_level: Change::Set(ThinkingLevel::Low), ..AgentChange::default() };
    let options = CreateOptions::default().with_agent(change).with_init(|_, _| unreachable!());
    let again = harness.root(&context(), options).await.unwrap();
    assert_eq!(again.id, root.id);
    assert_eq!(storage.commit_count(), 1);
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn keeps_root_and_conversation_identity_and_state_across_reopen() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.sqlite");
    let path = path.to_str().unwrap();
    let (harness, _) = open_harness(Arc::new(open_sqlite_storage(path).await.unwrap()), &["read"]).await;
    let root = harness.root(&context(), CreateOptions::default()).await.unwrap();
    let model = ModelRef { provider: "anthropic".into(), model_id: "claude".into() };
    let change = AgentChange { cwd: Change::Set("/repo".into()), ..AgentChange::model(model.clone()) };
    root.configure(change, &context()).await.unwrap();
    let entry = append(&root, "hello").await;
    let child = harness.create_conversation(CreateOptions::default(), &context()).await.unwrap();
    let fork = root.fork(entry.id, CreateOptions::default(), &context()).await.unwrap();
    let mut sessions = Vec::new();
    for id in [root.id, child.id, fork.id] {
        sessions.push(provider_session(&harness, id).await);
    }
    // A fork must not inherit its parent's provider identity.
    assert!(sessions.iter().all(|session| is_uuid_v7(session)));
    assert_eq!(sessions.iter().collect::<std::collections::HashSet<_>>().len(), 3);
    harness.close(&context()).await.unwrap();
    assert!(root.agent(&context()).await.is_err());

    // The new process installs nothing: the stored choices survive, the tools do not resolve.
    let (harness, _) = open_harness(Arc::new(open_sqlite_storage(path).await.unwrap()), &[]).await;
    let options = CreateOptions::default().with_init(|_, _| unreachable!());
    let reopened = harness.root(&context(), options).await.unwrap();
    assert_eq!(reopened.id, ROOT_CONVERSATION_ID);
    let agent = reopened.agent(&context()).await.unwrap();
    assert_eq!((agent.model.as_ref(), agent.cwd.as_deref()), (Some(&model), Some("/repo")));
    assert!(agent.tools.is_empty());
    assert_eq!(texts(&reopened).await, ["hello"]);
    assert_eq!(harness.conversation(child.id, &context()).await.unwrap().map(|c| c.id), Some(child.id));
    let mut reopened_sessions = Vec::new();
    for id in [root.id, child.id, fork.id] {
        reopened_sessions.push(provider_session(&harness, id).await);
    }
    assert_eq!(reopened_sessions, sessions);
    let reopened_fork = harness.conversation(fork.id, &context()).await.unwrap().unwrap();
    assert_eq!(texts(&reopened_fork).await, ["hello"]);
    assert_eq!(reopened_fork.agent(&context()).await.unwrap().model, Some(model));
    assert!(harness.conversation(ConversationId(999), &context()).await.unwrap().is_none());
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn creates_independent_conversations_atomically_with_init_and_rolls_back_failures() {
    let storage = Arc::new(ControlledStorage::default());
    let (harness, registry) = open_harness(storage.clone(), &["read"]).await;
    let options = CreateOptions::default().with_init(|tx, id| {
        Box::pin(async move {
            let draft = EntryDraft { model: Some(vec![user("seed")]), ..EntryDraft::new("message") };
            tx.append_entry(id, draft).await.map(drop)
        })
    });
    let created = harness.create_conversation(options, &context()).await.unwrap();
    assert_eq!(storage.commit_count(), 1);
    assert_eq!(texts(&created).await, ["seed"]);

    let before = storage.commit_count();
    let change = AgentChange { thinking_level: Change::Set(ThinkingLevel::High), ..AgentChange::default() };
    let failing = CreateOptions::default()
        .with_agent(change)
        .with_init(|_, _| Box::pin(async { Err(DurableError::error("init failed")) }));
    let error = harness.create_conversation(failing, &context()).await.err().unwrap();
    assert!(error.to_string().contains("init failed"));
    assert_eq!(storage.commit_count(), before);

    // A conversation on the default selection follows installs live.
    registry.install(Extension::new("tool:bash").tool(noop_tool("bash"))).unwrap();
    assert_eq!(created.agent(&context()).await.unwrap().tool_names(), ["read", "bash"]);
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn binds_commits_and_task_creation_to_the_conversation() {
    let (harness, _) = open_harness(Arc::new(MemoryStorage::new()), &[]).await;
    let conversation = harness.create_conversation(CreateOptions::default(), &context()).await.unwrap();
    let task = define_task(
        TaskDefinition::new("test.work", 1, |_| Ok(Json::from(json!({ "phase": "run" }))))
            .phase("run", |_, _, _| async { Ok(()) })
            .abort(|_, _, _| async { Ok(()) }),
    );
    let template = task.clone();
    let create = move |tx: Tx| async move {
        let options = TaskOptions { conversation_id: None, ..TaskOptions::conversation(ConversationId(0)) };
        tx.create_task(template, Json::from(json!({ "n": 1 })), options).await
    };
    let task_id = conversation.commit(create, &context()).await.unwrap();
    let record = harness.commit(move |tx| async move { tx.task(task_id).await }, &context()).await.unwrap();
    assert_eq!(record.unwrap().conversation_id, conversation.id);
    let create = move |tx: Tx| async move {
        let options = TaskOptions { conversation_id: None, ..TaskOptions::conversation(ConversationId(0)) };
        tx.create_task(task, Json::from(json!({ "n": 2 })), options).await
    };
    let error = harness.commit(create, &context()).await.err().unwrap();
    assert!(error.to_string().contains("requires options.conversationId"), "{error}");
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn runs_conversation_created_in_every_creating_commit_after_the_built_ins_and_before_agent_and_init() {
    let seen = Arc::new(Mutex::new(Vec::<String>::new()));
    let registry = Arc::new(create_registry());
    let models = chat_setup(Default::default()).models;
    let mut options = HarnessOptions::new(models, registry as Arc<dyn RegistryReader>);
    let hook_seen = seen.clone();
    options.conversation_created = Some(Arc::new(move |tx: Tx, record: ConversationRecord| {
        let seen = hook_seen.clone();
        Box::pin(async move {
            let agent = tx.doc(&AGENT_DOC, record.id).await?.get()?;
            let origin = if record.parent.is_none() { "new" } else { "fork" };
            let cwd = agent.cwd.clone().unwrap_or_else(|| "undefined".into());
            seen.lock().unwrap().push(format!("{}:{origin}:{cwd}", record.id));
            tx.doc(&NOTE_DOC, record.id).await?.edit(|note| {
                if note.text.is_empty() {
                    note.text = "created".into();
                }
            })?;
            if agent.cwd.as_deref() == Some("/fail") {
                return Err(DurableError::error("no"));
            }
            Ok(())
        })
    }));
    let harness = Harness::open(Arc::new(MemoryStorage::new()), options, &context()).await.unwrap();
    let init_seen = seen.clone();
    let root_options = CreateOptions::default()
        .with_agent(AgentChange { cwd: Change::Set("/root".into()), ..AgentChange::default() })
        .with_init(move |tx, id| {
            Box::pin(async move {
                let note = tx.doc(&NOTE_DOC, id).await?.get()?;
                init_seen.lock().unwrap().push(format!("init:{}", note.text));
                Ok(())
            })
        });
    let root = harness.root(&context(), root_options).await.unwrap();
    // A raw creation in a commit, as in a tool, and a fork, which already has the asOf copies.
    let raw = harness
        .commit(|tx| async move { Ok(tx.create_conversation(ConversationOwnership::Ownerless).await?.id) }, &context())
        .await
        .unwrap();
    let entry = append(&root, "hello").await;
    let fork = root.fork(entry.id, CreateOptions::default(), &context()).await.unwrap();
    assert_eq!(
        *seen.lock().unwrap(),
        [
            format!("{}:new:undefined", root.id),
            "init:created".to_owned(),
            format!("{raw}:new:undefined"),
            format!("{}:fork:/root", fork.id),
        ]
    );
    let note = harness.snapshot(&NOTE_DOC, raw, &context()).await.unwrap().unwrap();
    assert_eq!(note.to_serde(), json!({ "text": "created" }));
    // A throw fails the creating commit.
    root.configure(AgentChange { cwd: Change::Set("/fail".into()), ..AgentChange::default() }, &context())
        .await
        .unwrap();
    assert!(root.fork(entry.id, CreateOptions::default(), &context()).await.is_ok());
    let failing = append(&root, "after").await;
    let error = root.fork(failing.id, CreateOptions::default(), &context()).await.err().unwrap();
    assert!(error.to_string().contains("no"));
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn returns_stateless_handles_and_rejects_operations_after_close() {
    let (harness, _) = open_harness(Arc::new(MemoryStorage::new()), &[]).await;
    let root = harness.root(&context(), CreateOptions::default()).await.unwrap();
    let again = harness.root(&context(), CreateOptions::default()).await.unwrap();
    assert_eq!(again.id, root.id);
    harness.close(&context()).await.unwrap();
    let closed = |error: DurableError| error.to_string().contains("is closed");
    assert!(closed(harness.root(&context(), CreateOptions::default()).await.err().unwrap()));
    assert!(closed(harness.create_conversation(CreateOptions::default(), &context()).await.err().unwrap()));
    assert!(closed(harness.conversation(root.id, &context()).await.err().unwrap()));
}

#[tokio::test]
async fn forwards_generic_session_document_apis() {
    let (harness, _) = open_harness(Arc::new(MemoryStorage::new()), &[]).await;
    let root = harness.root(&context(), CreateOptions::default()).await.unwrap();
    let id = root.id;
    let first = move |tx: Tx| async move {
        tx.doc(&NOTE_DOC, id).await?.set(&Note { text: "first".into() })?;
        tx.append_entry(id, EntryDraft { model: Some(vec![user("m")]), ..EntryDraft::new("message") }).await
    };
    let entry = root.commit(first, &context()).await.unwrap();
    let second = move |tx: Tx| async move { tx.doc(&NOTE_DOC, id).await?.set(&Note { text: "second".into() }) };
    harness.commit(second, &context()).await.unwrap();
    let note = harness.snapshot(&NOTE_DOC, id, &context()).await.unwrap().unwrap();
    assert_eq!(note.to_serde(), json!({ "text": "second" }));
    let as_of = harness.snapshot_as_of(&NOTE_DOC, id, entry.id, &context()).await.unwrap().unwrap();
    assert_eq!(as_of.to_serde(), json!({ "text": "first" }));
    let state = harness.document_state(&NOTE_DOC, id, &context()).await.unwrap().unwrap();
    assert_eq!(state.value().to_serde(), json!({ "text": "second" }));
    state.dispose().unwrap();
    harness.close(&context()).await.unwrap();
}
