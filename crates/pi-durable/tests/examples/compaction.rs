//! Example 25: background, manual, and overflow compaction in a small context window.

use std::sync::Mutex;

use pi_ai::{AssistantMessage, Message, StopReason};
use pi_durable::entries::COMPACTION_ENTRY;
use pi_durable::harness::live::LIVE_DOC;
use pi_durable::testing::faux::{
    FauxMessageOptions, FauxModelDefinition, FauxResponseStep, RegisterFauxProviderOptions, faux_assistant_message,
};
use tokio::sync::Notify;

use super::*;

#[derive(Default)]
struct Script {
    overflow_once: bool,
    summaries: u32,
    hold: Option<Arc<Notify>>,
    /// Told when the held answer's request arrives.
    reached: Option<Arc<Notify>>,
}

fn text(message: Option<&Message>) -> String {
    text_of(message).unwrap_or_default().replace('\n', " ")
}

fn is_summary_request(messages: &[Message]) -> bool {
    matches!(messages.first(), Some(Message::System(system)) if matches!(&system.content, pi_ai::UserContent::Text(text) if text.contains("summarization")))
}

async fn respond(script: Arc<Mutex<Script>>, messages: Vec<Message>) -> AssistantMessage {
    if is_summary_request(&messages) {
        let summaries = {
            let mut script = script.lock().unwrap();
            script.summaries += 1;
            script.summaries
        };
        return faux_assistant_message(
            format!("## Goal\nPlan a week in Lisbon (summary #{summaries})."),
            FauxMessageOptions::default(),
        );
    }
    let held = {
        let mut script = script.lock().unwrap();
        if std::mem::take(&mut script.overflow_once) {
            let options = FauxMessageOptions {
                stop_reason: Some(StopReason::Error),
                error_message: Some("prompt is too long".into()),
                ..Default::default()
            };
            return faux_assistant_message("", options);
        }
        if let Some(reached) = script.reached.take().filter(|_| script.hold.is_some()) {
            reached.notify_one();
        }
        script.hold.take()
    };
    if let Some(held) = held {
        held.notified().await;
    }
    let question = messages.iter().rev().find(|message| matches!(message, Message::User(_)));
    let answer = format!("A detailed answer to \"{}\": {}", text(question), "details ".repeat(200));
    faux_assistant_message(answer, FauxMessageOptions::default())
}

/// The model context: a header with the message and stored entry counts, the head's reason, and one line per message.
async fn show(conversation: &Conversation) -> (usize, usize, Option<String>, Vec<String>) {
    let view = conversation.context(&ctx(), None).await.unwrap();
    let stored = conversation.entries(EntryRange::default(), 1000, None, &ctx()).await.unwrap().items.len();
    let head = view.head.clone().and_then(|head| COMPACTION_ENTRY.decode(head).ok().flatten());
    let reason = head.map(|head| serde_json::to_value(head.data.reason).unwrap().as_str().unwrap().to_owned());
    let lines = view.messages.iter().map(|message| match message {
        Message::System(_) => "system (system prompt)".to_owned(),
        Message::User(_) => format!("user {}", text(Some(message)).chars().take(70).collect::<String>()),
        _ => format!("assistant {}", text(Some(message)).chars().take(70).collect::<String>()),
    });
    (view.messages.len(), stored, reason, lines.collect())
}

/// Wait for the compactions running in `conversation`.
async fn drain(harness: &Harness, conversation: &Conversation) {
    let live = harness.snapshot(&LIVE_DOC, conversation.id, &ctx()).await.unwrap();
    let running = live.and_then(|live| live.get("compactions").map(Json::to_serde)).unwrap_or_default();
    for compaction in running.as_array().into_iter().flatten() {
        let id = serde_json::from_value(compaction["taskId"].clone()).unwrap();
        harness.wait_for_task(id, &ctx()).await.unwrap();
    }
}

