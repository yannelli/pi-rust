//! Fork copies by policy, ancestry, stored versions, incarnations, scopes, and pagination.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use pi_durable::tasks::{TaskOptions, TaskTemplate};

use super::support::*;
use super::*;

#[tokio::test]
async fn copies_as_of_and_current_singleton_and_family_bases_while_leaving_initial_documents_absent() {
    let as_of = rewindable("fork.policies.as-of", r#"{"value":"as-initial"}"#);
    let current = latest("fork.policies.current", LatestFork::Current, r#"{"value":"current-initial"}"#);
    let initial = latest("fork.policies.initial", LatestFork::Initial, r#"{"value":"fresh"}"#);
    let seeded = |seed: String| json(&format!(r#"{{"value":"{seed}"}}"#));
    let as_of_family = define_doc_family(DocFamilySpec::new(
        "fork.policies.as-of-family",
        1,
        RewindableConversation(DocumentFork::AsOf),
        seeded,
    ))
    .unwrap();
    let current_family = define_doc_family(DocFamilySpec::new(
        "fork.policies.current-family",
        1,
        LatestConversation(LatestFork::Current),
        seeded,
    ))
    .unwrap();
    let test = open_test_session();
    let parent = create_conversation(&test.session).await;
    let unused = "unused".to_owned();
    let tokens = (as_of.clone(), current.clone(), initial.clone(), as_of_family.clone(), current_family.clone());
    let seed = unused.clone();
    let fork_at = entry_with(&test.session, parent, "fork-point", move |tx| async move {
        let (a, c, i, af, cf) = tokens;
        set_value(&tx, &a, parent, "as-at-fork").await?;
        set_value(&tx, &c, parent, "current-at-fork").await?;
        set_value(&tx, &i, parent, "parent-only").await?;
        tx.family_doc(&af, parent, "a", &seed).await?.update(|d| d.set("value", "family-as-a"))??;
        tx.family_doc(&af, parent, "b", &seed).await?.update(|d| d.set("value", "family-as-b"))??;
        tx.family_doc(&cf, parent, "a", &seed)
            .await?
            .update(|d| d.set("value", "family-current-a"))?
            .map_err(Into::into)
    })
    .await;
    let tokens = (as_of.clone(), current.clone(), as_of_family.clone(), current_family.clone());
    test.session
        .commit(
            move |tx| async move {
                let (a, c, af, cf) = tokens;
                set_value(&tx, &a, parent, "as-after-fork").await?;
                set_value(&tx, &c, parent, "current-when-copied").await?;
                tx.family_doc(&af, parent, "a", &unused).await?.update(|d| d.set("value", "family-as-after"))??;
                let draft = tx.family_doc(&cf, parent, "a", &unused).await?;
                draft.update(|d| d.set("value", "family-current-when-copied"))?.map_err(Into::into)
            },
            &context(),
        )
        .await
        .unwrap();
    flush().await;
    let reads = test.storage.document_reads();
    let child = fork(&test.session, parent, fork_at).await.unwrap();
    flush().await;
    assert_eq!(test.storage.document_reads(), reads);
    let ctx = context();
    assert_eq!(snapshot(&test.session, &as_of, child.id).await.unwrap(), json(r#"{"value":"as-at-fork"}"#));
    assert_eq!(snapshot(&test.session, &current, child.id).await.unwrap(), json(r#"{"value":"current-when-copied"}"#));
    assert!(snapshot(&test.session, &initial, child.id).await.is_none());
    let family = |token, key| test.session.family_snapshot(token, child.id, key, &ctx);
    assert_eq!(family(&as_of_family, "a").await.unwrap().unwrap(), json(r#"{"value":"family-as-a"}"#));
    assert_eq!(family(&as_of_family, "b").await.unwrap().unwrap(), json(r#"{"value":"family-as-b"}"#));
    let copied = test.session.family_snapshot(&current_family, child.id, "a", &ctx).await.unwrap();
    assert_eq!(copied.unwrap(), json(r#"{"value":"family-current-when-copied"}"#));

    let admitted = copies(&test.storage.last_commit());
    assert_eq!(admitted.len(), 5);
    assert!(
        admitted.iter().all(|(record, _)| record.scope == DocumentScope::Conversation { conversation_id: child.id })
    );
    let publication = test.last_publication();
    let copied = document_copy_changes(&publication);
    assert_eq!(copied.len(), 5);
    for change in &copied {
        let DocumentCommitChange::Copy { record, conversation_id, source } = change else { panic!() };
        assert_eq!(record.created_at, publication.seq);
        assert_eq!(*conversation_id, child.id);
        let write = admitted.iter().find(|(candidate, _)| candidate.id == record.id).unwrap();
        assert_eq!(*source, write.1);
    }
    let parent_record = find_record(&test, "fork.policies.as-of", parent).await.unwrap();
    let child_record = find_record(&test, "fork.policies.as-of", child.id).await.unwrap();
    assert_ne!(child_record.id, parent_record.id);

    let token = as_of.clone();
    let child_id = child.id;
    test.session
        .commit(move |tx| async move { set_value(&tx, &token, child_id, "child-independent").await }, &ctx)
        .await
        .unwrap();
    assert_eq!(snapshot(&test.session, &as_of, parent).await.unwrap(), json(r#"{"value":"as-after-fork"}"#));
    assert_eq!(snapshot(&test.session, &as_of, child_id).await.unwrap(), json(r#"{"value":"child-independent"}"#));
    let token = initial.clone();
    test.session
        .commit(move |tx| async move { set_value(&tx, &token, child_id, "child-created").await }, &ctx)
        .await
        .unwrap();
    assert_eq!(snapshot(&test.session, &initial, child_id).await.unwrap(), json(r#"{"value":"child-created"}"#));
}

#[tokio::test]
async fn uses_final_document_state_from_the_fork_entry_commit_while_excluding_later_same_commit_entries() {
    let doc = rewindable("fork.same-commit", r#"{"value":"initial"}"#);
    let test = open_test_session();
    let parent = create_conversation(&test.session).await;
    let token = doc.clone();
    let (fork_at, excluded) = test
        .session
        .commit(
            move |tx| async move {
                let included = tx.append_entry(parent, EntryDraft::new("included")).await?;
                set_value(&tx, &token, parent, "final-state-of-commit").await?;
                let excluded = tx.append_entry(parent, EntryDraft::new("excluded")).await?;
                Ok((included.id, excluded.id))
            },
            &context(),
        )
        .await
        .unwrap();
    let child = fork(&test.session, parent, fork_at).await.unwrap();
    assert_eq!(snapshot(&test.session, &doc, child.id).await.unwrap(), json(r#"{"value":"final-state-of-commit"}"#));
    let child_id = child.id;
    let visible = test
        .session
        .commit(move |tx| async move { tx.scan_entries(EntryQuery::new(child_id), 10, None).await }, &context())
        .await
        .unwrap();
    let ids: Vec<_> = visible.items.iter().map(|entry| entry.id).collect();
    assert!(ids.contains(&fork_at));
    assert!(!ids.contains(&excluded));
}

#[tokio::test]
async fn selects_the_entry_owning_ancestor_for_as_of_copies_and_the_immediate_parent_for_current_copies() {
    let as_of = rewindable("fork.ancestry.as-of", r#"{"value":"initial"}"#);
    let current = latest("fork.ancestry.current", LatestFork::Current, r#"{"value":"initial"}"#);
    let test = open_test_session();
    let root = create_conversation(&test.session).await;
    let tokens = (as_of.clone(), current.clone());
    let inherited = entry_with(&test.session, root, "root", move |tx| async move {
        set_value(&tx, &tokens.0, root, "root-at-entry").await?;
        set_value(&tx, &tokens.1, root, "root-current").await
    })
    .await;
    let parent = fork(&test.session, root, inherited).await.unwrap().id;
    let tokens = (as_of.clone(), current.clone());
    let parent_entry = entry_with(&test.session, parent, "parent", move |tx| async move {
        set_value(&tx, &tokens.0, parent, "parent-at-own-entry").await?;
        set_value(&tx, &tokens.1, parent, "parent-current").await
    })
    .await;
    let inherited_fork = fork(&test.session, parent, inherited).await.unwrap().id;
    assert_eq!(snapshot(&test.session, &as_of, inherited_fork).await.unwrap(), json(r#"{"value":"root-at-entry"}"#));
    assert_eq!(snapshot(&test.session, &current, inherited_fork).await.unwrap(), json(r#"{"value":"parent-current"}"#));
    let own_fork = fork(&test.session, parent, parent_entry).await.unwrap().id;
    let expected = json(r#"{"value":"parent-at-own-entry"}"#);
    assert_eq!(snapshot(&test.session, &as_of, own_fork).await.unwrap(), expected);
    assert_eq!(snapshot(&test.session, &current, own_fork).await.unwrap(), json(r#"{"value":"parent-current"}"#));
}

fn versioned(version: u32, migrations: &Arc<AtomicUsize>) -> DocToken<Json, RewindableConversation> {
    let counter = migrations.clone();
    let initial = json(r#"{"count":0,"migrated":false}"#);
    let spec = DocSpec::new("fork.stored-version", version, RewindableConversation(DocumentFork::AsOf), move || {
        initial.clone()
    });
    define_doc(spec.migrate(move |value, from| {
        assert_eq!(from, 1);
        counter.fetch_add(1, Ordering::SeqCst);
        Ok(json(&format!(r#"{{"count":{},"migrated":true}}"#, value["count"])))
    }))
    .unwrap()
}

#[tokio::test]
async fn copies_the_stored_value_and_version_without_consulting_migration_definitions_or_migrated_caches() {
    let v1 = rewindable("fork.stored-version", r#"{"count":1}"#);
    let migrations = Arc::new(AtomicUsize::new(0));
    let v3 = versioned(3, &migrations);
    let test = open_test_session();
    let parent = create_conversation(&test.session).await;
    let token = v1.clone();
    let fork_at =
        entry_with(&test.session, parent, "point", move |tx| async move { tx.doc(&token, parent).await.map(drop) })
            .await;
    test.session.unload_documents().await.unwrap();
    assert_eq!(snapshot(&test.session, &v3, parent).await.unwrap(), json(r#"{"count":1,"migrated":true}"#));
    assert_eq!(migrations.load(Ordering::SeqCst), 1);
    let child = fork(&test.session, parent, fork_at).await.unwrap();
    assert_eq!(migrations.load(Ordering::SeqCst), 1);
    let record = find_record(&test, "fork.stored-version", child.id).await.unwrap();
    let stored = test.storage.document(record.id, DocumentPoint::Current, &context()).await.unwrap().unwrap();
    assert_eq!(stored.version, 1);
    assert_eq!(stored.value, json(r#"{"count":1}"#));
    assert_eq!(snapshot(&test.session, &v3, child.id).await.unwrap(), json(r#"{"count":1,"migrated":true}"#));
    assert_eq!(migrations.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn copies_the_incarnation_alive_at_the_fork_point_across_retirement_and_recreation() {
    let doc = rewindable("fork.incarnations", r#"{"value":"initial"}"#);
    let test = open_test_session();
    let parent = create_conversation(&test.session).await;
    let token = doc.clone();
    let old_at =
        entry_with(&test.session, parent, "old", move |tx| async move { set_value(&tx, &token, parent, "old").await })
            .await;
    let token = doc.clone();
    let retired_at =
        entry_with(&test.session, parent, "retired", move |tx| async move { tx.retire_doc(&token, parent).await })
            .await;
    let token = doc.clone();
    let new_at =
        entry_with(&test.session, parent, "new", move |tx| async move { set_value(&tx, &token, parent, "new").await })
            .await;
    let old_child = fork(&test.session, parent, old_at).await.unwrap().id;
    let empty_child = fork(&test.session, parent, retired_at).await.unwrap().id;
    let new_child = fork(&test.session, parent, new_at).await.unwrap().id;
    assert_eq!(snapshot(&test.session, &doc, old_child).await.unwrap(), json(r#"{"value":"old"}"#));
    assert!(snapshot(&test.session, &doc, empty_child).await.is_none());
    assert_eq!(snapshot(&test.session, &doc, new_child).await.unwrap(), json(r#"{"value":"new"}"#));
}

struct Work;

impl TaskTemplate for Work {
    fn name(&self) -> &str {
        "fork.scope.work"
    }
    fn version(&self) -> u32 {
        1
    }
    fn initial(&self, _input: &Json) -> DurableResult<Json> {
        Ok(json(r#"{"phase":"start"}"#))
    }
}

#[tokio::test]
async fn copies_only_conversation_documents_leaving_session_and_task_documents_in_their_original_scopes() {
    let copied = latest("fork.scope.conversation", LatestFork::Current, r#"{"value":"conversation"}"#);
    let session_only =
        define_doc(DocSpec::new("fork.scope.session", 1, SessionScope, || json(r#"{"value":"session"}"#)));
    let session_only = session_only.unwrap();
    let task_only = define_doc(DocSpec::new("fork.scope.task", 1, TaskScope, || json(r#"{"value":"task"}"#))).unwrap();
    let test = open_test_session();
    let parent = create_conversation(&test.session).await;
    let tokens = (copied.clone(), session_only.clone(), task_only.clone());
    let (fork_at, task_id) = test
        .session
        .commit(
            move |tx| async move {
                let entry = tx.append_entry(parent, EntryDraft::new("point")).await?;
                let task_id = tx.create_task(Arc::new(Work), Json::Null, TaskOptions::conversation(parent)).await?;
                tx.doc(&tokens.0, parent).await?;
                tx.doc(&tokens.1, ()).await?;
                tx.doc(&tokens.2, task_id).await?;
                Ok((entry.id, task_id))
            },
            &context(),
        )
        .await
        .unwrap();
    let child = fork(&test.session, parent, fork_at).await.unwrap();
    flush().await;
    let kinds: Vec<_> = copies(&test.storage.last_commit()).into_iter().map(|(record, _)| record.kind).collect();
    assert_eq!(kinds, ["fork.scope.conversation"]);
    let published: Vec<_> = document_copy_changes(&test.last_publication())
        .iter()
        .map(|change| change_record(change).kind.clone())
        .collect();
    assert_eq!(published, ["fork.scope.conversation"]);
    assert_eq!(snapshot(&test.session, &copied, child.id).await.unwrap(), json(r#"{"value":"conversation"}"#));
    assert_eq!(snapshot(&test.session, &session_only, ()).await.unwrap(), json(r#"{"value":"session"}"#));
    assert_eq!(snapshot(&test.session, &task_only, task_id).await.unwrap(), json(r#"{"value":"task"}"#));
}

#[tokio::test]
async fn copies_every_family_member_across_storage_scan_pages() {
    let initial = |seed: i64| json(&format!(r#"{{"value":{seed}}}"#));
    let family =
        define_doc_family(DocFamilySpec::new("fork.pagination", 1, LatestConversation(LatestFork::Current), initial))
            .unwrap();
    let test = open_test_session();
    let parent = create_conversation(&test.session).await;
    let fork_at = entry_with(&test.session, parent, "point", |_| async { Ok(()) }).await;
    let token = family.clone();
    test.session
        .commit(
            move |tx| async move {
                let drafts: Vec<_> =
                    (0..260).map(|index| tx.family_doc(&token, parent, &format!("member-{index}"), &index)).collect();
                for draft in futures::future::join_all(drafts).await {
                    draft?;
                }
                Ok(())
            },
            &context(),
        )
        .await
        .unwrap();
    let child = fork(&test.session, parent, fork_at).await.unwrap();
    assert_eq!(copies(&test.storage.last_commit()).len(), 260);
    let ctx = context();
    let first = test.session.family_snapshot(&family, child.id, "member-0", &ctx).await.unwrap();
    assert_eq!(first.unwrap(), json(r#"{"value":0}"#));
    let last = test.session.family_snapshot(&family, child.id, "member-259", &ctx).await.unwrap();
    assert_eq!(last.unwrap(), json(r#"{"value":259}"#));
}
