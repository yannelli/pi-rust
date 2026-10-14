//! Examples 09 and 13 to 16: context views, task recovery, a faux chat, system prompts, and a real model.

use std::sync::Mutex;

use pi_ai::{ContentBlock, Message, StopReason, UserContent, UserMessage};
use pi_durable::entries::{ASSISTANT_ENTRY, SYSTEM_ENTRY};
use pi_durable::env::{LocalEnvOptions, LocalExecutionEnv, SharedEnv};
use pi_durable::models::create_models;
use pi_durable::storage::sqlite::open_sqlite_storage;
use pi_durable::tasks::TaskOptions;
use pi_durable::testing::faux::{FauxMessageOptions, faux_assistant_message, faux_text, faux_tool_call_with_id};
use support::chat::answer;

use super::*;

fn user(text: &str, timestamp: i64) -> Message {
    Message::User(UserMessage { content: UserContent::Text(text.into()), timestamp })
}

fn assistant(text: &str, calls: &[&str], stop: Option<StopReason>) -> Message {
    let mut blocks = vec![faux_text(text)];
    blocks.extend(calls.iter().map(|id| faux_tool_call_with_id("read", json!({}), *id)));
    let stop = stop.unwrap_or(if calls.is_empty() { StopReason::Stop } else { StopReason::ToolUse });
    Message::Assistant(faux_assistant_message(blocks, FauxMessageOptions::stop_reason(stop)))
}

fn tool_result(id: &str) -> Message {
    let text = format!("file {id}");
    let result = json!({
        "role": "toolResult", "toolCallId": id, "toolName": "read", "content": [{ "type": "text", "text": text }],
        "isError": false, "timestamp": 3,
    });
    serde_json::from_value(result).unwrap()
}

fn show(message: &Message) -> String {
    match message {
        Message::User(user) => format!(
            "user: {}",
            match &user.content {
                UserContent::Text(text) => text.clone(),
                UserContent::Blocks(_) => String::new(),
            }
        ),
        Message::System(system) => format!("system: {}", serde_json::to_string(&system.sections).unwrap()),
        Message::Assistant(assistant) => {
            let parts: Vec<String> = assistant
                .content
                .iter()
                .map(|part| match part {
                    ContentBlock::Text { text, .. } => text.clone(),
                    ContentBlock::ToolCall(call) => format!("call({})", call.id),
                    _ => String::new(),
                })
                .collect();
            format!("assistant: {}", parts.join(" "))
        }
        Message::ToolResult(result) => {
            format!("result({}){}", result.tool_call_id, if result.is_error { " error" } else { "" })
        }
        _ => "other".into(),
    }
}

async fn say(transcript: &Conversation, draft: EntryDraft) -> EntryRecord {
    let id = transcript.id;
    transcript.commit(move |tx: Tx| async move { tx.append_entry(id, draft).await }, &ctx()).await.unwrap()
}

fn model(kind: &str, messages: Vec<Message>) -> EntryDraft {
    EntryDraft { model: Some(messages), ..EntryDraft::new(kind) }
}

