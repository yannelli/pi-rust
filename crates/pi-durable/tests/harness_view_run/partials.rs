//! Upstream `harness-view-run.test.ts`: in-flight partials rebuilt from the view's operations, after an overflow
//! replacement, and streamed as appends at their argument paths (#9265).

use pi_ai::StopReason;
use pi_chord::Context;
use pi_durable::DurableResult;
use pi_durable::session::{Observation, Observer, WatchCall, WatchEnd};
use pi_durable::testing::faux::{
    FauxMessageOptions, faux_assistant_message, faux_text, faux_thinking, faux_tool_call_with_id,
};

use super::*;

/// Records the value and operations of every change an observer gets.
struct OpsRecorder(Frames);

impl Observer for OpsRecorder {
    fn advance(&self, call: &WatchCall, _context: &Context) -> DurableResult<()> {
        if let WatchCall::Value { value, ops, .. } = call {
            self.0.lock().unwrap().push((value.clone(), ops.clone()));
        }
        Ok(())
    }

    fn end(&self, _end: WatchEnd) {}
}

/// Observe `conversation`'s view synchronously, one frame per commit that changes it.
async fn observe(harness: &Harness, conversation: &Conversation) -> (Observation, Frames) {
    let frames = Frames::default();
    let observer = Arc::new(OpsRecorder(frames.clone()));
    let observation = harness.observe_conversation(conversation.id, observer, &ctx(), None).await.unwrap();
    (observation, frames)
}

