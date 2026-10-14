//! Port of upstream `test/harness-compaction.test.ts`: range selection, serialization, and the first manual case.
//! The modules under `harness_compaction/` hold the rest, sharing `chat.rs`.

#[path = "harness_compaction/chat.rs"]
mod chat;
#[path = "harness_compaction/manual.rs"]
mod manual;
#[path = "harness_compaction/outcomes.rs"]
mod outcomes;
mod support;

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use pi_ai::{
    AssistantMessage, ContentBlock, Message, StopReason, SystemMessage, ToolResultMessage, UserContent, UserMessage,
};
use pi_chord::Json;
use pi_durable::harness::compaction::{select_cut, serialize_conversation};
use pi_durable::harness::context::order_tool_results;
use pi_durable::harness::usage::{PROVIDER_DOC, USAGE_DOC};
use pi_durable::harness::*;
use pi_durable::models::SimpleStreamOptions;
use pi_durable::storage::MemoryStorage;
use pi_durable::testing::faux::{
    FauxMessageOptions, FauxModelDefinition, FauxResponseStep, RegisterFauxProviderOptions, faux_assistant_message,
    faux_tool_call_with_id,
};
use pi_durable::{ConversationId, EntryId, EntryRecord, SubmissionStatus, TaskOutcome};
use serde_json::json;
use support::harness_support::*;

/// Text of about `tokens` estimated tokens, starting with `label`.
fn text(label: &str, tokens: usize) -> String {
    format!("{label} {}", "x".repeat((tokens * 4).saturating_sub(label.len() + 1)))
}

fn user(content: &str) -> Message {
    Message::User(UserMessage { content: UserContent::Text(content.into()), timestamp: 0 })
}

fn assistant(content: &str, calls: &[&str], stop_reason: StopReason) -> Message {
    let mut blocks = vec![ContentBlock::text(content)];
    blocks.extend(calls.iter().map(|id| faux_tool_call_with_id("read", json!({}), *id)));
    Message::Assistant(faux_assistant_message(blocks, FauxMessageOptions::stop_reason(stop_reason)))
}

fn answer_text(content: &str) -> Message {
    assistant(content, &[], StopReason::Stop)
}

fn tool_result(call_id: &str, content: &str) -> Message {
    Message::ToolResult(ToolResultMessage {
        tool_call_id: call_id.into(),
        tool_name: "read".into(),
        content: vec![ContentBlock::text(content)],
        is_error: false,
        timestamp: 0,
        ..ToolResultMessage::default()
    })
}

struct Entries(u64);

impl Entries {
    fn entry(&mut self, kind: &str, model: Vec<Message>, head: Option<u64>) -> EntryRecord {
        self.0 += 1;
        EntryRecord {
            kind: kind.into(),
            model: Some(model),
            data: None,
            edits: None,
            id: EntryId(self.0),
            conversation_id: ConversationId(1),
            head: head.map(EntryId),
            by_task_id: None,
        }
    }
}

/// A view over `entries` whose contributions are their models, with excluded assistants removed.
fn view(entries: &[EntryRecord], head: Option<&EntryRecord>) -> ContextView {
    let all: Vec<EntryRecord> = head.into_iter().chain(entries).cloned().collect();
    let contributions: Vec<Vec<Message>> = all
        .iter()
        .map(|record| {
            let model = record.model.clone().unwrap_or_default();
            model
                .into_iter()
                .filter(|message| match message {
                    Message::Assistant(assistant) => {
                        !matches!(assistant.stop_reason, StopReason::Error | StopReason::Aborted | StopReason::Deferred)
                    }
                    _ => true,
                })
                .collect()
        })
        .collect();
    let messages = order_tool_results(&contributions.concat());
    ContextView { head: head.cloned(), entries: all, contributions, messages }
}

