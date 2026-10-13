//! Document content: rewindable history, long delta tails, copies, and version transitions.

use std::sync::Arc;

use pi_chord::Json;
use pi_chord::delta::Op;

use super::{context, create_root, expect_err, json, mint, ops};
use crate::ids::{ConversationId, DocumentId, Seq};
use crate::types::{
    ConversationRecord, DocumentAddress, DocumentContent, DocumentCopySource, DocumentCreate, DocumentFork,
    DocumentHistory, DocumentPoint, DocumentQuery, DocumentScope, Storage, StorageWrite, StoredDocument,
};

pub(crate) fn conversation_document(
    id: DocumentId,
    kind: &str,
    conversation_id: ConversationId,
    history: DocumentHistory,
    fork: DocumentFork,
) -> DocumentCreate {
    DocumentCreate {
        id,
        kind: kind.into(),
        key: None,
        scope: DocumentScope::Conversation { conversation_id },
        history: Some(history),
        fork: Some(fork),
    }
}

pub(crate) fn create(record: &DocumentCreate, version: u32, value: Json) -> StorageWrite {
    StorageWrite::DocumentCreate { record: record.clone(), content: DocumentContent::Base { version, value } }
}

pub(crate) fn change_base(id: DocumentId, version: u32, value: Json) -> StorageWrite {
    StorageWrite::DocumentChange { id, content: DocumentContent::Base { version, value } }
}

pub(crate) fn change_delta(id: DocumentId, version: u32, ops: Vec<Op>) -> StorageWrite {
    StorageWrite::DocumentChange { id, content: DocumentContent::Delta { version, ops } }
}

pub(crate) async fn read(storage: &dyn Storage, id: DocumentId, at: DocumentPoint) -> Option<StoredDocument> {
    storage.document(id, at, context()).await.unwrap()
}

pub(crate) async fn value(storage: &dyn Storage, id: DocumentId, at: DocumentPoint) -> Option<Json> {
    read(storage, id, at).await.map(|stored| stored.value)
}

