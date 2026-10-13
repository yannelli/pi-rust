//! Records serialize to the JSON text upstream writes.

use pi_chord::{CopyJsonOptions, Json, copy_json};
use pi_durable::*;

fn text<T: serde::Serialize>(value: &T) -> String {
    copy_json(value, CopyJsonOptions { omit_undefined_properties: true }).unwrap().to_string()
}

#[test]
fn task_record_matches_upstream_layout() {
    let record = TaskRecord {
        id: TaskId(5),
        conversation_id: ConversationId(1),
        kind: "demo".into(),
        version: 1,
        input: Json::parse(r#"{"b":1,"2":true,"1":null}"#).unwrap(),
        owner: None,
        background: false,
        abort_requested: false,
        abort_reason: None,
        abandon_on_restart: None,
        state: TaskState::Pending { checkpoint: Json::parse(r#"{"phase":"start"}"#).unwrap() },
        memos: None,
        started_at: None,
        ended_at: None,
    };
    let encoded = text(&record);
    assert_eq!(
        encoded,
        r#"{"id":5,"conversationId":1,"kind":"demo","version":1,"input":{"1":null,"2":true,"b":1},"background":false,"abortRequested":false,"state":{"status":"pending","checkpoint":{"phase":"start"}}}"#
    );
    let decoded: TaskRecord = Json::parse(&encoded).unwrap().deserialize_into().unwrap();
    assert_eq!(decoded, record);
}

#[test]
fn document_content_and_writes_round_trip() {
    let content = DocumentContent::Base { version: 2, value: Json::parse(r#"{"a":1}"#).unwrap() };
    assert_eq!(text(&content), r#"{"version":2,"kind":"base","value":{"a":1}}"#);
    let write = StorageWrite::DocumentCopy {
        record: DocumentCreate {
            id: DocumentId(9),
            kind: "pi.agent".into(),
            key: None,
            scope: DocumentScope::Conversation { conversation_id: ConversationId(3) },
            history: Some(DocumentHistory::Latest),
            fork: Some(DocumentFork::Current),
        },
        source: DocumentCopySource { id: DocumentId(4), at: DocumentPoint::Current },
    };
    let encoded = text(&write);
    assert_eq!(
        encoded,
        r#"{"type":"document.copy","record":{"id":9,"kind":"pi.agent","scope":{"kind":"conversation","conversationId":3},"history":"latest","fork":"current"},"source":{"id":4,"at":"current"}}"#
    );
    let decoded: StorageWrite = Json::parse(&encoded).unwrap().deserialize_into().unwrap();
    assert_eq!(decoded, write);
}

#[test]
fn entry_record_with_messages_round_trips() {
    let encoded = r#"{"kind":"pi.user","model":[{"role":"user","content":"hi","timestamp":1}],"id":7,"conversationId":1,"head":7}"#;
    let entry: EntryRecord = Json::parse(encoded).unwrap().deserialize_into().unwrap();
    assert_eq!(entry.head, Some(EntryId(7)));
    assert_eq!(text(&entry), encoded);
}
