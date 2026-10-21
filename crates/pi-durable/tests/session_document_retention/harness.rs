//! The Harness reads the retention from its settings at each use, and runs the same with documents evicted at once.

use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use pi_ai::StopReason;
use pi_durable::documents::*;
use pi_durable::harness::*;
use pi_durable::session::Tx;
use pi_durable::storage::MemoryStorage;
use pi_durable::testing::faux::{
    FauxMessageOptions, FauxResponseStep, faux_assistant_message, faux_text, faux_tool_call_with_id,
};
use serde_json::{Value, json};

use super::support::harness_support::*;
use super::support::task_support::eventually;
use super::{Counter, counter_spec};

/// A conversation document any Harness test can read.
static TEST_DOC: LazyLock<DocToken<Counter, LatestConversation>> = LazyLock::new(|| {
    define_doc(counter_spec("test.retention.harness", LatestConversation(LatestFork::Initial))).unwrap()
});

#[tokio::test]
async fn follows_a_harness_settings_change_at_the_next_use() {
    let setup = chat_setup(Default::default());
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    let id = root.id;
    let edit = move |tx: Tx| async move { tx.doc(&TEST_DOC, id).await?.edit(|counter| counter.count += 1.0) };
    root.commit(edit, &ctx()).await.unwrap();
    let cached = || harness.session().document_cache_sizes().cached;
    let before = cached();
    assert!(before > 0);
    setup.settings.lock().unwrap().document_retention_ms = Some(0.0);
    harness.snapshot(&TEST_DOC, id, &ctx()).await.unwrap();
    eventually(|| async {
        harness.session().read_on_line(async { Ok(()) }).await.unwrap();
        cached() < before
    })
    .await;
    let read = harness.snapshot(&TEST_DOC, id, &ctx()).await.unwrap().unwrap();
    assert_eq!(read["count"], serde_json::Value::from(1.0).into());
    harness.close(&ctx()).await.unwrap();
}

async fn event_sequence(document_retention_ms: f64) -> Vec<String> {
    let setup = chat_setup(Default::default());
    {
        let mut settings = setup.settings.lock().unwrap();
        settings.document_retention_ms = Some(document_retention_ms);
        settings.progress = Some(ProgressOverrides { partial_interval_ms: None, output_interval_ms: Some(0) });
    }
    let steps = define_tool(
        "steps",
        "Streams three steps",
        json!({ "type": "object", "properties": {} }),
        |_, api, _| async move {
            for step in ["a\n", "b\n", "c\n"] {
                api.output(step)?;
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            Ok(ToolExecutionResult::default())
        },
    );
    setup.registry.install(Extension::new("tool:steps").tool(steps)).unwrap();
    let call = faux_tool_call_with_id("steps", json!({}), "s1");
    let first = faux_assistant_message(vec![call], FauxMessageOptions::stop_reason(StopReason::ToolUse));
    let last = faux_assistant_message(vec![faux_text("done")], FauxMessageOptions::default());
    setup.faux.set_responses([FauxResponseStep::from(first), FauxResponseStep::from(last)]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    let stream = watch_events(&harness, root.id, &ctx()).await.unwrap();
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    stream
        .start(move |batch, _| {
            sink.lock().unwrap().extend(batch.iter().map(Value::from));
            async { Ok(()) }
        })
        .unwrap();
    root.submit(SubmissionDraft::input("go"), &ctx()).await.unwrap().wait(&ctx()).await.unwrap();
    harness.wait_for_idle(&ctx()).await.unwrap();
    stream.stop().await;
    harness.close(&ctx()).await.unwrap();
    // Ids and times differ between runs; the sequence of event types and tool outputs must not.
    let events = events.lock().unwrap().clone();
    events
        .iter()
        .map(|event| match event["type"].as_str().unwrap() {
            "tool_execution_update" => format!("update:{}", event["output"]),
            other => other.to_owned(),
        })
        .collect()
}

#[tokio::test]
async fn gives_the_same_events_with_documents_evicted_right_after_use_as_with_documents_kept() {
    assert_eq!(event_sequence(0.0).await, event_sequence(600_000.0).await);
}
