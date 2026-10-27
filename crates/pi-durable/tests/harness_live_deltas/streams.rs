//! Partial commits of a streamed answer.

use std::sync::Arc;

use pi_durable::harness::*;
use pi_durable::storage::MemoryStorage;
use pi_durable::testing::faux::{FauxTokenSize, RegisterFauxProviderOptions};
use serde_json::{Value, json};

use super::record_live_ops;
use super::support::chat::answer;
use super::support::harness_support::*;

fn text_path() -> Value {
    json!(["generation", "message", "content", 0, "text"])
}

/// Stream `text` in chunks of 4 tokens (16 characters) and return the ops of the partial commits.
async fn stream(text: &str, tokens_per_second: f64, partial_interval_ms: Option<u64>) -> Vec<Vec<Value>> {
    let token_size = Some(FauxTokenSize { min: Some(4), max: Some(4) });
    let options =
        RegisterFauxProviderOptions { tokens_per_second: Some(tokens_per_second), token_size, ..Default::default() };
    let setup = chat_setup(options);
    let progress = ProgressOverrides { partial_interval_ms, ..Default::default() };
    setup.settings.lock().unwrap().progress = Some(progress);
    setup.faux.set_responses([answer(text)]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    let commits = record_live_ops(&harness);
    root.submit(SubmissionDraft::input("go"), &ctx()).await.unwrap().wait(&ctx()).await.unwrap();
    let commits = commits.lock().unwrap().clone();
    harness.close(&ctx()).await.unwrap();
    commits[2..commits.len() - 1].to_vec()
}

#[tokio::test]
async fn streams_partial_text_as_appends() {
    let partials = stream(&"word ".repeat(250), 400.0, None).await;
    assert!(partials.len() > 2);
    assert_eq!(partials[0].len(), 1);
    assert_eq!((partials[0][0][0].clone(), partials[0][0][1].clone()), (json!("s"), json!(["generation", "message"])));
    assert!(partials[0][0][2].is_object());
    for ops in &partials[1..] {
        assert_eq!(ops.len(), 1, "{ops:?}");
        assert_eq!((ops[0][0].clone(), ops[0][1].clone()), (json!("a"), text_path()));
        assert!(ops[0][2].is_string());
    }
}

#[tokio::test(start_paused = true)]
async fn appends_what_the_copied_partial_grew_by_when_a_delta_arrives_with_the_commit_timer() {
    // Paused time fires the 4 ms commit timer together with each 4 ms chunk (1,000 tokens a second), as coarse
    // Windows timers often do. A delta the loop gets before the commit task runs must not join its append.
    let text = "word ".repeat(1000);
    let partials = stream(&text, 1000.0, Some(4)).await;
    assert!(partials.len() > 100, "{}", partials.len());
    let mut committed = partials[0][0][2]["content"][0]["text"].as_str().unwrap().to_owned();
    for ops in &partials[1..] {
        assert_eq!(ops.len(), 1, "{ops:?}");
        assert_eq!((ops[0][0].clone(), ops[0][1].clone()), (json!("a"), text_path()));
        committed.push_str(ops[0][2].as_str().unwrap());
        assert!(text.starts_with(&committed), "{committed}");
    }
}