pub async fn reconstructs_rewindable_documents(storage: Arc<dyn Storage>) {
    let storage = &*storage;
    let ctx = context();
    let root_id = create_root(storage).await;
    let first_id: DocumentId = mint(storage).await;
    let first_record =
        conversation_document(first_id, "conversation.notes", root_id, DocumentHistory::Rewindable, DocumentFork::AsOf);
    let mut initial = json(r#"{"items":["a"],"nested":{"count":1}}"#);
    let created_at = storage.commit(&[create(&first_record, 1, initial.clone())], ctx).await.unwrap();
    let delta = ops(r#"[["p",["items"],1,0,["b"]],["s",["nested","count"],2]]"#);
    let changed_at = storage.commit(&[change_delta(first_id, 1, delta)], ctx).await.unwrap();

    initial.get_mut("items").unwrap().push("caller mutation").unwrap();
    let at_create = read(storage, first_id, DocumentPoint::At(created_at)).await.unwrap();
    assert_eq!((at_create.version, at_create.deltas_since_base), (1, 0));
    assert_eq!(at_create.value, json(r#"{"items":["a"],"nested":{"count":1}}"#));
    let mut changed = read(storage, first_id, DocumentPoint::At(changed_at)).await.unwrap();
    assert_eq!(changed.value, json(r#"{"items":["a","b"],"nested":{"count":2}}"#));
    assert_eq!(changed.deltas_since_base, 1);
    changed.value.get_mut("items").unwrap().push("read mutation").unwrap();
    let current = value(storage, first_id, DocumentPoint::Current).await;
    assert_eq!(current, Some(json(r#"{"items":["a","b"],"nested":{"count":2}}"#)));

    let checkpoint = json(r#"{"items":["checkpoint"],"nested":{"count":3}}"#);
    let checkpoint_at = storage.commit(&[change_base(first_id, 2, checkpoint.clone())], ctx).await.unwrap();
    let replace = ops(r#"[["r",{"items":["replacement"],"nested":{"count":4}}]]"#);
    let replaced_at = storage.commit(&[change_delta(first_id, 2, replace)], ctx).await.unwrap();
    let at_changed = read(storage, first_id, DocumentPoint::At(changed_at)).await.unwrap();
    assert_eq!(at_changed.version, 1);
    assert_eq!(at_changed.value, json(r#"{"items":["a","b"],"nested":{"count":2}}"#));
    let at_checkpoint = read(storage, first_id, DocumentPoint::At(checkpoint_at)).await.unwrap();
    assert_eq!((at_checkpoint.version, at_checkpoint.value, at_checkpoint.deltas_since_base), (2, checkpoint, 0));
    let at_replaced = read(storage, first_id, DocumentPoint::At(replaced_at)).await.unwrap();
    assert_eq!(at_replaced.value, json(r#"{"items":["replacement"],"nested":{"count":4}}"#));
    assert_eq!(at_replaced.deltas_since_base, 1);
    assert_eq!(read(storage, first_id, DocumentPoint::Current).await.unwrap().deltas_since_base, 1);

    let second_id: DocumentId = mint(storage).await;
    let second_record = DocumentCreate { id: second_id, ..first_record.clone() };
    let writes = [
        create(&second_record, 1, json(r#"{"items":["new"]}"#)),
        StorageWrite::DocumentRetire { id: first_id },
        change_delta(first_id, 2, ops(r#"[["s",["retiring"],true]]"#)),
    ];
    let retired_at = storage.commit(&writes, ctx).await.unwrap();
    let address = DocumentAddress { kind: first_record.kind.clone(), scope: first_record.scope, key: None };
    let found = storage.find_document(&address, DocumentPoint::At(changed_at), ctx).await.unwrap();
    assert_eq!(found.map(|record| record.id), Some(first_id));
    let found = storage.find_document(&address, DocumentPoint::At(retired_at), ctx).await.unwrap().unwrap();
    assert_eq!((found.id, found.created_at), (second_id, retired_at));
    let scan = |at: Seq| async move {
        let query = DocumentQuery { scope: first_record.scope, at: DocumentPoint::At(at), kind: None };
        let page = storage.scan_documents(&query, 10, None, ctx).await.unwrap();
        page.items.iter().map(|record| record.id).collect::<Vec<_>>()
    };
    assert_eq!(scan(changed_at).await, [first_id]);
    assert_eq!(scan(retired_at).await, [second_id]);
    assert_eq!(read(storage, first_id, DocumentPoint::At(retired_at)).await, None);
    assert_eq!(value(storage, second_id, DocumentPoint::Current).await, Some(json(r#"{"items":["new"]}"#)));
}

fn rows(count: u64, value: impl Fn(u64) -> (i64, String)) -> Json {
    let rows = (0..count).map(|index| {
        let (value, stable) = value(index);
        serde_json::json!({ "value": value, "stable": stable })
    });
    Json::from(serde_json::Value::Array(rows.collect()))
}

fn set_row(target: &mut Json, index: usize, revision: i64) {
    target.get_mut("rows").unwrap().at_mut(index).unwrap().set("value", -revision).unwrap();
    target.set("revision", revision).unwrap();
}

fn row_ops(index: usize, revision: i64) -> Vec<Op> {
    ops(&format!(r#"[["s",["rows",{index},"value"],{}],["s",["revision"],{revision}]]"#, -revision))
}

pub async fn streams_long_document_tails(storage: Arc<dyn Storage>) {
    let storage = &*storage;
    let ctx = context();
    let root_id = create_root(storage).await;
    let id: DocumentId = mint(storage).await;
    let record =
        conversation_document(id, "conversation.long-tail", root_id, DocumentHistory::Rewindable, DocumentFork::AsOf);
    let mut initial = Json::empty_object();
    initial.set("revision", 0).unwrap();
    initial.set("rows", rows(512, |value| (value as i64, format!("row-{value}")))).unwrap();
    let created_at = storage.commit(&[create(&record, 1, initial.clone())], ctx).await.unwrap();
    let mut before_replacement = initial.clone();
    let mut before_replacement_at = created_at;
    for revision in 1..=24_i64 {
        let index = (revision as usize * 17) % 512;
        set_row(&mut before_replacement, index, revision);
        before_replacement_at = storage.commit(&[change_delta(id, 1, row_ops(index, revision))], ctx).await.unwrap();
    }

    let mut replacement = Json::empty_object();
    replacement.set("revision", 100).unwrap();
    replacement.set("rows", rows(512, |value| (10_000 + value as i64, format!("new-{value}")))).unwrap();
    let snapshot = replacement.clone();
    let replacement_at =
        storage.commit(&[change_delta(id, 1, vec![Op::Replace(replacement.clone())])], ctx).await.unwrap();
    set_row(&mut replacement, 0, 999);

    let mut current = snapshot.clone();
    for revision in 101..=124_i64 {
        let index = (revision as usize * 19) % 512;
        set_row(&mut current, index, revision);
        storage.commit(&[change_delta(id, 1, row_ops(index, revision))], ctx).await.unwrap();
    }

    assert_eq!(value(storage, id, DocumentPoint::At(created_at)).await, Some(initial));
    assert_eq!(value(storage, id, DocumentPoint::At(before_replacement_at)).await, Some(before_replacement));
    assert_eq!(value(storage, id, DocumentPoint::At(replacement_at)).await, Some(snapshot));
    let mut read_value = value(storage, id, DocumentPoint::Current).await.unwrap();
    assert_eq!(read_value, current);
    read_value.get_mut("rows").unwrap().at_mut(0).unwrap().set("value", -1_000).unwrap();
    assert_eq!(value(storage, id, DocumentPoint::Current).await, Some(current));
}

pub async fn copies_document_bases_independently(storage: Arc<dyn Storage>) {
    let storage = &*storage;
    let ctx = context();
    let root_id = create_root(storage).await;
    let child_id: ConversationId = mint(storage).await;
    let second_child_id: ConversationId = mint(storage).await;
    let conversation = |id| StorageWrite::Conversation { value: ConversationRecord { id, parent: None, owner: None } };
    storage.commit(&[conversation(child_id), conversation(second_child_id)], ctx).await.unwrap();
    let source_id: DocumentId = mint(storage).await;
    let rewindable = |id, conversation_id| {
        conversation_document(id, "copy.source", conversation_id, DocumentHistory::Rewindable, DocumentFork::AsOf)
    };
    let source = rewindable(source_id, root_id);
    let created_at =
        storage.commit(&[create(&source, 2, json(r#"{"count":1,"rows":[{"value":"base"}]}"#))], ctx).await.unwrap();
    let delta = ops(r#"[["s",["count"],2],["p",["rows"],1,0,[{"value":"current"}]]]"#);
    storage.commit(&[change_delta(source_id, 2, delta)], ctx).await.unwrap();
    let historical_copy: DocumentId = mint(storage).await;
    let current_copy: DocumentId = mint(storage).await;
    let retired_copy: DocumentId = mint(storage).await;
    let copy =
        |record: DocumentCreate, id, at| StorageWrite::DocumentCopy { record, source: DocumentCopySource { id, at } };
    let writes = [
        copy(rewindable(historical_copy, child_id), source_id, DocumentPoint::At(created_at)),
        copy(rewindable(current_copy, second_child_id), source_id, DocumentPoint::Current),
        copy(rewindable(retired_copy, root_id), source_id, DocumentPoint::Current),
        StorageWrite::DocumentRetire { id: retired_copy },
    ];
    storage.commit(&writes, ctx).await.unwrap();
    let historical = read(storage, historical_copy, DocumentPoint::Current).await.unwrap();
    assert_eq!((historical.version, historical.value), (2, json(r#"{"count":1,"rows":[{"value":"base"}]}"#)));
    let current_value = json(r#"{"count":2,"rows":[{"value":"base"},{"value":"current"}]}"#);
    let current = read(storage, current_copy, DocumentPoint::Current).await.unwrap();
    assert_eq!((current.version, current.value), (2, current_value.clone()));
    assert_eq!(read(storage, retired_copy, DocumentPoint::Current).await, None);

    let writes =
        [change_base(source_id, 2, json(r#"{"count":99,"rows":[]}"#)), StorageWrite::DocumentRetire { id: source_id }];
    storage.commit(&writes, ctx).await.unwrap();
    assert_eq!(value(storage, current_copy, DocumentPoint::Current).await, Some(current_value.clone()));

    let latest_source_id: DocumentId = mint(storage).await;
    let latest_copy_id: DocumentId = mint(storage).await;
    let latest_source =
        conversation_document(latest_source_id, "copy.latest", root_id, DocumentHistory::Latest, DocumentFork::Current);
    storage.commit(&[create(&latest_source, 4, json(r#"{"retained":"copy"}"#))], ctx).await.unwrap();
    let latest_copy = DocumentCreate {
        id: latest_copy_id,
        scope: DocumentScope::Conversation { conversation_id: child_id },
        ..latest_source.clone()
    };
    storage.commit(&[copy(latest_copy, latest_source_id, DocumentPoint::Current)], ctx).await.unwrap();
    let writes = [
        change_base(latest_source_id, 4, json(r#"{"retained":"source-only"}"#)),
        StorageWrite::DocumentRetire { id: latest_source_id },
    ];
    storage.commit(&writes, ctx).await.unwrap();
    let copied = read(storage, latest_copy_id, DocumentPoint::Current).await.unwrap();
    assert_eq!((copied.version, copied.value), (4, json(r#"{"retained":"copy"}"#)));

    // Rejected without effect: the copy and the retirement in the same batch are both absent.
    let conflict_id: DocumentId = mint(storage).await;
    let writes = [
        copy(rewindable(conflict_id, child_id), current_copy, DocumentPoint::Current),
        StorageWrite::DocumentRetire { id: current_copy },
    ];
    assert!(storage.commit(&writes, ctx).await.is_err());
    assert_eq!(read(storage, conflict_id, DocumentPoint::Current).await, None);
    assert_eq!(value(storage, current_copy, DocumentPoint::Current).await, Some(current_value));

    let mismatch_id: DocumentId = mint(storage).await;
    let mismatched = DocumentCreate { kind: "copy.mismatch".into(), ..rewindable(mismatch_id, child_id) };
    assert!(storage.commit(&[copy(mismatched, current_copy, DocumentPoint::Current)], ctx).await.is_err());
    assert_eq!(read(storage, mismatch_id, DocumentPoint::Current).await, None);
}

pub async fn uses_bases_for_version_transitions(storage: Arc<dyn Storage>) {
    let storage = &*storage;
    let ctx = context();
    create_root(storage).await;
    let id: DocumentId = mint(storage).await;
    let record = DocumentCreate {
        id,
        kind: "session.settings".into(),
        key: None,
        scope: DocumentScope::Session,
        history: None,
        fork: None,
    };
    storage.commit(&[create(&record, 1, json(r#"{"count":1}"#))], ctx).await.unwrap();
    storage.commit(&[change_delta(id, 1, ops(r#"[["s",["count"],2]]"#))], ctx).await.unwrap();
    let migrated_at = storage.commit(&[change_base(id, 2, json(r#"{"count":3}"#))], ctx).await.unwrap();
    let current = read(storage, id, DocumentPoint::Current).await.unwrap();
    assert_eq!((current.version, current.value), (2, json(r#"{"count":3}"#)));
    expect_err(storage.document(id, DocumentPoint::At(migrated_at), ctx).await, "does not retain historical content");

    let stale = storage.commit(&[change_delta(id, 1, ops(r#"[["s",["count"],4]]"#))], ctx).await;
    expect_err(stale, "version transition requires a base");
    assert_eq!(value(storage, id, DocumentPoint::Current).await, Some(json(r#"{"count":3}"#)));
    storage.commit(&[StorageWrite::DocumentRetire { id }], ctx).await.unwrap();
    assert_eq!(read(storage, id, DocumentPoint::Current).await, None);
}
