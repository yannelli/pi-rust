//! Bases and overflow, what each kind of listener sees, pinning, and a crash. The GC case is not ported: a held revision
//! is dropped with its hold, which `pinning` checks through the cache.

use std::sync::{Arc, Mutex};

use pi_chord::delta::{Op, apply_immutable};
use pi_durable::session::{SessionOptions, Tx, create_session};
use pi_durable::*;
use serde_json::json;

use super::support::*;
use super::*;
use crate::support::{ValueCall, on_value};

#[tokio::test]
async fn stores_a_base_when_a_held_commit_selected_one_and_counts_the_store_as_one_delta_otherwise() {
    let opened = open(None, None).await;
    let (mark, id) = (opened.mark(), opened.conversation_id);
    let change = move |tx: Tx| async move {
        tx.doc(&TEXT_DOC, id).await?.core().update(|state| {
            state.set("text", "x")?;
            state.set("base", true)
        })??;
        Ok(())
    };
    opened.session.commit_with_options(change, &context(), CommitOptions::DEFERRED).await.unwrap();
    opened.append("y", CommitOptions::DEFERRED).await.unwrap();
    opened.fire().await;
    let contents: Vec<_> = changes_of(&opened.batches_since(mark)).iter().map(|(_, c)| content_json(c)).collect();
    assert_eq!(contents, [json!({ "version": 1, "kind": "base", "value": { "text": "xy", "base": true } })]);
}

/// Hold more than `MAX_PENDING_OPS` operations: deferred commits of one set per key, each below the tracker's own
/// limit, past which a commit is one root replacement.
async fn overflow(opened: &Opened) {
    let id = opened.conversation_id;
    let keys = move |tx: Tx| async move {
        tx.doc(&TEXT_DOC, id).await?.core().update(|state| state.set("keys", json(r"{}")))??;
        Ok(())
    };
    opened.session.commit(keys, &context()).await.unwrap();
    for from in (0..=pi_chord::delta::MAX_PENDING_OPS).step_by(4_000) {
        let change = move |tx: Tx| async move {
            tx.doc(&TEXT_DOC, id).await?.core().update(|state| {
                let keys = state.get_mut("keys").unwrap();
                for index in from..from + 4_000 {
                    keys.set(format!("k{index}"), index).unwrap();
                }
            })?;
            Ok(())
        };
        opened.session.commit_with_options(change, &context(), CommitOptions::DEFERRED).await.unwrap();
    }
}

#[tokio::test]
async fn stores_the_whole_value_once_more_than_max_pending_ops_operations_are_held() {
    let opened = open(None, None).await;
    overflow(&opened).await;
    let mark = opened.mark();
    opened.fire().await;
    let value = opened.snapshot().await.unwrap();
    let changes = changes_of(&opened.batches_since(mark));
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].1, DocumentContent::Base { version: 1, value: value.clone() });
    // A durable listener replaces its copy with the stored value.
    let published = opened.stored.lock().unwrap().last().cloned().unwrap();
    let DocumentCommitChange::Change { ops, .. } = &document_changes(&published)[0] else { unreachable!() };
    assert_eq!(*ops, vec![Op::Replace(value)]);
}

#[tokio::test]
async fn stores_a_base_of_the_commit_s_own_value_when_it_stores_an_overflowed_hold() {
    let opened = open(None, None).await;
    overflow(&opened).await;
    opened.append("a", CommitOptions::DEFERRED).await.unwrap();
    let mark = opened.mark();
    opened.append("b", IMMEDIATE).await.unwrap();
    let value = opened.snapshot().await.unwrap();
    assert_eq!(value["text"], json("\"ab\""));
    let changes = changes_of(&opened.batches_since(mark));
    assert_eq!(
        changes.iter().map(|(_, content)| content.clone()).collect::<Vec<_>>(),
        [DocumentContent::Base { version: 1, value }]
    );
}

