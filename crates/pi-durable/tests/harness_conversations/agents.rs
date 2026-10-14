//! Forks, fork-aware history paging, and upstream `describe("Harness agent")`.

use std::sync::Arc;

use pi_ai::ThinkingLevel;
use pi_chord::Json;
use pi_durable::entries::define_entry;
use pi_durable::harness::live::LIVE_DOC;
use pi_durable::harness::usage::PROVIDER_DOC;
use pi_durable::harness::*;
use pi_durable::session::{SessionOptions, Tx, create_session};
use pi_durable::storage::MemoryStorage;
use pi_durable::tasks::TaskOptions;
use pi_durable::*;
use serde_json::{Value, json};

use super::support::{ControlledStorage, context};
use super::{append, is_uuid_v7, noop_tool, open_harness, texts, user};

fn ownerless() -> CreateOptions {
    CreateOptions { ownership: ConversationOwnership::Ownerless, ..CreateOptions::default() }
}

fn names(names: &[&str]) -> Vec<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}

async fn agent_doc(harness: &Harness, id: ConversationId) -> Option<Value> {
    harness.snapshot(&AGENT_DOC, id, &context()).await.unwrap().map(|agent| agent.to_serde())
}

fn thinking(level: ThinkingLevel) -> AgentChange {
    AgentChange { thinking_level: Change::Set(level), ..AgentChange::default() }
}

#[tokio::test]
async fn forks_at_a_concrete_entry_with_the_as_of_agent_and_applies_agent_and_init_overrides() {
    let (harness, registry) = open_harness(Arc::new(MemoryStorage::new()), &["read"]).await;
    registry.install(Extension::new("tool:legacy").tool(noop_tool("legacy"))).unwrap();
    let root = harness.root(&context(), CreateOptions::default()).await.unwrap();
    root.configure(thinking(ThinkingLevel::Low), &context()).await.unwrap();
    let at = append(&root, "one").await;
    let change =
        AgentChange { tools: Change::Set(ToolFilter::List(names(&["read"]))), ..thinking(ThinkingLevel::High) };
    root.configure(change, &context()).await.unwrap();
    append(&root, "two").await;

    let child = root.fork(at.id, ownerless(), &context()).await.unwrap();
    assert_eq!(agent_doc(&harness, child.id).await, Some(json!({ "thinkingLevel": "low" })));
    assert_eq!(texts(&child).await, ["one"]);

    let init: ConversationInit = Box::new(|tx: Tx, id| {
        Box::pin(async move {
            let level = tx.doc(&AGENT_DOC, id).await?.value()?["thinkingLevel"].clone();
            assert_eq!(level, Json::from("minimal"));
            Ok(())
        })
    });
    let tools = Change::Set(ToolFilter::List(names(&["legacy"])));
    let options = CreateOptions {
        agent: Some(AgentChange { tools, ..thinking(ThinkingLevel::Minimal) }),
        init: Some(init),
        ..ownerless()
    };
    let overridden = root.fork(at.id, options, &context()).await.unwrap();
    assert_eq!(
        agent_doc(&harness, overridden.id).await,
        Some(json!({ "thinkingLevel": "minimal", "tools": ["legacy"] }))
    );
    assert_eq!(agent_doc(&harness, root.id).await, Some(json!({ "thinkingLevel": "high", "tools": ["read"] })));

    let unrelated = harness.create_conversation(ownerless(), &context()).await.unwrap();
    assert!(root.fork(at.id, ownerless(), &context()).await.is_ok());
    let error = unrelated.fork(at.id, ownerless(), &context()).await.err().unwrap();
    assert!(error.to_string().contains("is not visible"), "{error}");
    harness.close(&context()).await.unwrap();
}

async fn ascending(conversation: &Conversation) -> Vec<String> {
    let range = EntryRange { order: Some(ScanOrder::Ascending), ..EntryRange::default() };
    let (mut all, mut cursor) = (Vec::new(), None);
    loop {
        let page = conversation.entries(range.clone(), 2, cursor, &context()).await.unwrap();
        let model = |entry: &EntryRecord| super::support::harness_support::text_of(entry.model.as_ref()?.first());
        all.extend(page.items.iter().filter_map(model));
        cursor = page.next;
        if cursor.is_none() {
            return all;
        }
    }
}