#[test]
fn keeps_about_keep_recent_tokens_and_cuts_at_the_first_candidate_at_or_after_the_budget() {
    let mut ids = Entries(0);
    let entries = [
        ids.entry("pi.user", vec![user(&text("1", 10))], None),
        ids.entry("pi.assistant", vec![assistant("2", &["c1"], StopReason::ToolUse)], None),
        ids.entry("pi.tool-result", vec![tool_result("c1", &text("3", 3000))], None),
        ids.entry("pi.assistant", vec![answer_text(&text("4", 10))], None),
        ids.entry("pi.user", vec![user(&text("5", 10))], None),
        ids.entry("pi.assistant", vec![answer_text(&text("6", 10))], None),
    ];
    assert_eq!(select_cut(&view(&entries, None), 2000), Some(3));
}

#[test]
fn cuts_at_a_user_entry() {
    let mut ids = Entries(0);
    let entries = [
        ids.entry("pi.user", vec![user(&text("u1", 100))], None),
        ids.entry("pi.assistant", vec![answer_text(&text("a1", 100))], None),
        ids.entry("pi.user", vec![user(&text("u2", 100))], None),
        ids.entry("pi.assistant", vec![answer_text(&text("a2", 100))], None),
    ];
    assert_eq!(select_cut(&view(&entries, None), 150), Some(2));
}

#[test]
fn cuts_at_an_assistant_in_the_middle_of_one_long_run_and_never_at_a_tool_result() {
    let mut ids = Entries(0);
    let mut entries = vec![ids.entry("pi.user", vec![user("do it")], None)];
    for index in 0..5 {
        let id = format!("c{index}");
        entries.push(ids.entry(
            "pi.assistant",
            vec![assistant(&format!("step {index}"), &[&id], StopReason::ToolUse)],
            None,
        ));
        entries.push(ids.entry("pi.tool-result", vec![tool_result(&id, &text(&format!("r{index}"), 100))], None));
    }
    let cut = select_cut(&view(&entries, None), 150).unwrap();
    // The budget is reached at the fourth result; the cut is the last call, whose result it keeps.
    assert_eq!(entries[cut].kind, "pi.assistant");
}

#[test]
fn keeps_a_huge_last_tool_result_together_with_its_assistant() {
    let mut ids = Entries(0);
    let entries = [
        ids.entry("pi.user", vec![user("u")], None),
        ids.entry("pi.assistant", vec![assistant("a", &["c"], StopReason::ToolUse)], None),
        ids.entry("pi.tool-result", vec![tool_result("c", &text("big", 5000))], None),
    ];
    assert_eq!(select_cut(&view(&entries, None), 100), Some(1));
}

#[test]
fn never_cuts_at_a_system_entry_or_an_excluded_error_or_aborted_answer() {
    let mut ids = Entries(0);
    let mut sections = pi_ai::JsonObject::new();
    sections.insert("s".into(), json!(text("s", 100)));
    let system = Message::System(SystemMessage {
        content: UserContent::Text(String::new()),
        sections: Some(sections),
        tools_added: None,
        tools_removed: None,
        timestamp: 0,
    });
    let entries = [
        ids.entry("pi.user", vec![user(&text("u1", 100))], None),
        ids.entry("pi.assistant", vec![answer_text(&text("a1", 100))], None),
        ids.entry("pi.system", vec![system], None),
        ids.entry("pi.assistant", vec![assistant(&text("err", 100), &[], StopReason::Error)], None),
        ids.entry("pi.assistant", vec![assistant(&text("stopped", 100), &[], StopReason::Aborted)], None),
        ids.entry("pi.assistant", vec![answer_text(&text("a2", 100))], None),
    ];
    // The walk reaches 150 at the system entry; the excluded answers after it contribute nothing.
    assert_eq!(select_cut(&view(&entries, None), 150), Some(5));
}

