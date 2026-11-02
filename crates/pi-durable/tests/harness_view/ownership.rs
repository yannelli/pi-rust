//! Upstream `harness-view.test.ts`: a live window releases the entries it trims.

use std::sync::Weak;

use pi_durable::storage::open_sqlite_storage;

use super::*;
use crate::support::{ValueCall, on_value};

#[tokio::test]
async fn releases_trimmed_entries_while_the_watch_remains_live() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("session.sqlite");
    let storage = open_sqlite_storage(&database.display().to_string()).await.unwrap();
    let (harness, root) = open_chat(Arc::new(storage), &chat_setup(Default::default())).await;
    let window = ConversationWindow { min: Some(5), max: Some(10), from: None };
    let watch = root.watch(&context(), Some(window)).await.unwrap();
    let first: Arc<Mutex<Option<Weak<JsonMap>>>> = Arc::default();
    let captured = first.clone();
    let (delivered, mut deliveries) = tokio::sync::mpsc::unbounded_channel();
    watch
        .start(on_value(move |ValueCall { value, .. }, _| {
            let entries = value.get("entries").unwrap().as_array().unwrap();
            let mut first = captured.lock().unwrap();
            if first.is_none()
                && let Some(Json::Object(entry)) = entries.first()
            {
                *first = Some(Arc::downgrade(entry));
            }
            delivered.send(entries.len()).unwrap();
            async { Ok(()) }
        }))
        .unwrap();

    for index in 0..201 {
        let id = root.id;
        root.commit(move |tx: Tx| async move { tx.append_entry(id, EntryDraft::new("note")).await }, &context())
            .await
            .unwrap();
        let count = tokio::time::timeout(std::time::Duration::from_secs(5), deliveries.recv()).await.unwrap().unwrap();
        assert!(count <= 10);
        if index == 0 {
            assert!(first.lock().unwrap().as_ref().unwrap().upgrade().is_some());
        }
    }
    drained().await;
    assert!(first.lock().unwrap().as_ref().unwrap().upgrade().is_none());
    assert!(watch.value().get("entries").unwrap().as_array().unwrap().len() <= 10);
    watch.stop().await;
    harness.close(&context()).await.unwrap();
}