#[tokio::test]
async fn paginates_fork_aware_history_through_deep_ancestor_caps_and_same_commit_prefixes() {
    let (harness, _) = open_harness(Arc::new(MemoryStorage::new()), &[]).await;
    let root = harness.root(&context(), CreateOptions::default()).await.unwrap();
    append(&root, "r1").await;
    let id = root.id;
    let both = move |tx: Tx| async move {
        let draft = |text: &str| EntryDraft { model: Some(vec![user(text)]), ..EntryDraft::new("message") };
        let r2 = tx.append_entry(id, draft("r2")).await?;
        tx.append_entry(id, draft("r3")).await?;
        Ok(r2)
    };
    let r2 = root.commit(both, &context()).await.unwrap();
    let child = root.fork(r2.id, ownerless(), &context()).await.unwrap();
    let c1 = append(&child, "c1").await;
    append(&child, "c2").await;
    let grandchild = child.fork(c1.id, ownerless(), &context()).await.unwrap();
    append(&grandchild, "g1").await;

    assert_eq!(texts(&root).await, ["r3", "r2", "r1"]);
    assert_eq!(texts(&child).await, ["c2", "c1", "r2", "r1"]);
    assert_eq!(texts(&grandchild).await, ["g1", "c1", "r2", "r1"]);
    // Upstream #10546.
    assert_eq!(ascending(&grandchild).await, ["r1", "r2", "c1", "g1"]);
    // Upstream also passes a `conversationId`, which the range ignores; a Rust `EntryRange` has no such field.
    let range = EntryRange { min_entry_id: Some(r2.id), max_entry_id: Some(c1.id), ..EntryRange::default() };
    let bounded = grandchild.entries(range, 10, None, &context()).await.unwrap();
    assert_eq!(bounded.items.iter().map(|entry| entry.id).collect::<Vec<_>>(), [c1.id, r2.id]);
    let message = define_entry::<Value>("message").unwrap();
    assert!(message.is(bounded.items.first()));
    assert!(!message.is(None));
    harness.close(&context()).await.unwrap();
}

async fn offered(conversation: &Conversation) -> Vec<String> {
    let agent = conversation.agent(&context()).await.unwrap();
    agent.tools.iter().map(|tool| tool.name().to_owned()).collect()
}

fn tools(change: ToolFilter) -> AgentChange {
    AgentChange { tools: Change::Set(change), ..AgentChange::default() }
}

#[tokio::test]
async fn replaces_whole_fields_clears_them_with_null_and_leaves_undefined_fields_alone() {
    let (harness, registry) = open_harness(Arc::new(MemoryStorage::new()), &[]).await;
    let coding = Extension::new("coding").tool(noop_tool("read")).tool(noop_tool("bash")).tool(noop_tool("edit"));
    registry.install(coding).unwrap();
    let root = harness.root(&context(), CreateOptions::default()).await.unwrap();
    let agent = root.agent(&context()).await.unwrap();
    assert_eq!((agent.thinking_level, agent.model.clone()), (ThinkingLevel::Off, None));
    assert_eq!(offered(&root).await, ["read", "bash", "edit"]);

    let model = ModelRef { provider: "openai".into(), model_id: "gpt".into() };
    root.configure(AgentChange { model: Change::Set(model), ..thinking(ThinkingLevel::Medium) }, &context())
        .await
        .unwrap();
    let instructions = AgentChange { instructions: Change::Set("Be terse.".into()), ..AgentChange::default() };
    root.configure(instructions, &context()).await.unwrap();
    let expected = json!({
        "model": { "provider": "openai", "modelId": "gpt" }, "thinkingLevel": "medium", "instructions": "Be terse."
    });
    assert_eq!(agent_doc(&harness, root.id).await, Some(expected));
    let clear = AgentChange { model: Change::Clear, instructions: Change::Clear, ..AgentChange::default() };
    root.configure(clear, &context()).await.unwrap();
    assert_eq!(agent_doc(&harness, root.id).await, Some(json!({ "thinkingLevel": "medium" })));

    root.configure(tools(ToolFilter::Remove { remove: names(&["edit"]) }), &context()).await.unwrap();
    assert_eq!(offered(&root).await, ["read", "bash"]);
    // A new filter replaces the old one: edit is offered again.
    root.configure(tools(ToolFilter::Remove { remove: names(&["bash"]) }), &context()).await.unwrap();
    assert_eq!(offered(&root).await, ["read", "edit"]);
    root.configure(tools(ToolFilter::List(names(&["edit", "read"]))), &context()).await.unwrap();
    assert_eq!(offered(&root).await, ["edit", "read"]);
    root.configure(AgentChange { tools: Change::Clear, ..AgentChange::default() }, &context()).await.unwrap();
    assert_eq!(offered(&root).await, ["read", "bash", "edit"]);
    // Names are stored without checking the registry.
    root.configure(tools(ToolFilter::List(names(&["missing", "read"]))), &context()).await.unwrap();
    assert_eq!(agent_doc(&harness, root.id).await.unwrap()["tools"], json!(["missing", "read"]));
    assert_eq!(offered(&root).await, ["read"]);
    harness.close(&context()).await.unwrap();
}

async fn session_of(harness: &Harness, id: ConversationId) -> Option<String> {
    let provider = harness.snapshot(&PROVIDER_DOC, id, &context()).await.unwrap()?;
    provider["sessionId"].as_str().map(str::to_owned)
}