#[tokio::test]
async fn rebuilds_every_committed_partial_of_thinking_text_and_tool_call_arguments_from_the_views_operations() {
    let setup = paced(150.0);
    let blocks = vec![
        faux_thinking("thinking about it ".repeat(10)),
        faux_text("some text ".repeat(10)),
        faux_tool_call_with_id("missing", json!({ "path": "a/long/path/".repeat(10), "note": "x".repeat(60) }), "c1"),
    ];
    let message = faux_assistant_message(blocks, FauxMessageOptions::stop_reason(StopReason::ToolUse));
    setup.faux.set_responses([message.into(), answer("done")]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    let partials = partials_of(&harness);
    let (observation, frames) = observe(&harness, &root).await;
    root.submit(SubmissionDraft::input("go"), &ctx()).await.unwrap().wait(&ctx()).await.unwrap();
    observation.detach();
    let frames = frames.lock().unwrap().clone();
    // Replaying the operations alone, from the observation's value, gives every committed partial.
    let mut replayed = observation.value.clone();
    let mut rebuilt: Vec<Value> = Vec::new();
    for (_, ops) in &frames {
        replayed = apply_immutable(Some(&replayed), ops).unwrap();
        let live = live_of(&replayed);
        if let Some(partial) = live["generation"].get("message")
            && rebuilt.last() != Some(partial)
        {
            rebuilt.push(partial.clone());
        }
    }
    assert_eq!(&replayed, &frames.last().unwrap().0);
    let partials = partials.lock().unwrap().clone();
    assert!(partials.len() > 2, "{}", partials.len());
    let contents = |messages: &[Value]| messages.iter().map(|message| message["content"].clone()).collect::<Vec<_>>();
    assert_eq!(contents(&rebuilt), contents(&partials));
    // Growing text and thinking travel as appends.
    let ops: Vec<Value> = frames.iter().flat_map(|(_, ops)| ops_json(ops)).collect();
    let appends = ops.iter().filter(|op| op[0] == "a" && op[1][2] == "generation").count();
    assert!(appends > 2, "{appends}");
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn applies_later_operations_to_an_overflow_replacement_that_holds_an_in_flight_partial() {
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &chat_setup(Default::default())).await;
    let partial = faux_assistant_message(vec![faux_text("hel")], FauxMessageOptions::default());
    let partial = serde_json::to_value(partial).unwrap();
    let window = ConversationWindow { min: Some(1), max: Some(1), from: None };
    let watch = root.watch(&ctx(), Some(window)).await.unwrap();
    change_live(&root, move |live| live["generation"] = json!({ "attempt": 1, "message": partial })).await;
    // Past 10,000 pending operations: an append, a splice, and a `before` per commit.
    for _ in 0..3400 {
        append_note(&root).await;
    }
    let frames = start_recording(&watch);
    let delivered = |count: usize| {
        let frames = frames.clone();
        move || {
            let reached = frames.lock().unwrap().len() >= count;
            async move { reached }
        }
    };
    wait_for(delivered(1)).await;
    let (first, ops) = frames.lock().unwrap()[0].clone();
    assert!(matches!(&ops[..], [Op::Replace(value)] if *value == first), "{:?}", ops_json(&ops));
    change_live(&root, |live| {
        let text = &mut live["generation"]["message"]["content"][0]["text"];
        *text = json!(format!("{}lo", text.as_str().unwrap()));
    })
    .await;
    wait_for(delivered(2)).await;
    let update = frames.lock().unwrap().last().unwrap().1.clone();
    let path = json!(["docs", "pi.live", "generation", "message", "content", 0, "text"]);
    assert_eq!(ops_json(&update), [json!(["a", path, "lo"])]);
    let rebuilt = apply_immutable(Some(&first), &update).unwrap();
    assert_eq!(live_of(&rebuilt)["generation"]["message"]["content"], json!([{ "type": "text", "text": "hello" }]));
    watch.stop().await;
    harness.close(&ctx()).await.unwrap();
}

const MESSAGE: [&str; 4] = ["docs", "pi.live", "generation", "message"];

#[tokio::test]
async fn streams_a_tool_call_partial_as_appends_at_its_argument_paths() {
    let setup = paced(500.0);
    setup.settings.lock().unwrap().progress =
        Some(ProgressOverrides { partial_interval_ms: Some(20), output_interval_ms: Some(100), ..Default::default() });
    let args = json!({ "path": "src/file.ts", "content": "const a = 1;\n".repeat(150) });
    setup.faux.set_responses([calls(&[("missing", args, "c1")]), answer("done")]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    let partials = partials_of(&harness);
    let (observation, frames) = observe(&harness, &root).await;
    root.submit(SubmissionDraft::input("go"), &ctx()).await.unwrap().wait(&ctx()).await.unwrap();
    observation.detach();
    let ops: Vec<Value> = frames.lock().unwrap().iter().flat_map(|(_, ops)| ops_json(ops)).collect();
    // Operations on the in-flight message, with their paths relative to it.
    let mut on_message: Vec<(String, Vec<Value>)> = Vec::new();
    for op in &ops {
        let path = if op[0] == "r" { Vec::new() } else { op[1].as_array().cloned().unwrap_or_default() };
        let at = |(index, segment): (usize, &&str)| path.get(index).and_then(Value::as_str) == Some(*segment);
        if MESSAGE.iter().enumerate().all(at) {
            on_message.push((op[0].as_str().unwrap_or_default().to_owned(), path[MESSAGE.len()..].to_vec()));
        }
    }
    let argument_appends: Vec<&Vec<Value>> = on_message
        .iter()
        .filter(|(kind, path)| kind == "a" && path.get(2).and_then(Value::as_str) == Some("arguments"))
        .map(|(_, path)| path)
        .collect();
    assert!(argument_appends.len() > 10, "{}", argument_appends.len());
    let argument =
        |path: &&Vec<Value>| path.last().and_then(Value::as_str).is_some_and(|key| ["path", "content"].contains(&key));
    assert!(argument_appends.iter().all(argument), "{argument_appends:?}");
    // The call is set whole only when a member appears (at most once per argument); growing strings are appends.
    let block_sets = on_message.iter().filter(|(kind, path)| kind != "a" && path.len() == 2).count();
    assert!(block_sets <= 2, "{block_sets}");
    // The committed partials carry only the parsed arguments, never the raw argument text.
    for partial in partials.lock().unwrap().iter() {
        for block in partial["content"].as_array().unwrap() {
            let mut keys: Vec<&str> = block.as_object().unwrap().keys().map(String::as_str).collect();
            keys.sort();
            assert_eq!(keys, ["arguments", "id", "name", "type"]);
        }
    }
    harness.close(&ctx()).await.unwrap();
}
