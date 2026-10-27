//! Session replay items (upstream `interactive-mode-compaction.test.ts`, message meta part).

use serde_json::{Value, json};

use super::RenderItem;
use crate::interactive::mode::InteractiveMode;
use crate::markdown_transform::MessageMeta;
use crate::session::entry::Entry;

fn entry(value: Value) -> Entry {
    let Value::Object(raw) = value else { panic!("entry object") };
    Entry::from_raw(raw)
}

#[test]
fn replayed_messages_carry_their_entry_id_and_timestamp() {
    let usage = json!({"input": 1, "output": 1, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 2,
        "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0}});
    let entries = [
        entry(json!({"type": "compaction", "id": "current", "parentId": null, "timestamp": "2025-01-02T00:00:00Z",
            "summary": "previous summary", "firstKeptEntryId": "kept", "tokensBefore": 10, "usage": usage})),
        entry(json!({"type": "message", "id": "kept", "parentId": "current", "timestamp": "2025-01-01T00:00:00Z",
            "message": {"role": "user", "content": "hello", "timestamp": 1}})),
    ];
    let metas: Vec<Option<MessageMeta>> = InteractiveMode::render_items_of(&entries)
        .into_iter()
        .map(|item| match item {
            RenderItem::Message { meta, .. } => meta,
            _ => None,
        })
        .collect();
    let meta = |id: &str, timestamp: &str| Some(MessageMeta { id: id.into(), timestamp: timestamp.into() });
    assert_eq!(metas, [meta("current", "2025-01-02T00:00:00Z"), None, meta("kept", "2025-01-01T00:00:00Z")]);
}