#[tokio::test]
async fn gives_conversations_created_through_tx_their_documents_empty_an_owner_copy_or_the_fork_s_as_of_copy() {
    let (harness, _) = open_harness(Arc::new(MemoryStorage::new()), &["read"]).await;
    let agent = AgentChange {
        model: Change::Set(ModelRef { provider: "faux".into(), model_id: "m".into() }),
        instructions: Change::Set("Main role.".into()),
        cwd: Change::Set("/repo".into()),
        ..AgentChange::default()
    };
    let root = harness.root(&context(), CreateOptions::default().with_agent(agent)).await.unwrap();
    let owner = define_task(
        TaskDefinition::new("test.owner", 1, |_| Ok(Json::from(json!({ "phase": "never" }))))
            .phase("never", |_, _, _| async { Ok(()) })
            .abort(|_, _, _| async { Ok(()) }),
    );
    let root_id = root.id;
    let stage = move |tx: Tx| async move {
        let task_id = tx.create_task(owner, Json::from(json!({})), TaskOptions::conversation(root_id)).await?;
        let plain = tx.create_conversation(ConversationOwnership::Ownerless).await?;
        let owned = tx.create_conversation(ConversationOwnership::Task { task_id }).await?;
        // The copy exists when create_conversation() returns, so a configure() in the same callback overrides it.
        assert_eq!(tx.doc(&AGENT_DOC, owned.id).await?.value()?["cwd"], Json::from("/repo"));
        configure(&tx, owned.id, &AgentChange { cwd: Change::Set("/worktree".into()), ..AgentChange::default() })
            .await?;
        Ok((task_id, plain.id, owned.id))
    };
    let (task_id, plain, owned) = root.commit(stage, &context()).await.unwrap();
    assert_eq!(agent_doc(&harness, plain).await, Some(json!({})));
    let live = harness.snapshot(&LIVE_DOC, plain, &context()).await.unwrap().map(|live| live.to_serde());
    assert_eq!(live, Some(json!({})));
    assert!(is_uuid_v7(&session_of(&harness, plain).await.unwrap()));
    assert_ne!(session_of(&harness, owned).await, session_of(&harness, root.id).await);
    let expected =
        json!({ "model": { "provider": "faux", "modelId": "m" }, "instructions": "Main role.", "cwd": "/worktree" });
    assert_eq!(agent_doc(&harness, owned).await, Some(expected));

    // A later owner change does not reach the child.
    root.configure(thinking(ThinkingLevel::High), &context()).await.unwrap();
    assert!(agent_doc(&harness, owned).await.unwrap().get("thinkingLevel").is_none());

    // A task-owned fork keeps the as-of copy of its fork parent's agent.
    let note = move |tx: Tx| async move { Ok(tx.append_entry(plain, EntryDraft::new("note")).await?.id) };
    let at = root.commit(note, &context()).await.unwrap();
    let fork = move |tx: Tx| async move {
        Ok(tx.fork_conversation(plain, at, ConversationOwnership::Task { task_id }).await?.id)
    };
    let fork = root.commit(fork, &context()).await.unwrap();
    assert_eq!(agent_doc(&harness, fork).await, Some(json!({})));
    let live = harness.snapshot(&LIVE_DOC, fork, &context()).await.unwrap().map(|live| live.to_serde());
    assert_eq!(live, Some(json!({})));
    assert_ne!(session_of(&harness, fork).await, session_of(&harness, plain).await);
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn reads_an_absent_agent_for_conversations_a_plain_session_created_without_writing() {
    let storage = Arc::new(ControlledStorage::default());
    let session = create_session(storage.clone(), SessionOptions::default());
    let create = |tx: Tx| async move { Ok(tx.create_conversation(ConversationOwnership::Ownerless).await?.id) };
    let id = session.commit(create, &context()).await.unwrap();
    let (harness, _) = open_harness(storage.clone(), &["read"]).await;
    let raw = harness.conversation(id, &context()).await.unwrap().unwrap();
    assert!(harness.snapshot(&LIVE_DOC, id, &context()).await.unwrap().is_none());
    let commits = storage.commit_count();
    let agent = raw.agent(&context()).await.unwrap();
    assert_eq!(agent.thinking_level, ThinkingLevel::Off);
    assert_eq!(agent.tools.iter().map(|tool| tool.name().to_owned()).collect::<Vec<_>>(), ["read"]);
    assert_eq!(storage.commit_count(), commits);
    raw.configure(thinking(ThinkingLevel::Low), &context()).await.unwrap();
    assert_eq!(agent_doc(&harness, id).await, Some(json!({ "thinkingLevel": "low" })));
    harness.close(&context()).await.unwrap();
}
