//! Invalid and cancelled requests of upstream `test/harness-storage-failure.test.ts`: they fail only their call.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use pi_chord::{AbortController, ChordError, Json, with_abort_signal};
use pi_durable::harness::*;
use pi_durable::session::Tx;
use pi_durable::*;
use serde_json::json;

use super::support::{ControlledStorage, StorageCall, context};
use super::{Started, note, started};

// `pi.live` keeps no history, so upstream's `snapshotAsOf(LiveDoc)` request is rejected by the Rust type system and
// is left out below.
#[tokio::test]
async fn fails_only_the_call_for_an_invalid_request() {
    let Started { harness, root, reports, .. } = started(Arc::new(pi_durable::storage::MemoryStorage::new())).await;
    note(&root).await.unwrap();
    let bad: pi_durable::Cursor = Json::from(json!({ "after": "x" })).as_object().cloned().unwrap();
    let error = root.entries(EntryRange::default(), 10, Some(bad), &context()).await.map(|_| ()).unwrap_err();
    assert_eq!(error.name(), "StorageRequestError", "{error}");
    let ascending = EntryRange { order: Some(ScanOrder::Ascending), ..EntryRange::default() };
    let page = root.entries(ascending.clone(), 1, None, &context()).await.unwrap();
    let fallback = || Json::from(json!({ "after": 1, "order": "ascending" })).as_object().cloned().unwrap();
    let cursor = page.next.unwrap_or_else(fallback);
    let descending = EntryRange { order: Some(ScanOrder::Descending), ..EntryRange::default() };
    let error = root.entries(descending, 1, Some(cursor), &context()).await.map(|_| ()).unwrap_err();
    assert_eq!(error.name(), "StorageRequestError", "{error}");
    let query =
        EntryQuery { conversation_id: ConversationId(999_999), min_entry_id: None, max_entry_id: None, order: None };
    let scan = move |tx: Tx| async move { tx.scan_entries(query, 10, None).await.map(|_| ()) };
    assert_eq!(root.commit(scan, &context()).await.unwrap_err().name(), "StorageRequestError");
    note(&root).await.unwrap();
    tokio::time::sleep(Duration::from_millis(5)).await;
    assert!(harness.session().failure().is_none());
    assert!(reports.lock().unwrap().is_empty());
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn does_not_fail_it_for_a_read_its_caller_cancelled() {
    let storage = Arc::new(ControlledStorage::default());
    let Started { harness, root, .. } = started(storage.clone()).await;
    let controller = AbortController::new();
    let cancel = Arc::new(AtomicBool::new(false));
    let (armed, aborter) = (cancel.clone(), controller.clone());
    *storage.call_hook.lock().unwrap() = Some(Arc::new(move |call: StorageCall| {
        let fire = call.method == "scan_entries" && armed.swap(false, Ordering::SeqCst);
        if fire {
            aborter.abort(Some(ChordError::error("caller gave up")));
        }
        Box::pin(async move { if fire { Err(DurableError::error("caller gave up")) } else { Ok(()) } })
    }));
    cancel.store(true, Ordering::SeqCst);
    let cancelling = with_abort_signal(&controller.signal(), &context());
    let error = root.context(&cancelling, None).await.map(|_| ()).unwrap_err();
    assert!(error.to_string().contains("caller gave up"), "{error}");
    note(&root).await.unwrap();
    harness.close(&context()).await.unwrap();
}
