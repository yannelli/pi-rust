//! Semantic conformance cases every `Storage` must pass (spec section 10). Each case is a public async function that
//! panics on failure; [`storage_conformance_tests!`](crate::storage_conformance_tests) makes one `#[tokio::test]`
//! per case from a provider that opens a fresh storage, awaits the test once, and cleans up (upstream `withStorage`).

mod document_indexes;
mod documents;
mod records;
mod scans;
mod tasks;

use std::fmt::Debug;
use std::sync::Arc;

use futures::future::BoxFuture;
use pi_chord::delta::{Op, parse_ops};
use pi_chord::{BACKGROUND_CONTEXT, Context, Json};

pub use document_indexes::*;
pub use documents::{
    copies_document_bases_independently, reconstructs_rewindable_documents, streams_long_document_tails,
    uses_bases_for_version_transitions,
};
pub use records::{
    commits_mixed_table_writes_atomically, detaches_prototype_like_keys, detaches_writes_and_returned_records,
    keeps_one_global_id_namespace, rejects_operations_after_close, reserves_root_conversation_id,
};
pub use scans::*;
pub use tasks::*;

use crate::errors::DurableResult;
use crate::ids::{ConversationId, EntryId, ROOT_CONVERSATION_ID, TaskId};
use crate::types::{EntryRecord, Storage, StorageWrite, TaskRecord, TaskState};

/// The body of one case, run against the storage a provider opened.
pub type StorageTest = Box<dyn FnOnce(Arc<dyn Storage>) -> BoxFuture<'static, ()> + Send>;

/// One named conformance case.
#[derive(Clone, Copy)]
pub struct StorageConformanceCase {
    /// Upstream case name.
    pub name: &'static str,
    pub run: fn(Arc<dyn Storage>) -> BoxFuture<'static, ()>,
}

/// Calls `$callback!` with every case as `function => "upstream name"`.
#[doc(hidden)]
#[macro_export]
macro_rules! __for_each_storage_conformance_case {
    ($callback:ident ! { $($args:tt)* }) => {
        $crate::$callback! { $($args)*
            reserves_root_conversation_id => "reserves ID 1 for the immutable root conversation",
            commits_mixed_table_writes_atomically => "commits mixed table writes atomically and rolls all of them back on failure",
            detaches_writes_and_returned_records => "detaches retained writes and every returned record",
            detaches_prototype_like_keys => "detaches prototype-like JSON keys without changing object prototypes",
            indexes_entries_out_of_id_order => "indexes entries committed out of ID order",
            continues_entry_cursor_after_newer_commit => "continues an entry cursor below its last item after a newer commit",
            paginates_conversations_by_cursor => "paginates conversations by opaque cursor in ascending ID order",
            filters_conversations_by_owner => "filters and pages conversations by durable owner edges",
            scans_deep_fork_history => "scans deep fork history newest-first through every ancestor cap",
            scans_tables_in_either_order => "scans tables in either ID order and continues a cursor in its order",
            replaces_task_records => "replaces complete task records and pages filtered task scans",
            scans_waiting_and_completing_tasks => "stores owners and scans waiting and completing tasks by status",
            scans_sleeping_tasks => "stores and scans sleeping tasks by status",
            indexes_request_ids_per_conversation => "indexes request IDs per conversation and replaces complete submission records",
            stores_passive_write_submissions => "stores passive write submissions without input-only lifecycle states",
            reconstructs_rewindable_documents => "reconstructs rewindable documents and preserves half-open incarnations",
            streams_long_document_tails => "streams long document tails across root replacement deltas",
            copies_document_bases_independently => "copies stored document bases independently and rejects ambiguous sources",
            uses_bases_for_version_transitions => "uses bases for version transitions and rejects historical reads of current-only documents",
            indexes_addresses_and_scopes => "indexes logical addresses and exact-scope scans independently",
            keeps_document_lifecycle_atomic => "keeps document lifecycle failures atomic and gives create-plus-retire an empty lifetime",
            rolls_back_tables_on_document_failure => "rolls back record tables and secondary indexes when a document command fails",
            keeps_string_identities_lossless => "keeps indexed string identities lossless",
            keeps_one_global_id_namespace => "keeps one global record ID namespace and rejects exhausted ID minting",
            rejects_operations_after_close => "rejects every operation after close",
        }
    };
}