#[tokio::test]
async fn ex25_compaction_summarizes_in_the_background_on_demand_and_after_overflow() {
    let tiny =
        FauxModelDefinition { context_window: Some(3000), max_tokens: Some(1000), ..FauxModelDefinition::new("tiny") };
    let setup = chat_setup(RegisterFauxProviderOptions { models: vec![tiny], ..Default::default() });
    let script = Arc::new(Mutex::new(Script::default()));
    let steps = (0..100).map(|_| {
        let script = script.clone();
        FauxResponseStep::async_factory(move |transcript, _, _, _| {
            let (script, messages) = (script.clone(), transcript.messages.clone());
            Box::pin(async move { Ok(respond(script, messages).await) })
        })
    });
    setup.faux.set_responses(steps.collect::<Vec<_>>());
    let background_tokens = Arc::new(Mutex::new(800));
    let source = background_tokens.clone();
    let mut options = setup.options();
    options.settings = Some(Arc::new(move || {
        let compaction = CompactionOverrides {
            reserve_tokens: Some(1000),
            keep_recent_tokens: Some(400),
            background_tokens: Some(*source.lock().unwrap()),
            ..Default::default()
        };
        Ok(HarnessSettings { compaction: Some(compaction), ..Default::default() })
    }));
    let harness = Harness::open(memory(), options, &ctx()).await.unwrap();
    let model = ModelRef { provider: "faux".into(), model_id: "tiny".into() };
    let root = harness.root(&ctx(), CreateOptions::default().with_agent(AgentChange::model(model))).await.unwrap();
    let ask = |question: &'static str| {
        let (harness, root) = (harness.clone(), root.clone());
        async move {
            let record =
                root.submit(SubmissionDraft::input(question), &ctx()).await.unwrap().wait(&ctx()).await.unwrap();
            drain(&harness, &root).await;
            match record.status {
                SubmissionStatus::Done => Ok(show(&root).await),
                _ => Err((record.reason, record.detail)),
            }
        }
    };
    let mut reasons = Vec::new();
    for question in ["Where should we stay?", "What should we eat?", "Which day trips?", "Any museums?", "Nightlife?"] {
        let (messages, stored, reason, _) = ask(question).await.unwrap();
        assert!(messages <= stored, "{messages} messages from {stored} entries");
        reasons.push(reason);
    }
    // Background compaction starts once the context passes the threshold, and the summary heads the context.
    let threshold = Some("threshold".to_owned());
    assert_eq!(reasons, [None, None, threshold.clone(), threshold.clone(), threshold]);

    // A manual compaction while an answer is held finishes, and its summary waits for the answer.
    let finish = Arc::new(Notify::new());
    let reached = Arc::new(Notify::new());
    {
        let mut held = script.lock().unwrap();
        (held.hold, held.reached) = (Some(finish.clone()), Some(reached.clone()));
    }
    let busy = root.submit(SubmissionDraft::input("How do we get around?"), &ctx()).await.unwrap();
    reached.notified().await;
    let manual = root.compact(Some("Keep the hotel shortlist".into()), &ctx()).await.unwrap();
    let state = serde_json::to_value(harness.wait_for_task(manual, &ctx()).await.unwrap().state).unwrap();
    let placement = serde_json::from_value(state["outcome"]["result"]["submissionId"].clone()).unwrap();
    let summary = harness.submission(placement, &ctx()).await.unwrap().unwrap();
    assert_eq!(summary.status(&ctx()).await.unwrap().status, SubmissionStatus::Queued);
    finish.notify_one();
    busy.wait(&ctx()).await.unwrap();
    assert_eq!(summary.wait(&ctx()).await.unwrap().status, SubmissionStatus::Done);
    drain(&harness, &root).await;
    // A background compaction of the busy turn may land after it, so the manual summary is found among the entries.
    let reasons: Vec<String> = all_entries(&root)
        .await
        .into_iter()
        .filter_map(|entry| COMPACTION_ENTRY.decode(entry).ok().flatten())
        .map(|entry| serde_json::to_value(entry.data.reason).unwrap().as_str().unwrap().to_owned())
        .collect();
    assert!(reasons.contains(&"manual".to_owned()), "{reasons:?}");
    let (_, _, _, lines) = show(&root).await;
    assert!(lines[0].starts_with("user The conversation history before this point was compacted"), "{lines:?}");

    // Without background compaction, then after one overflow: the run compacts and answers again.
    *background_tokens.lock().unwrap() = 0;
    ask("What should we pack?").await.unwrap();
    // The faux usage counts cache writes too, so the next request passes the blocking threshold and compacts first.
    // The overflow that follows comes after this generation's compaction, so it fails the input.
    script.lock().unwrap().overflow_once = true;
    let failed = ask("Summarize the plan for my partner").await.unwrap_err();
    assert_eq!(failed.0.as_deref(), Some("model_error"));
    assert_eq!(failed.1.map(|detail| detail.to_serde()), Some(json!("prompt is too long")));
    let (_, _, reason, lines) = show(&root).await;
    assert_eq!(reason.as_deref(), Some("threshold"));
    // The baseline leads the summary and the kept answer.
    assert_eq!(lines[0], "system (system prompt)");
    assert_eq!(lines[3], "user Summarize the plan for my partner");
    harness.close(&ctx()).await.unwrap();
}