#[tokio::test]
async fn ex09_context_derives_request_messages_from_the_raw_transcript() {
    let options = HarnessOptions::new(Arc::new(create_models()), Arc::new(create_registry()));
    let harness = Harness::open(memory(), options, &ctx()).await.unwrap();
    let transcript = harness.create_conversation(CreateOptions::default(), &ctx()).await.unwrap();
    let question = say(&transcript, model("message", vec![user("read a and b", 1)])).await;
    say(&transcript, model("message", vec![assistant("I crashed", &[], Some(StopReason::Aborted))])).await;
    let calls = say(&transcript, model("message", vec![assistant("reading", &["a", "b"], None)])).await;
    say(&transcript, model("message", vec![tool_result("b")])).await; // Results finish out of order.
    let sections = serde_json::from_value(json!({ "cwd": "<cwd>/repo</cwd>" })).unwrap();
    let system = pi_ai::SystemMessage {
        content: UserContent::Text(String::new()),
        sections: Some(sections),
        tools_added: None,
        tools_removed: None,
        timestamp: 4,
    };
    say(&transcript, model("pi.system", vec![Message::System(system)])).await;
    say(&transcript, model("message", vec![tool_result("a")])).await;
    say(&transcript, model("message", vec![assistant("a and b look fine", &[], None)])).await;
    let replace = ContextEdit {
        target: question.id,
        action: ContextEditAction::Replace,
        messages: Some(vec![user("read files a and b", 1)]),
    };
    let edit = EntryDraft {
        data: Some(Json::from(json!("user fixed a typo"))),
        edits: Some(vec![replace]),
        ..EntryDraft::new("edit")
    };
    say(&transcript, edit).await;
    say(&transcript, EntryDraft { data: Some(Json::from(json!("display only"))), ..EntryDraft::new("note") }).await;

    let view = transcript.context(&ctx(), None).await.unwrap();
    let kinds: Vec<&str> = view.entries.iter().map(|entry| entry.kind.as_str()).collect();
    let raw = ["message", "message", "message", "message", "pi.system", "message", "message", "edit", "note"];
    assert_eq!(kinds, raw);
    let request = [
        "user: read files a and b",
        "assistant: reading call(a) call(b)",
        "result(a)",
        "result(b)",
        r#"system: {"cwd":"<cwd>/repo</cwd>"}"#,
        "assistant: a and b look fine",
    ];
    assert_eq!(view.messages.iter().map(show).collect::<Vec<_>>(), request);
    // A fork at the calls ends before the later edit, and gets error results for the calls it cut off.
    let cut = transcript.fork(calls.id, CreateOptions::default(), &ctx()).await.unwrap();
    let forked: Vec<String> = cut.context(&ctx(), None).await.unwrap().messages.iter().map(show).collect();
    assert_eq!(
        forked,
        ["user: read a and b", "assistant: reading call(a) call(b)", "result(a) error", "result(b) error"]
    );

    let summary = vec![user("Summary: a and b are fine.", 5)];
    say(&transcript, EntryDraft { head: Some(DraftHead::SelfEntry), ..model("summary", summary) }).await;
    let view = transcript.context(&ctx(), None).await.unwrap();
    let head = view.head.as_ref().map(|head| head.kind.as_str());
    assert_eq!(
        (head, view.messages.iter().map(show).collect::<Vec<_>>()),
        (Some("summary"), vec!["user: Summary: a and b are fine.".to_owned()])
    );
    let history = transcript.entries(EntryRange::default(), 3, None, &ctx()).await.unwrap();
    let newest: Vec<&str> = history.items.iter().map(|entry| entry.kind.as_str()).collect();
    assert_eq!((newest, history.next.is_some()), (vec!["summary", "note", "edit"], true));
    harness.close(&ctx()).await.unwrap();
}

/// Ticks counted by the phase, and a hook told each tick it reaches.
type Reached = Arc<Mutex<Option<tokio::sync::oneshot::Sender<()>>>>;

fn ticker(printed: Arc<Mutex<Vec<u64>>>, reached: Reached) -> Task {
    define_task(
        TaskDefinition::new("example.ticker", 1, |_| Ok(Json::from(json!({ "phase": "tick", "n": 1 }))))
            .phase("tick", move |task, runtime, context| {
                let (printed, reached) = (printed.clone(), reached.clone());
                async move {
                    let n = task.state.checkpoint().and_then(|c| c.get("n")?.as_u64()).unwrap();
                    let name = format!("printed-{n}");
                    if runtime.memo(&name).await?.is_none() {
                        runtime.memo_or(&name, Json::from(true), &context).await?;
                        printed.lock().unwrap().push(n);
                    }
                    if n == 2
                        && let Some(sender) = reached.lock().unwrap().take()
                    {
                        let _ = sender.send(());
                    }
                    let to = task.input.get("to").and_then(Json::as_u64).unwrap();
                    let next = if n == to {
                        support::task_support::completed(json!(format!("counted to {n}")))
                    } else {
                        support::task_support::running(json!({ "phase": "tick", "n": n + 1 }))
                    };
                    runtime.commit(move |_, _| async move { Ok(next) }, &context).await?;
                    runtime.sleep(runtime.now() + 50.0, &context).await
                }
            })
            .abort(|_, runtime, context| async move {
                let aborted =
                    Some(TaskState::Terminal { outcome: TaskOutcome::Aborted { reason: None, result: None } });
                runtime.commit(move |_, _| async move { Ok(aborted) }, &context).await
            }),
    )
}