#[doc(hidden)]
#[macro_export]
macro_rules! __storage_conformance_list {
    ($($case:ident => $name:literal,)*) => {
        vec![$($crate::testing::storage_conformance::StorageConformanceCase {
            name: $name,
            run: |storage| ::std::boxed::Box::pin($crate::testing::storage_conformance::$case(storage)),
        },)*]
    };
}

#[doc(hidden)]
#[macro_export]
macro_rules! __storage_conformance_test_fns {
    ($provider:path; $($case:ident => $name:literal,)*) => {$(
        #[doc = $name]
        #[$crate::testing::__tokio::test(crate = "pi_durable::testing::__tokio")]
        async fn $case() {
            $provider(::std::boxed::Box::new(|storage| {
                ::std::boxed::Box::pin($crate::testing::storage_conformance::$case(storage))
            }))
            .await;
        }
    )*};
}

/// Generate one `#[tokio::test]` per conformance case. `$provider` is an `async fn(StorageTest)` that opens a fresh
/// storage, awaits the test with it, and cleans up.
#[macro_export]
macro_rules! storage_conformance_tests {
    ($provider:path) => {
        $crate::__for_each_storage_conformance_case!(__storage_conformance_test_fns! { $provider; });
    };
}

/// Every case, for runners that drive them without the macro.
pub fn storage_conformance_cases() -> Vec<StorageConformanceCase> {
    crate::__for_each_storage_conformance_case!(__storage_conformance_list! {})
}

pub(crate) fn context() -> &'static Context {
    &BACKGROUND_CONTEXT
}

pub(crate) fn json(text: &str) -> Json {
    Json::parse(text).expect("valid JSON")
}

pub(crate) fn ops(text: &str) -> Vec<Op> {
    parse_ops(&json(text)).expect("valid operations")
}

pub(crate) async fn mint<T: From<u64>>(storage: &dyn Storage) -> T {
    T::from(storage.mint_id().await.expect("mint an ID"))
}

pub(crate) async fn create_root(storage: &dyn Storage) -> ConversationId {
    let root = crate::types::ConversationRecord { id: ROOT_CONVERSATION_ID, parent: None, owner: None };
    storage.commit(&[StorageWrite::Conversation { value: root }], context()).await.expect("create the root");
    ROOT_CONVERSATION_ID
}

pub(crate) fn pending_task(id: TaskId, conversation_id: ConversationId, phase: &str) -> TaskRecord {
    TaskRecord {
        id,
        conversation_id,
        kind: "test.task".into(),
        version: 1,
        input: Json::from(serde_json::json!({ "value": id.0 })),
        owner: None,
        background: false,
        abort_requested: false,
        abort_reason: None,
        abandon_on_restart: None,
        state: TaskState::Pending { checkpoint: Json::from(serde_json::json!({ "phase": phase })) },
        memos: None,
        started_at: None,
        ended_at: None,
        restarts: None,
    }
}

pub(crate) fn entry(id: EntryId, conversation_id: ConversationId, kind: &str) -> EntryRecord {
    EntryRecord {
        kind: kind.into(),
        model: None,
        data: None,
        edits: None,
        id,
        conversation_id,
        head: None,
        by_task_id: None,
    }
}

/// Assert that `result` failed with a message that includes `includes`.
#[track_caller]
pub(crate) fn expect_err<T: Debug>(result: DurableResult<T>, includes: &str) {
    match result {
        Ok(value) => panic!("expected an error including {includes:?}, got {value:?}"),
        Err(error) => {
            let message = error.to_string();
            assert!(message.contains(includes), "expected an error including {includes:?}, got {message:?}");
        }
    }
}

pub(crate) fn ids<T>(items: &[T], id: impl Fn(&T) -> u64) -> Vec<u64> {
    items.iter().map(id).collect()
}