#[tokio::test]
async fn give_observe_commits_exactly_what_was_stored_and_nothing_for_a_held_commit() {
    let opened = open(None, None).await;
    let (from, publications, id) = (opened.mark(), opened.stored.lock().unwrap().len(), opened.conversation_id);
    opened.append("a", CommitOptions::DEFERRED).await.unwrap();
    opened.append("b", CommitOptions::DEFERRED).await.unwrap();
    opened.append("c", IMMEDIATE).await.unwrap();
    opened.append("d", CommitOptions::DEFERRED).await.unwrap();
    opened.fire().await;
    opened.append("e", CommitOptions::DEFERRED).await.unwrap();
    let note = move |tx: Tx| async move {
        tx.doc(&TEXT_DOC, id).await?;
        tx.append_entry(id, EntryDraft::new("test.note")).await.map(drop)
    };
    opened.session.commit(note, &context()).await.unwrap();
    opened.append("f", CommitOptions::DEFERRED).await.unwrap();
    opened.session.close(&context()).await.unwrap();
    let batches = opened.batches_since(from);
    let published = opened.stored.lock().unwrap()[publications..].to_vec();
    assert_eq!(published.len(), batches.len());
    // A replica built from the publications alone always equals what Storage holds.
    let (mut from_storage, mut from_publications) = (json(r#"{"text":""}"#), json(r#"{"text":""}"#));
    for (batch, publication) in batches.iter().zip(&published) {
        let content = changes_of(std::slice::from_ref(batch)).into_iter().find(|(at, _)| *at == opened.record_id);
        from_storage = match content.unwrap().1 {
            DocumentContent::Base { value, .. } => value,
            DocumentContent::Delta { ops, .. } => apply_immutable(Some(&from_storage), &ops).unwrap(),
        };
        let change =
            document_changes(publication).into_iter().find(|change| change_record(change).id == opened.record_id);
        let DocumentCommitChange::Change { ops, value, .. } = change.unwrap() else { unreachable!() };
        from_publications = apply_immutable(Some(&from_publications), &ops).unwrap();
        assert_eq!(from_publications, from_storage);
        assert_eq!(value, from_storage.clone());
    }
    assert_eq!(from_storage, json(r#"{"text":"abcdef"}"#));
    let kinds: Vec<Vec<&str>> = published
        .iter()
        .map(|publication| {
            let mut kinds: Vec<&str> = publication
                .changes
                .iter()
                .map(|change| if matches!(change, CommitChange::Entry(_)) { "entry" } else { "document" })
                .collect();
            kinds.sort();
            kinds
        })
        .collect();
    assert_eq!(kinds, [vec!["document"], vec!["document"], vec!["document", "entry"], vec!["document"]]);
}

#[tokio::test]
async fn show_every_held_commit_to_watches_and_the_session_s_own_observers_once() {
    let opened = open(None, None).await;
    let watch = opened.session.watch_doc(&TEXT_DOC, opened.conversation_id, &context()).await.unwrap().unwrap();
    let values: Arc<Mutex<Vec<String>>> = Arc::default();
    let sink = values.clone();
    watch
        .start(on_value(move |ValueCall { value, .. }, _| {
            sink.lock().unwrap().push(value["text"].as_str().unwrap_or_default().to_owned());
            async { Ok(()) }
        }))
        .unwrap();
    let from = opened.observed.lock().unwrap().len();
    for text in ["a", "b", "c"] {
        opened.append(text, CommitOptions::DEFERRED).await.unwrap();
        flush().await;
    }
    opened.append("d", IMMEDIATE).await.unwrap();
    flush().await;
    opened.fire().await;
    assert_eq!(*values.lock().unwrap(), ["a", "ab", "abc", "abcd"]);
    let seen = opened.observed.lock().unwrap()[from..].to_vec();
    assert_eq!(seen.iter().map(|publication| publication.is_memory()).collect::<Vec<_>>(), [true, true, true, false]);
    // The storing commit shows its own operations; the held ones were shown already. The timer showed nothing.
    let ops: Vec<Vec<Vec<Op>>> = seen
        .iter()
        .map(|publication| {
            let changes = publication.changes().iter().filter_map(|change| match change {
                CommitChange::Document(DocumentCommitChange::Change { ops, .. }) => Some(ops.clone()),
                _ => None,
            });
            changes.collect()
        })
        .collect();
    let append = |text: &str| vec![vec![Op::Append(vec!["text".into()], text.into())]];
    assert_eq!(ops, [append("a"), append("b"), append("c"), append("d")]);
    watch.stop().await;
}

#[tokio::test]
async fn keeps_a_held_document_cached_with_retention_0_until_it_is_stored_then_lets_it_go() {
    let opened = open(Some(0.0), None).await;
    let sizes = || {
        let sizes = opened.session.document_cache_sizes();
        (sizes.cached, sizes.held, sizes.attached)
    };
    opened.session.read_on_line(async { Ok(()) }).await.unwrap();
    assert_eq!(sizes(), (0, 0, 0));
    opened.append("held", CommitOptions::DEFERRED).await.unwrap();
    opened.session.read_on_line(async { Ok(()) }).await.unwrap();
    assert_eq!(sizes(), (1, 1, 1));
    opened.fire().await;
    opened.session.commit(|_| async { Ok(()) }, &context()).await.unwrap();
    assert_eq!(sizes(), (0, 0, 0));
    assert_eq!(opened.snapshot().await, Some(json(r#"{"text":"held"}"#)));
}

#[tokio::test]
async fn leaves_storage_at_the_last_stored_state_which_memory_had_and_nothing_held_after_reopen() {
    let opened = open(None, None).await;
    let states: Arc<Mutex<Vec<String>>> = Arc::default();
    let (sink, record_id) = (states.clone(), opened.record_id);
    std::mem::forget(opened.session.follow_commits(move |publication, _| {
        for change in publication.changes() {
            if let CommitChange::Document(DocumentCommitChange::Change { record, value, .. }) = change
                && record.id == record_id
            {
                sink.lock().unwrap().push(value["text"].as_str().unwrap_or_default().to_owned());
            }
        }
        Ok(())
    }));
    opened.append("a", CommitOptions::DEFERRED).await.unwrap();
    opened.fire().await;
    opened.append("b", CommitOptions::DEFERRED).await.unwrap();
    opened.append("c", CommitOptions::DEFERRED).await.unwrap();
    opened.session.fail(DurableError::error("crash"), None);
    opened.session.closed().await;
    opened.storage.reopen();
    let reopened = create_session(opened.storage.clone(), SessionOptions::default());
    let value = reopened.snapshot(&TEXT_DOC, opened.conversation_id, &context()).await.unwrap().unwrap();
    assert_eq!(value, json(r#"{"text":"a"}"#));
    assert!(states.lock().unwrap().contains(&"a".to_owned()));
    assert_eq!(reopened.document_cache_sizes().held, 0);
    reopened.close(&context()).await.unwrap();
}