#[tokio::test]
async fn ex13_recovery_continues_a_ticker_from_its_saved_checkpoint_after_reopen() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.sqlite").to_string_lossy().into_owned();
    let (printed, reached): (Arc<Mutex<Vec<u64>>>, Reached) = Default::default();
    let registry = Arc::new(create_registry());
    registry.install(Extension::new("ticker").task(ticker(printed.clone(), reached.clone()))).unwrap();
    let open = || async {
        let options = HarnessOptions::new(Arc::new(create_models()), registry.clone() as Arc<dyn RegistryReader>);
        Harness::open(Arc::new(open_sqlite_storage(&path).await.unwrap()), options, &ctx()).await.unwrap()
    };
    let first_run = open().await;
    let root = first_run.root(&ctx(), CreateOptions::default()).await.unwrap();
    let (task, id) = (registry.snapshot().task("example.ticker").unwrap(), root.id);
    let create = move |tx: Tx| async move {
        tx.create_task(task, Json::from(json!({ "to": 5 })), TaskOptions::conversation(id)).await
    };
    let ticker_id = root.commit(create, &ctx()).await.unwrap();
    let (sender, tick_two) = tokio::sync::oneshot::channel();
    *reached.lock().unwrap() = Some(sender);
    first_run.resume().unwrap();
    tick_two.await.unwrap();
    first_run.close(&ctx()).await.unwrap();

    let reader = open().await;
    let saved = reader.get_task(ticker_id, &ctx()).await.unwrap().unwrap();
    reader.close(&ctx()).await.unwrap();
    assert!(matches!(&saved.state, TaskState::Running { .. } | TaskState::Pending { .. }));
    let n = saved.state.checkpoint().and_then(|c| c.get("n")?.as_u64()).unwrap();
    assert!((2..=3).contains(&n), "{n}");
    let memos: Vec<String> = saved.memos.unwrap_or_default().keys().map(|key| key.to_string()).collect();
    assert!(memos.contains(&"printed-2".to_owned()), "{memos:?}");

    let second_run = open().await;
    let counted = second_run.wait_for_task(ticker_id, &ctx()).await.unwrap();
    let outcome = serde_json::to_value(&counted.state).unwrap()["outcome"].clone();
    assert_eq!(outcome, json!({ "status": "completed", "result": "counted to 5" }));
    // Memos keep each tick printed once across the restart.
    assert_eq!(*printed.lock().unwrap(), [1, 2, 3, 4, 5]);
    second_run.close(&ctx()).await.unwrap();
}

fn text_section(key: &str, text: &str, tag: Option<bool>) -> Arc<PromptSection> {
    let text = text.to_owned();
    section(key, move |_, _| std::future::ready(Ok(Some(text.clone()))), tag)
}

#[tokio::test]
async fn ex14_chat_answers_through_the_faux_provider() {
    let setup = chat_setup(Default::default());
    setup.faux.set_responses([answer("Paris.")]);
    let terse = text_section("preamble", "You answer in one word.", Some(false));
    setup.registry.install(Extension::new("terse").section(terse)).unwrap();
    let (harness, root) = open_chat(memory(), &setup).await;
    let capital = root.submit(SubmissionDraft::input("Capital of France?"), &ctx()).await.unwrap();
    let answered = capital.wait(&ctx()).await.unwrap();
    let answer_id = answered.answer.unwrap();
    let entry = root.commit(move |tx: Tx| async move { tx.entry(answer_id).await }, &ctx()).await.unwrap();
    assert!(ASSISTANT_ENTRY.is(entry.as_ref()));
    let reply = entry.and_then(|entry| entry.model).unwrap_or_default();
    assert_eq!(text_of(reply.first()).as_deref(), Some("Paris."));
    let kinds = kinds(&all_entries(&root).await);
    // The input lands first; the system entry is written when the run prepares its request.
    assert_eq!(kinds, ["pi.user", "pi.system", "pi.assistant"]);
    harness.close(&ctx()).await.unwrap();
}

async fn system_entries(conversation: &Conversation) -> Vec<Value> {
    let entries = all_entries(conversation).await.into_iter().filter(|entry| SYSTEM_ENTRY.is(Some(entry)));
    let messages = entries.flat_map(|entry| entry.model.unwrap_or_default());
    messages.map(|message| serde_json::to_value(message).unwrap()).collect()
}

fn sections_of(messages: &[Value]) -> Vec<(Value, Value)> {
    messages.iter().map(|message| (message["content"].clone(), message["sections"].clone())).collect()
}

