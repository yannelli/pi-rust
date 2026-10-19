//! The upstream "per-model compaction overrides" cases.

use std::collections::BTreeMap;

use super::chat::*;

fn for_model(key: &str, entry: CompactionOverrides) -> Option<BTreeMap<String, CompactionOverrides>> {
    Some(BTreeMap::from([(key.to_owned(), entry)]))
}

#[tokio::test]
async fn applies_the_current_model_override_to_the_blocking_threshold() {
    for key in ["faux/faux-1", "faux/other"] {
        let chat = open(window(1000)).await;
        chat.history().await;
        // Harness-wide: blocking only above the full window; the override for faux-1 blocks above 700.
        let entry = CompactionOverrides { reserve_tokens: Some(300), ..Default::default() };
        let model_overrides = for_model(key, entry);
        chat.set_policy(CompactionOverrides { reserve_tokens: Some(0), model_overrides, ..blocking() });
        chat.faux.summary(summary());
        chat.turn(&text("u4", 200), "a4").await;
        assert_eq!(chat.faux.summary_requests().len(), usize::from(key == "faux/faux-1"), "{key}");
        chat.close().await;
    }
}

#[tokio::test]
async fn applies_the_request_model_override_on_a_context_overflow() {
    let chat = open(OpenOptions::default()).await;
    chat.history().await;
    let model_overrides = for_model("faux/faux-1", CompactionOverrides { enabled: Some(true), ..Default::default() });
    chat.set_policy(CompactionOverrides { model_overrides, ..manual() });
    chat.faux.summary(summary());
    chat.faux.agent(failure(OVERFLOW));
    chat.faux.agent(answer("fits"));
    let input = chat.submit(&text("u4", 100)).await;
    assert_eq!(input.wait(&ctx()).await.unwrap().status, SubmissionStatus::Done);
    assert_eq!(chat.faux.summary_requests().len(), 1);
    chat.close().await;
}

#[tokio::test]
async fn applies_the_model_override_when_a_compaction_selects_and_sizes_its_summary() {
    let chat = open(OpenOptions::default()).await;
    chat.history().await;
    let entry = CompactionOverrides { keep_recent_tokens: Some(150), reserve_tokens: Some(500), ..Default::default() };
    let model_overrides = for_model("faux/faux-1", entry);
    chat.set_policy(CompactionOverrides { keep_recent_tokens: Some(100_000), model_overrides, ..manual() });
    chat.faux.summary(summary());
    let id = chat.compact(None).await;
    assert_eq!(chat.outcome(id).await["status"], "completed");
    // 0.8 * 500 from the override, below the model's 900.
    assert_eq!(chat.faux.summary_requests()[0].options.max_tokens, Some(400));
    let messages = chat.root.context(&ctx(), None).await.unwrap().messages;
    let kept: Vec<String> = messages[1..].iter().map(|message| user_text(Some(message))).collect();
    assert_eq!(kept, [text("u3", 100), text("a3", 100)]);
    chat.close().await;
}