#[test]
fn follows_edited_contributions_an_omitted_entry_adds_nothing_and_is_no_candidate() {
    let mut ids = Entries(0);
    let entries = [
        ids.entry("pi.user", vec![user(&text("u1", 100))], None),
        ids.entry("pi.assistant", vec![answer_text(&text("a1", 100))], None),
        ids.entry("pi.user", vec![user(&text("u2", 100))], None),
        ids.entry("pi.assistant", vec![answer_text(&text("a2", 100))], None),
    ];
    let plain = view(&entries, None);
    assert_eq!(select_cut(&plain, 150), Some(2));
    let contributions: Vec<Vec<Message>> = plain
        .contributions
        .iter()
        .enumerate()
        .map(|(index, m)| if index == 2 { Vec::new() } else { m.clone() })
        .collect();
    let messages = order_tool_results(&contributions.concat());
    let omitted = ContextView { contributions, messages, ..plain };
    assert_eq!(select_cut(&omitted, 150), Some(1));
}

#[test]
fn does_not_cut_at_a_user_entry_that_a_result_of_the_preceding_call_still_follows() {
    let mut ids = Entries(0);
    let entries = [
        ids.entry("pi.user", vec![user(&text("u1", 100))], None),
        ids.entry("pi.assistant", vec![assistant("a", &["c"], StopReason::ToolUse)], None),
        ids.entry("pi.user", vec![user(&text("steer", 100))], None),
        ids.entry("pi.tool-result", vec![tool_result("c", &text("r", 100))], None),
        ids.entry("pi.assistant", vec![answer_text(&text("a2", 100))], None),
    ];
    // The budget is reached at the steer; its result follows it, so the cut moves to the next assistant.
    assert_eq!(select_cut(&view(&entries, None), 250), Some(4));
}

#[test]
fn finds_nothing_when_the_budget_is_never_reached_or_only_the_marker_precedes_the_cut() {
    let mut ids = Entries(0);
    let small =
        [ids.entry("pi.user", vec![user("hi")], None), ids.entry("pi.assistant", vec![answer_text("hello")], None)];
    assert_eq!(select_cut(&view(&small, None), 150), None);
    let marker = ids.entry("pi.compaction", vec![user("summary")], Some(0));
    // The budget is reached at the only entry after the marker, so the marker alone would be summarized.
    let after = [ids.entry("pi.user", vec![user(&text("u", 200))], None)];
    assert_eq!(select_cut(&view(&after, Some(&marker)), 150), None);
}

#[test]
fn summarizes_an_earlier_summary_marker_first() {
    let mut ids = Entries(0);
    let marker = ids.entry("pi.compaction", vec![user("EARLIER")], Some(0));
    let kept = [
        ids.entry("pi.user", vec![user(&text("u1", 100))], None),
        ids.entry("pi.assistant", vec![answer_text(&text("a1", 100))], None),
        ids.entry("pi.user", vec![user(&text("u2", 100))], None),
        ids.entry("pi.assistant", vec![answer_text(&text("a2", 100))], None),
    ];
    let selected = view(&kept, Some(&marker));
    assert_eq!(select_cut(&selected, 150), Some(3));
    assert!(serialize_conversation(&selected.contributions[..3].concat()).starts_with("[User]: EARLIER"));
}

#[test]
fn writes_a_transcript_truncates_tool_results_and_omits_system_messages() {
    let mut sections = pi_ai::JsonObject::new();
    sections.insert("s".into(), json!("hidden"));
    let system = Message::System(SystemMessage {
        content: UserContent::Text(String::new()),
        sections: Some(sections),
        tools_added: None,
        tools_removed: None,
        timestamp: 0,
    });
    let reply = faux_assistant_message(
        vec![
            ContentBlock::Thinking { thinking: "hmm".into(), thinking_signature: None, redacted: false },
            ContentBlock::text("sure"),
            faux_tool_call_with_id("read", json!({ "path": "a.ts" }), "c"),
        ],
        FauxMessageOptions::default(),
    );
    let messages = [system, user("hello"), Message::Assistant(reply), tool_result("c", &"y".repeat(2500))];
    let serialized = serialize_conversation(&messages);
    assert!(!serialized.contains("hidden"));
    assert!(serialized.contains("[User]: hello"));
    assert!(serialized.contains("[Assistant thinking]: hmm"));
    assert!(serialized.contains("[Assistant]: sure"));
    assert!(serialized.contains("[Assistant tool calls]: read(path=\"a.ts\")"), "{serialized}");
    assert!(
        serialized.contains(&format!("[Tool result]: {}\n\n[... 500 more characters truncated]", "y".repeat(2000)))
    );
}