#[tokio::test]
async fn ex15_system_prompt_composes_wrapped_sections_cwd_and_instructions() {
    let setup = chat_setup(Default::default());
    setup.faux.set_responses([answer("Done."), answer("Done."), answer("Done.")]);
    let cwd =
        section("cwd", |input: PromptInput, _| std::future::ready(Ok(input.env.map(|env| env.cwd().to_owned()))), None);
    let coding = Extension::new("coding").section(text_section("preamble", "You are a coding agent.", Some(false)));
    setup.registry.install(coding.section(cwd)).unwrap();
    let agents_md =
        Extension::new("agents-md").section(text_section("agents_md", "Run npm run check after changes.", None));
    setup.registry.install(agents_md).unwrap();
    let terse = Extension::new("terse").wrap(wrap_section("preamble", |preamble| {
        let render = preamble.render.clone();
        let wrapped = move |input, context| {
            let rendered = render(input, context);
            async move { Ok(rendered.await?.map(|text| format!("{text} Be terse."))) }
        };
        Ok(section(preamble.key.clone(), wrapped, preamble.tag))
    }));
    setup.registry.install(terse).unwrap();
    let options = setup.options().with_env(|target, _| {
        let cwd = target.cwd.unwrap_or_else(|| "/".into());
        let env: SharedEnv = Arc::new(LocalExecutionEnv::new(LocalEnvOptions::new(&cwd)));
        Box::pin(async move { Ok(Some(env)) })
    });
    let (harness, root) = open_chat_with(memory(), &setup, options).await;
    root.configure(AgentChange { cwd: Change::Set("/repo".into()), ..Default::default() }, &ctx()).await.unwrap();
    let subagent = AgentChange {
        cwd: Change::Set("/repo".into()),
        extensions: Change::Set(ExtensionSelection::Edit { add: None, remove: Some(vec!["agents-md".into()]) }),
        instructions: Change::Set("Only read; never edit files.".into()),
        ..AgentChange::model(faux_model())
    };
    let subagent = harness.create_conversation(CreateOptions::default().with_agent(subagent), &ctx()).await.unwrap();
    root.submit(SubmissionDraft::input("Fix the build."), &ctx()).await.unwrap().wait(&ctx()).await.unwrap();
    subagent.submit(SubmissionDraft::input("Read the logs."), &ctx()).await.unwrap().wait(&ctx()).await.unwrap();
    let preamble = "You are a coding agent. Be terse.";
    let root_prompt = json!({ "preamble": preamble, "cwd": "<cwd>\n/repo\n</cwd>", "agents_md": "<agents_md>\nRun npm run check after changes.\n</agents_md>" });
    assert_eq!(sections_of(&system_entries(&root).await), [(json!(""), root_prompt)]);
    let instructions = "<instructions>\nOnly read; never edit files.\n</instructions>";
    let sub_prompt = json!({ "preamble": preamble, "cwd": "<cwd>\n/repo\n</cwd>", "instructions": instructions });
    assert_eq!(sections_of(&system_entries(&subagent).await), [(json!(""), sub_prompt)]);

    // A cwd change appends a system entry that patches only that section.
    root.configure(AgentChange { cwd: Change::Set("/repo/packages".into()), ..Default::default() }, &ctx())
        .await
        .unwrap();
    root.submit(SubmissionDraft::input("Now the package."), &ctx()).await.unwrap().wait(&ctx()).await.unwrap();
    let after = sections_of(&system_entries(&root).await);
    assert_eq!(after.len(), 2);
    assert_eq!(after[1].1, json!({ "cwd": "<cwd>\n/repo/packages\n</cwd>" }));
    harness.close(&ctx()).await.unwrap();
}

// Needs a network provider and an OpenAI key; upstream skips it without `OPENAI_API_KEY` too.
#[tokio::test]
#[ignore = "needs a network model provider"]
async fn ex16_real_model_streams_an_answer() {
    if std::env::var("OPENAI_API_KEY").is_err() {
        return;
    }
    let registry = Arc::new(create_registry());
    registry
        .install(Extension::new("concise").section(text_section(
            "preamble",
            "You are a concise assistant.",
            Some(false),
        )))
        .unwrap();
    let options = HarnessOptions::new(Arc::new(create_models()), registry as Arc<dyn RegistryReader>);
    let harness = Harness::open(memory(), options, &ctx()).await.unwrap();
    let model = ModelRef { provider: "openai".into(), model_id: "gpt-6-sol".into() };
    let root = harness.root(&ctx(), CreateOptions::default().with_agent(AgentChange::model(model))).await.unwrap();
    let poem = root.submit(SubmissionDraft::input("Write a long poem"), &ctx()).await.unwrap();
    assert_eq!(poem.wait(&ctx()).await.unwrap().status, SubmissionStatus::Done);
    harness.close(&ctx()).await.unwrap();
}
