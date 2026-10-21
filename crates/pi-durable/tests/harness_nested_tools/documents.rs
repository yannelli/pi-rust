//! One document per nested call, written only by that call, and none for a refused call.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use pi_durable::harness::tool::NESTED_CALL_DOC;
use pi_durable::harness::*;
use pi_durable::{StorageWrite, TaskRecord};
use serde_json::json;

use super::support::harness_support::*;
use super::support::{ControlledStorage, Deferred};
use super::{add_tool, args, call, done, echo_tool, tool};

const KIND: &str = "pi.tool.nested-call";

#[tokio::test]
async fn stores_one_document_per_nested_call_keyed_by_its_key_written_only_by_that_call() {
    let setup = chat_setup(Default::default());
    add_tool(&setup, echo_tool(Arc::default()));
    add_tool(
        &setup,
        tool("batch", |_, api, context| async move {
            let named = ExecuteToolOptions { key: Some("named".into()), ..Default::default() };
            let (a, b) = futures::join!(
                api.execute_tool("echo", args(json!({ "text": "a" })), &context, Default::default()),
                api.execute_tool("echo", args(json!({ "text": "b" })), &context, named)
            );
            a?;
            b?;
            Ok(ToolExecutionResult::default())
        }),
    );
    setup.faux.set_responses([call("batch"), done()]);
    let storage = Arc::new(ControlledStorage::default());
    let (harness, root) = open_chat(storage.clone(), &setup).await;
    root.submit(SubmissionDraft::input("go"), &ctx()).await.unwrap().wait(&ctx()).await.unwrap();
    let commits = storage.commits();
    let mut keys: Vec<(String, Option<String>)> = commits
        .iter()
        .flatten()
        .filter_map(|write| match write {
            StorageWrite::DocumentCreate { record, .. } if record.kind.starts_with("pi.tool.") => {
                Some((record.kind.clone(), record.key.clone()))
            }
            _ => None,
        })
        .collect();
    keys.sort();
    assert_eq!(keys, [(KIND.to_owned(), Some("1".to_owned())), (KIND.to_owned(), Some("named".to_owned()))]);
    // No commit creates or changes the documents of two calls.
    let calls: HashSet<_> = commits
        .iter()
        .flatten()
        .filter_map(|write| match write {
            StorageWrite::DocumentCreate { record, .. } if record.kind == KIND => Some(record.id),
            _ => None,
        })
        .collect();
    for writes in &commits {
        let touched: HashSet<_> = writes
            .iter()
            .filter_map(|write| match write {
                StorageWrite::DocumentCreate { record, .. } => Some(record.id),
                StorageWrite::DocumentChange { id, .. } => Some(*id),
                _ => None,
            })
            .filter(|id| calls.contains(id))
            .collect();
        assert!(touched.len() <= 1, "{touched:?}");
    }
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn rejects_a_nested_call_of_an_aborted_caller_and_leaves_no_document_for_it() {
    let setup = chat_setup(Default::default());
    add_tool(&setup, echo_tool(Arc::default()));
    let (started, marked) = (Deferred::default(), Deferred::default());
    let (caller, rejected) = (Arc::new(Mutex::new(None)), Arc::new(Mutex::new(String::new())));
    let (id_sink, error_sink, signal, gate) = (caller.clone(), rejected.clone(), started.clone(), marked.clone());
    add_tool(
        &setup,
        tool("late", move |_, api, _| {
            let (id_sink, error_sink, started, marked) =
                (id_sink.clone(), error_sink.clone(), signal.clone(), gate.clone());
            async move {
                *id_sink.lock().unwrap() = Some(api.task_id);
                started.resolve();
                marked.wait().await;
                // The context is not aborted: the admission itself must refuse.
                let made = api.execute_tool("echo", args(json!({ "text": "a" })), &ctx(), Default::default()).await;
                *error_sink.lock().unwrap() = made.err().map(|error| error.to_string()).unwrap_or_default();
                Ok(ToolExecutionResult::default())
            }
        }),
    );
    setup.faux.set_responses([call("late"), done()]);
    let (harness, root) = open_chat(Arc::new(ControlledStorage::default()), &setup).await;
    let submission = root.submit(SubmissionDraft::input("go"), &ctx()).await.unwrap();
    started.wait().await;
    let id = caller.lock().unwrap().unwrap();
    let mark = move |tx: pi_durable::session::Tx| async move {
        let record = tx.task(id).await?.unwrap();
        tx.set_task(TaskRecord { abort_requested: true, ..record })
    };
    harness.commit(mark, &ctx()).await.unwrap();
    marked.resolve();
    submission.wait(&ctx()).await.unwrap();
    let message = rejected.lock().unwrap().clone();
    assert!(["abort mark", "aborted", "has settled"].iter().any(|part| message.contains(part)), "{message}");
    let found = harness.session().family_snapshot(&NESTED_CALL_DOC, id, "1", &ctx()).await.unwrap();
    assert_eq!(found, None);
    harness.close(&ctx()).await.unwrap();
}