type SummaryRequest = (Vec<Message>, SimpleStreamOptions);

/// Scripted faux model that answers agent requests and summarization requests from separate queues.
#[derive(Clone, Default)]
struct Script {
    agent: Arc<Mutex<VecDeque<AssistantMessage>>>,
    summaries: Arc<Mutex<VecDeque<AssistantMessage>>>,
    agent_requests: Arc<Mutex<Vec<Vec<Message>>>>,
    summary_requests: Arc<Mutex<Vec<SummaryRequest>>>,
}

fn is_summary_request(messages: &[Message]) -> bool {
    let Some(Message::System(system)) = messages.first() else { return false };
    matches!(&system.content, UserContent::Text(text) if text.starts_with("You are a context summarization assistant"))
}

fn script(setup: &ChatSetup) -> Script {
    let script = Script::default();
    let steps = (0..500).map(|_| {
        let script = script.clone();
        FauxResponseStep::try_factory(move |transcript, options, _, _| {
            let messages = transcript.messages.clone();
            let summary = is_summary_request(&messages);
            let step = if summary {
                script.summary_requests.lock().unwrap().push((messages, options.clone()));
                script.summaries.lock().unwrap().pop_front()
            } else {
                script.agent_requests.lock().unwrap().push(messages);
                script.agent.lock().unwrap().pop_front()
            };
            step.ok_or_else(|| format!("No scripted {} response", if summary { "summary" } else { "agent" }))
        })
    });
    setup.faux.set_responses(steps.collect::<Vec<_>>());
    script
}

fn reply(content: &str) -> AssistantMessage {
    faux_assistant_message(content, FauxMessageOptions::default())
}

async fn turn(root: &Conversation, script: &Script, input: &str, answer: &str) {
    script.agent.lock().unwrap().push_back(reply(answer));
    let settled = root.submit(SubmissionDraft::input(input), &ctx()).await.unwrap().wait(&ctx()).await.unwrap();
    assert_eq!(settled.status, SubmissionStatus::Done);
}

fn user_text(message: Option<&Message>) -> String {
    text_of(message).unwrap_or_default()
}

