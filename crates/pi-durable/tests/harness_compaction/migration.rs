//! Upstream `harness-compaction.test.ts`: `pi.live` version 2 names a compaction's trigger `trigger`.

use std::sync::{Arc, LazyLock};

use pi_chord::Json;
use pi_durable::documents::{DocSpec, DocToken, LatestConversation, LatestFork, define_doc};
use pi_durable::harness::live::LIVE_DOC;
use pi_durable::session::{SessionOptions, Tx, create_session};
use pi_durable::storage::MemoryStorage;
use pi_durable::{
    ConversationId, ConversationOwnership, DocumentAddress, DocumentPoint, DocumentScope, Storage, StoredDocument,
};
use serde_json::{Value, json};

use super::support::context;

/// `pi.live` as version 1 wrote it.
static LIVE_V1: LazyLock<DocToken<Json, LatestConversation>> = LazyLock::new(|| {
    let scope = LatestConversation(LatestFork::Initial);
    define_doc(DocSpec::new("pi.live", 1, scope, Json::empty_object, |_, _, _| Ok(false))).unwrap()
});

async fn stored(storage: &MemoryStorage, conversation_id: ConversationId) -> StoredDocument {
    let scope = DocumentScope::Conversation { conversation_id };
    let address = DocumentAddress { kind: "pi.live".into(), scope, key: None };
    let record = storage.find_document(&address, DocumentPoint::Current, &context()).await.unwrap().unwrap();
    storage.document(record.id, DocumentPoint::Current, &context()).await.unwrap().unwrap()
}

#[tokio::test]
async fn reads_a_version_1_value_with_each_compactions_reason_as_trigger_and_stores_it_migrated_on_its_next_write() {
    let storage = Arc::new(MemoryStorage::new());
    let session = create_session(storage.clone(), SessionOptions::default());
    let create = |tx: Tx| async move { Ok(tx.create_conversation(ConversationOwnership::Ownerless).await?.id) };
    let conversation_id = session.commit(create, &context()).await.unwrap();
    let write = move |tx: Tx| async move {
        let compactions = json!([{ "taskId": 7, "reason": "threshold", "blocking": true, "attempt": 2 }]);
        let failure = json!({ "taskId": 5, "reason": "manual", "error": "no model" });
        let live = tx.doc(&LIVE_V1, conversation_id).await?;
        live.update(|value| value.set("compactions", Json::from(compactions)))??;
        live.update(|value| value.set("compactionFailure", Json::from(failure)))?.map_err(Into::into)
    };
    session.commit(write, &context()).await.unwrap();
    // The version 2 token reloads the stored value and migrates it.
    let read = session.snapshot(&LIVE_DOC, conversation_id, &context()).await.unwrap().unwrap();
    let migrated = json!({
        "compactions": [{ "taskId": 7, "trigger": "threshold", "blocking": true, "attempt": 2 }],
        "compactionFailure": { "taskId": 5, "trigger": "manual", "error": "no model" },
    });
    assert_eq!(read.to_serde(), migrated);
    let keys: Vec<String> = read["compactions"].as_array().unwrap()[0]
        .as_object()
        .unwrap()
        .iter()
        .map(|(key, _)| key.to_string())
        .collect();
    assert_eq!(keys, ["taskId", "trigger", "blocking", "attempt"]);
    // A read migrates in memory only: Storage keeps the version 1 value.
    let before = stored(&storage, conversation_id).await;
    assert_eq!(before.version, 1);
    let value = before.value.to_serde();
    assert_eq!(
        (&value["compactions"][0]["reason"], &value["compactionFailure"]["reason"]),
        (&json!("threshold"), &json!("manual"))
    );
    let bump = move |tx: Tx| async move {
        tx.doc(&LIVE_DOC, conversation_id).await?.edit(|live| {
            if let Some(status) = live.compactions.as_mut().and_then(|statuses| statuses.first_mut()) {
                status.attempt = 3;
            }
        })
    };
    session.commit(bump, &context()).await.unwrap();
    let after = stored(&storage, conversation_id).await;
    assert_eq!(after.version, 2);
    let value: Value = after.value.to_serde();
    assert_eq!(
        (&value["compactions"][0]["trigger"], &value["compactions"][0]["attempt"]),
        (&json!("threshold"), &json!(3))
    );
    assert_eq!(value["compactionFailure"]["trigger"], "manual");
    session.close(&context()).await.unwrap();
}

fn keys(value: &Json) -> Vec<String> {
    value.as_object().unwrap().iter().map(|(key, _)| key.to_string()).collect()
}

#[test]
fn migrates_version_1_json_whole_renaming_only_reason_in_any_key_order_and_keeping_unknown_fields() {
    let raw = Json::from(json!({
        "zUnknown": { "keep": true },
        "compactionFailure": { "error": "no model", "reason": "manual", "taskId": 5, "extra": 1 },
        "compactions": [{ "attempt": 2, "reason": "threshold", "extra": "x", "taskId": 7, "blocking": true }],
        "aUnknown": [1, { "reason": "unrelated" }],
    }));
    let migrated = LIVE_DOC.definition().migrate(&raw, 1).unwrap();
    let expected = json!({
        "zUnknown": { "keep": true },
        "compactionFailure": { "error": "no model", "trigger": "manual", "taskId": 5, "extra": 1 },
        "compactions": [{ "attempt": 2, "trigger": "threshold", "extra": "x", "taskId": 7, "blocking": true }],
        "aUnknown": [1, { "reason": "unrelated" }],
    });
    assert_eq!(migrated.to_serde(), expected);
    assert_eq!(keys(&migrated), ["zUnknown", "compactionFailure", "compactions", "aUnknown"]);
    assert_eq!(keys(&migrated["compactionFailure"]), ["error", "trigger", "taskId", "extra"]);
    let status = &migrated["compactions"].as_array().unwrap()[0];
    assert_eq!(keys(status), ["attempt", "trigger", "extra", "taskId", "blocking"]);
}
