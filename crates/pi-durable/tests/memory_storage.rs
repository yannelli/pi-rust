//! Port of upstream `test/memory-storage.test.ts`.

use std::sync::Arc;

use pi_chord::{BACKGROUND_CONTEXT, Json};
use pi_durable::storage::MemoryStorage;
use pi_durable::testing::storage_conformance::StorageTest;
use pi_durable::*;

async fn with_memory_storage(test: StorageTest) {
    test(Arc::new(MemoryStorage::new())).await;
}

mod conformance {
    pi_durable::storage_conformance_tests!(super::with_memory_storage);
}

#[tokio::test]
async fn does_not_expose_retained_state_through_a_prepared_commit() {
    let storage = MemoryStorage::new();
    let root = ConversationRecord { id: ROOT_CONVERSATION_ID, parent: None, owner: None };
    storage.commit(&[StorageWrite::Conversation { value: root }], &|_| {}, &BACKGROUND_CONTEXT).await.unwrap();
    let entry_id = EntryId(2);
    let mut data = Json::parse(r#"{"nested":[1]}"#).unwrap();
    let entry = EntryRecord {
        kind: "test".into(),
        model: None,
        data: Some(data.clone()),
        edits: None,
        id: entry_id,
        conversation_id: ROOT_CONVERSATION_ID,
        head: None,
        by_task_id: None,
    };
    let prepared = storage.prepare_commit(&[StorageWrite::Entry { value: entry }], None).unwrap();
    // The prepared writes are only lent out; a caller that changes its own copy leaves them intact.
    let StorageWrite::Entry { value } = &prepared.writes()[0] else { panic!("expected an entry write") };
    data.get_mut("nested").unwrap().push(2).unwrap();
    assert_eq!(value.data, Some(Json::parse(r#"{"nested":[1]}"#).unwrap()));

    assert_eq!(storage.apply(&prepared), Seq(2));
    assert_eq!(storage.apply(&prepared), Seq(2));
    let stored = storage.entry(entry_id, &BACKGROUND_CONTEXT).await.unwrap().unwrap();
    assert_eq!(stored.entry.data, Some(Json::parse(r#"{"nested":[1]}"#).unwrap()));
}