#[tokio::test]
async fn places_the_summary_at_once_when_idle_and_keeps_raw_history() {
    let model = FauxModelDefinition {
        context_window: Some(100_000),
        max_tokens: Some(900),
        ..FauxModelDefinition::new("faux-1")
    };
    let setup = chat_setup(RegisterFauxProviderOptions { models: vec![model], ..Default::default() });
    let script = script(&setup);
    add_section(&setup.registry, "preamble", "You are helpful.", Some(false));
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    {
        let mut settings = setup.settings.lock().unwrap();
        settings.stream = Some(ConversationStreamOptions {
            cache_retention: Some(pi_ai::CacheRetention::None),
            ..Default::default()
        });
        settings.compaction = Some(CompactionOverrides {
            enabled: Some(false),
            reserve_tokens: Some(1000),
            keep_recent_tokens: Some(150),
            background_tokens: Some(0),
        });
    }
    harness.resume().unwrap();
    for (input, answer) in [("u1", "a1"), ("u2", "a2"), ("u3", "a3")] {
        turn(&root, &script, &text(input, 100), &text(answer, 100)).await;
    }
    let before = all_entries(&root).await;
    let usage_before = harness.snapshot(&USAGE_DOC, root.id, &ctx()).await.unwrap().unwrap().to_serde();
    script.summaries.lock().unwrap().push_back(reply("SUMMARY"));

    let id = root.compact(Some("focus on files".into()), &ctx()).await.unwrap();
    let record = harness.wait_for_task(id, &ctx()).await.unwrap();
    let Some(TaskOutcome::Completed { result: Some(result) }) = record.state.outcome() else {
        panic!("expected a completed compaction: {:?} {:?}", record.state, setup.reports());
    };
    let submission_id = result.get("submissionId").unwrap().deserialize_into().unwrap();
    let placed = harness.submission(submission_id, &ctx()).await.unwrap().unwrap().wait(&ctx()).await.unwrap();
    assert_eq!(placed.status, SubmissionStatus::Done);

    // Raw history is unchanged; one summary entry heads the first kept entry, u3.
    let after = all_entries(&root).await;
    assert_eq!(after[..before.len()], before[..]);
    let marker = after.last().unwrap();
    let u3 = before
        .iter()
        .find(|record| user_text(record.model.as_ref().and_then(|m| m.first())).starts_with("u3"))
        .unwrap();
    assert_eq!((marker.kind.as_str(), marker.head), ("pi.compaction", Some(u3.id)));
    assert_eq!(marker.data.as_ref().map(Json::to_serde), Some(json!({ "reason": "manual" })));
    assert_eq!(placed.entry, Some(marker.id));
    let summary = user_text(marker.model.as_ref().and_then(|m| m.first()));
    assert_eq!(
        summary,
        "The conversation history before this point was compacted into the following summary:\n\n<summary>\nSUMMARY\n</summary>"
    );

    // The model context is the summary followed by the kept entries.
    let view = root.context(&ctx(), None).await.unwrap();
    let texts: Vec<String> = view.messages.iter().map(|message| user_text(Some(message))).collect();
    assert_eq!(texts, [summary, text("u3", 100), text("a3", 100)]);

    // The summarizer saw the serialized prefix, the prompt, and the instructions, without caching.
    let (messages, options) = script.summary_requests.lock().unwrap()[0].clone();
    assert_eq!(messages.len(), 2);
    let prompt = user_text(messages.get(1));
    assert!(prompt.starts_with("<conversation>\n[User]: u1 "), "{prompt}");
    assert!(prompt.contains("[Assistant]: a2 ") && !prompt.contains("u3 ") && prompt.contains("## Goal"));
    assert!(prompt.ends_with("\n\nAdditional focus: focus on files"));
    assert_eq!(options.cache_retention, Some(pi_ai::CacheRetention::None));
    assert_eq!(options.max_tokens, Some(800));
    let provider = harness.snapshot(&PROVIDER_DOC, root.id, &ctx()).await.unwrap().unwrap();
    assert_eq!(options.session_id.as_deref(), provider.get("sessionId").and_then(Json::as_str));
    assert!(options.deferred.is_none());

    // The summarizer's spend is in the ledger.
    let usage = harness.snapshot(&USAGE_DOC, root.id, &ctx()).await.unwrap().unwrap().to_serde();
    let input = |usage: &serde_json::Value| usage["models"]["faux/faux-1"]["input"].as_u64().unwrap();
    assert!(input(&usage) > input(&usage_before));
    turn(&root, &script, "next", "done").await;
    let agent = script.agent_requests.lock().unwrap().last().unwrap().clone();
    // The next request: the summary, the kept turn, the new input, then one complete system baseline.
    let roles: Vec<&str> = agent
        .iter()
        .map(|message| match message {
            Message::User(_) => "user",
            Message::Assistant(_) => "assistant",
            Message::System(_) => "system",
            _ => "other",
        })
        .collect();
    assert_eq!(roles, ["user", "user", "assistant", "user", "system"]);
    harness.close(&ctx()).await.unwrap();
}
