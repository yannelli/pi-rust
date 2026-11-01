//! Examples 00 to 05: a bare Session with conversations, documents, forks, owned conversations, and observation.

use std::collections::BTreeMap;
use std::sync::Mutex;

use pi_chord::{DeliveryKind, StateListener};
use pi_durable::tasks::{TaskOptions, TaskOwnership};

use super::*;
use crate::support::{ValueCall, on_value};

#[tokio::test]
async fn ex00_conversation_creates_an_ownerless_conversation() {
    let session = new_session();
    let standalone = ownerless(&session).await;
    assert_eq!((standalone.parent, standalone.owner), (None, None));
    session.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn ex01_documents_reads_the_notes_saved_with_each_entry() {
    let session = new_session();
    let chat = ownerless(&session).await.id;
    let first = note_with(&session, chat, "hello", "after hello").await;
    let second = note_with(&session, chat, "goodbye", "after goodbye").await;
    let latest = session.snapshot(&NOTES, chat, &ctx()).await.unwrap();
    assert_eq!(notes(latest).as_deref(), Some("after goodbye"));
    let at_first = session.snapshot_as_of(&NOTES, chat, first.id, &ctx()).await.unwrap();
    assert_eq!(notes(at_first).as_deref(), Some("after hello"));
    let at_second = session.snapshot_as_of(&NOTES, chat, second.id, &ctx()).await.unwrap();
    assert_eq!(notes(at_second).as_deref(), Some("after goodbye"));
    session.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn ex02_forks_copies_the_transcript_and_notes_up_to_the_fork_entry() {
    let session = new_session();
    let chat = ownerless(&session).await.id;
    let first = note_with(&session, chat, "hello", "after hello").await;
    note_with(&session, chat, "goodbye", "after goodbye").await;
    let fork =
        move |tx: Tx| async move { tx.fork_conversation(chat, first.id, ConversationOwnership::Ownerless).await };
    let branch = session.commit(fork, &ctx()).await.unwrap().id;
    let scan = move |tx: Tx| async move { tx.scan_entries(EntryQuery::new(branch), 10, None).await };
    let mut entries = session.commit(scan, &ctx()).await.unwrap().items;
    entries.sort_by_key(|entry| entry.id);
    let data: Vec<Value> =
        entries.iter().map(|entry| entry.data.as_ref().map_or(Value::Null, Json::to_serde)).collect();
    assert_eq!(data, [json!("hello")]);
    assert_eq!(notes(session.snapshot(&NOTES, branch, &ctx()).await.unwrap()).as_deref(), Some("after hello"));

    // Writes to the fork leave the parent alone.
    let edit =
        move |tx: Tx| async move { tx.doc(&NOTES, branch).await?.edit(|n| n.text = "changed only in the fork".into()) };
    session.commit(edit, &ctx()).await.unwrap();
    let fork_notes = notes(session.snapshot(&NOTES, branch, &ctx()).await.unwrap());
    assert_eq!(fork_notes.as_deref(), Some("changed only in the fork"));
    assert_eq!(notes(session.snapshot(&NOTES, chat, &ctx()).await.unwrap()).as_deref(), Some("after goodbye"));
    session.close(&ctx()).await.unwrap();
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RegisteredAgent {
    conversation_id: ConversationId,
    request_id: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
struct AgentRegistry {
    agents: BTreeMap<String, RegisteredAgent>,
}

#[tokio::test]
async fn ex03_owned_conversations_creates_a_task_owned_child_and_records_it() {
    let supervisor = define_task(
        TaskDefinition::new("example.supervisor", 1, |_| Ok(Json::from(json!({ "phase": "ready" }))))
            .phase("ready", |_, _, _| async { Ok(()) })
            .abort(|_, _, _| async { Ok(()) }),
    );
    let spec = DocSpec::new(
        "example.agent-registry",
        1,
        LatestConversation(LatestFork::Initial),
        AgentRegistry::default,
        |_, _, _| Ok(false),
    );
    let registry = define_doc(spec).unwrap();
    let session = new_session();
    let main = ownerless(&session).await.id;
    let doc = registry.clone();
    let setup = move |tx: Tx| async move {
        let options = TaskOptions { background: true, ..TaskOptions::conversation(main) };
        let supervisor_id = tx.create_task(supervisor, Json::Null, options).await?;
        // The child conversation lives exactly as long as its owner task.
        let child = tx.create_conversation(ConversationOwnership::Task { task_id: supervisor_id }).await?;
        let request_id = format!("researcher:first-message:{supervisor_id}");
        let registered = RegisteredAgent { conversation_id: child.id, request_id };
        tx.doc(&doc, main).await?.edit(|registry| registry.agents.insert("researcher".into(), registered))?;
        Ok((supervisor_id, child))
    };
    let (supervisor_id, child) = session.commit(setup, &ctx()).await.unwrap();
    assert_eq!(child.owner.map(|owner| owner.task_id), Some(supervisor_id));
    let stored = session.snapshot(&registry, main, &ctx()).await.unwrap().unwrap().to_serde();
    let expected =
        json!({ "conversationId": child.id, "requestId": format!("researcher:first-message:{supervisor_id}") });
    assert_eq!(stored["agents"]["researcher"], expected);
    // Session-level tasks carry no scheduler; the Harness runs them.
    let _ = TaskOwnership::Conversation;
    session.close(&ctx()).await.unwrap();
}

async fn notes_conversation(session: &Session) -> ConversationId {
    let create = |tx: Tx| async move {
        let conversation = tx.create_conversation(ConversationOwnership::Ownerless).await?;
        tx.doc(&NOTES, conversation.id).await?.edit(|notes| notes.text = "first".into())?;
        Ok(conversation.id)
    };
    session.commit(create, &ctx()).await.unwrap()
}

async fn set_notes(session: &Session, id: ConversationId, text: &str) {
    let text = text.to_owned();
    let edit = move |tx: Tx| async move { tx.doc(&NOTES, id).await?.edit(|notes| notes.text = text) };
    session.commit(edit, &ctx()).await.unwrap();
}

#[tokio::test]
async fn ex04_chord_state_publishes_the_hydrated_value_then_each_commit() {
    let session = new_session();
    let chat = notes_conversation(&session).await;
    let state = session.document_state(&NOTES, chat, &ctx()).await.unwrap().expect("notes are absent");
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = seen.clone();
    let stop = state.subscribe(StateListener::sync(move |value, _, delivery| {
        let kind = if delivery.kind == DeliveryKind::Hydrate { "hydrate" } else { "update" };
        sink.lock().unwrap().push((kind, delivery.sequence, notes(Some(value.clone())).unwrap_or_default()));
        Ok(())
    }));
    set_notes(&session, chat, "published through Chord").await;
    stop.close().unwrap();
    state.dispose().unwrap();
    let expected = [("hydrate", 0, "first".to_owned()), ("update", 1, "published through Chord".to_owned())];
    assert_eq!(*seen.lock().unwrap(), expected);
    session.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn ex05_watches_delivers_committed_values_after_the_baseline() {
    let session = new_session();
    let chat = notes_conversation(&session).await;
    let watch = session.watch_doc(&NOTES, chat, &ctx()).await.unwrap().expect("notes are absent");
    assert_eq!(notes(Some(watch.value())).as_deref(), Some("first"));
    let (sender, delivered) = tokio::sync::oneshot::channel();
    let sender = Mutex::new(Some(sender));
    watch
        .start(on_value(move |ValueCall { value, .. }, _| {
            if let Some(sender) = sender.lock().unwrap().take() {
                let _ = sender.send(notes(Some(value)));
            }
            async { Ok(()) }
        }))
        .unwrap();
    set_notes(&session, chat, "observed asynchronously").await;
    assert_eq!(delivered.await.unwrap().as_deref(), Some("observed asynchronously"));
    watch.stop().await;
    session.close(&ctx()).await.unwrap();
}
