//! Upstream `describe("generation hooks")`, `describe("tool execution api")`, and `describe("coding tools")`.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use pi_ai::{ContentBlock, Message, UserContent, UserMessage};
use pi_chord::{Context, Json};
use pi_durable::env::{LocalEnvOptions, LocalExecutionEnv, SharedEnv};
use pi_durable::harness::generation::{GenerationHooks, generation_task};
use pi_durable::harness::live::LIVE_DOC;
use pi_durable::harness::tool::ToolHooks;
use pi_durable::harness::*;
use pi_durable::session::Tx;
use pi_durable::storage::MemoryStorage;
use pi_durable::tasks::TaskOptions;
use pi_durable::testing::faux::{FauxDeferredOptions, FauxResponseStep, RegisterFauxProviderOptions};
use pi_durable::tools::{BashToolOptions, ReadToolOptions, create_bash_tool, create_edit_tool, create_read_tool};
use pi_durable::*;
use serde_json::{Value, json};

use super::support::harness_support::*;
use super::{Run, add_tool, add_tool_hooks, calls, done, empty_result, result_text, results, run, tool};

fn answer(text: &str) -> FauxResponseStep {
    pi_durable::testing::faux::faux_assistant_message(text, Default::default()).into()
}

fn install(setup: &ChatSetup, name: &str, hooks: GenerationHooks) {
    setup.registry.install(Extension::new(name).hook(hook(&generation_task(), hooks))).unwrap();
}

fn role(message: &Message) -> &'static str {
    match message {
        Message::User(_) => "user",
        Message::Assistant(_) => "assistant",
        Message::System(_) => "system",
        Message::ToolResult(_) => "toolResult",
        _ => "other",
    }
}

fn user_texts(entries: &[EntryRecord]) -> Vec<String> {
    let users = entries.iter().filter(|entry| entry.kind == "pi.user");
    users.map(|entry| text_of(entry.model.as_ref().and_then(|model| model.first())).unwrap_or_default()).collect()
}

#[tokio::test]
async fn replaces_request_messages_observes_responses_and_continues_on_yield() {
    let setup = chat_setup(Default::default());
    let requests = Arc::new(Mutex::new(Vec::<Vec<String>>::new()));
    let record = || {
        let sink = requests.clone();
        FauxResponseStep::factory(move |request, _, _, _| {
            let messages = request
                .messages
                .iter()
                .map(|message| format!("{}:{}", role(message), text_of(Some(message)).unwrap_or_default()));
            let count = {
                let mut requests = sink.lock().unwrap();
                requests.push(messages.collect());
                requests.len()
            };
            pi_durable::testing::faux::faux_assistant_message(format!("answer {count}").as_str(), Default::default())
        })
    };
    let inject = GenerationHooks {
        before_request: Some(Arc::new(|mut messages: Vec<Message>, _, _| {
            messages.push(Message::User(UserMessage { content: UserContent::Text("injected".into()), timestamp: 0 }));
            Box::pin(async move { Ok(Some(messages)) })
        })),
        ..GenerationHooks::default()
    };
    install(&setup, "inject", inject);
    let responses = Arc::new(Mutex::new(Vec::new()));
    let sink = responses.clone();
    let observe = GenerationHooks {
        after_response: Some(Arc::new(move |message: pi_ai::AssistantMessage, _, _| {
            sink.lock().unwrap().push(text_of(Some(&Message::Assistant(message))).unwrap_or_default());
            Box::pin(async { Ok(()) })
        })),
        ..GenerationHooks::default()
    };
    install(&setup, "observe", observe);
    let yields = Arc::new(AtomicUsize::new(0));
    let continue_once = GenerationHooks {
        on_yield: Some(Arc::new(move |_, _, _| {
            let first = yields.fetch_add(1, Ordering::SeqCst) == 0;
            let keep = first.then(|| UserContent::Blocks(vec![ContentBlock::text("keep going")]));
            Box::pin(async move { Ok(keep) })
        })),
        ..GenerationHooks::default()
    };
    install(&setup, "continue", continue_once);
    let Run { harness, entries, status, .. } = run(&setup, vec![record(), record()]).await;
    assert_eq!(status, SubmissionStatus::Done);
    let first = requests.lock().unwrap()[0].clone();
    assert_eq!(first[first.len() - 2..], ["user:go", "user:injected"]);
    assert_eq!(*responses.lock().unwrap(), ["answer 1", "answer 2"]);
    assert_eq!(kinds(&entries), ["pi.user", "pi.assistant", "pi.user", "pi.assistant"]);
    // The injected message was used for the request only.
    assert!(!user_texts(&entries).contains(&"injected".to_owned()));
    harness.close(&ctx()).await.unwrap();
}

fn continue_first(text: &'static str) -> GenerationHooks {
    let yields = Arc::new(AtomicUsize::new(0));
    GenerationHooks {
        on_yield: Some(Arc::new(move |_, _, _| {
            let keep = (yields.fetch_add(1, Ordering::SeqCst) == 0).then(|| UserContent::Text(text.into()));
            Box::pin(async move { Ok(keep) })
        })),
        ..GenerationHooks::default()
    }
}

#[tokio::test]
async fn keeps_the_run_s_input_open_across_an_on_yield_continuation_and_answers_it_with_the_final_answer() {
    let setup = chat_setup(Default::default());
    install(&setup, "continue", continue_first("again"));
    let harness_slot = Arc::new(Mutex::new(None::<Harness>));
    let seen = Arc::new(Mutex::new(None));
    let (slot, sink) = (harness_slot.clone(), seen.clone());
    let second = FauxResponseStep::async_factory(move |_, _, _, _| {
        let (harness, sink) = (slot.lock().unwrap().clone().unwrap(), sink.clone());
        Box::pin(async move {
            let live = harness.snapshot(&LIVE_DOC, ROOT_CONVERSATION_ID, &ctx()).await.unwrap().unwrap();
            let input: SubmissionId = live["run"]["inputs"][0].deserialize_into().unwrap();
            let status = harness.submission(input, &ctx()).await.unwrap().unwrap().status(&ctx()).await.unwrap();
            *sink.lock().unwrap() = Some((input, status.status));
            Ok(pi_durable::testing::faux::faux_assistant_message("second", Default::default()))
        })
    });
    setup.faux.set_responses([answer("first"), second]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    *harness_slot.lock().unwrap() = Some(harness.clone());
    root.submit(SubmissionDraft::input("go"), &ctx()).await.unwrap().wait(&ctx()).await.unwrap();
    let (input, status) = seen.lock().unwrap().unwrap();
    assert_eq!(status, SubmissionStatus::Placed);
    let entries = all_entries(&root).await;
    let answers: Vec<&EntryRecord> = entries.iter().filter(|entry| entry.kind == "pi.assistant").collect();
    let record = harness.submission(input, &ctx()).await.unwrap().unwrap().status(&ctx()).await.unwrap();
    assert_eq!((record.status, record.answer), (SubmissionStatus::Done, Some(answers[1].id)));
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn observes_responses_that_arrive_by_polling_a_deferred_request() {
    let deferred = FauxDeferredOptions { pending_fetches: 1, poll_after_ms: Some(1) };
    let setup = chat_setup(RegisterFauxProviderOptions { deferred: Some(deferred), ..Default::default() });
    let observed = Arc::new(Mutex::new(Vec::new()));
    let sink = observed.clone();
    let observe = GenerationHooks {
        after_response: Some(Arc::new(move |message: pi_ai::AssistantMessage, _, _| {
            let reason = serde_json::to_value(message.stop_reason).unwrap();
            let text = text_of(Some(&Message::Assistant(message))).unwrap_or_default();
            sink.lock().unwrap().push(format!("{}:{text}", reason.as_str().unwrap()));
            Box::pin(async { Ok(()) })
        })),
        ..GenerationHooks::default()
    };
    install(&setup, "observe", observe);
    let stream = ConversationStreamOptions {
        deferred: Some(pi_durable::models::DeferredOption::Enabled(true)),
        ..Default::default()
    };
    setup.settings.lock().unwrap().stream = Some(stream);
    let result = run(&setup, vec![answer("late")]).await;
    assert_eq!(result.status, SubmissionStatus::Done);
    // The still-deferred results are not terminal.
    assert_eq!(*observed.lock().unwrap(), ["stop:late"]);
    result.harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn lets_the_first_on_yield_continuation_win_and_reports_throws_without_stopping_later_handlers() {
    let setup = chat_setup(Default::default());
    let called = Arc::new(Mutex::new(Vec::<&str>::new()));
    let sink = called.clone();
    let throwing = GenerationHooks {
        after_response: Some(Arc::new(move |_, _, _| {
            sink.lock().unwrap().push("throwing observer");
            Box::pin(async { Err(DurableError::error("observer failed")) })
        })),
        ..GenerationHooks::default()
    };
    install(&setup, "throwing", throwing);
    let sink = called.clone();
    let next = GenerationHooks {
        after_response: Some(Arc::new(move |_, _, _| {
            sink.lock().unwrap().push("next observer");
            Box::pin(async { Ok(()) })
        })),
        ..GenerationHooks::default()
    };
    install(&setup, "next", next);
    install(&setup, "first", continue_first("first"));
    let sink = called.clone();
    let second = GenerationHooks {
        on_yield: Some(Arc::new(move |_, _, _| {
            let mut called = sink.lock().unwrap();
            called.push("second onYield");
            let count = called.iter().filter(|name| **name == "second onYield").count();
            let keep = (count == 1).then(|| UserContent::Text("second".into()));
            Box::pin(async move { Ok(keep) })
        })),
        ..GenerationHooks::default()
    };
    install(&setup, "second", second);
    let Run { harness, entries, .. } = run(&setup, vec![answer("a"), answer("b"), answer("c")]).await;
    // The first continuation skips the second handler; on the next answer the second handler's continuation wins.
    assert_eq!(user_texts(&entries), ["go", "first", "second"]);
    let called = called.lock().unwrap().clone();
    assert_eq!(called.iter().filter(|name| **name == "second onYield").count(), 2);
    assert_eq!(called.iter().filter(|name| **name == "next observer").count(), 3);
    assert!(setup.reports().contains(&"observer failed".to_owned()));
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn keeps_durable_hook_decisions_in_task_memos() {
    let setup = chat_setup(Default::default());
    add_tool(&setup, tool("echo", |_, _, _| async { Ok(empty_result()) }));
    let asked = Arc::new(AtomicUsize::new(0));
    let count = asked.clone();
    let hooks = ToolHooks {
        before_tool: Some(Arc::new(move |_, api: HookApi, context: Context| {
            count.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                let decision = api.memo_or("approval:decision", Json::from("approved"), &context).await?;
                assert_eq!(api.memo_or("approval:decision", Json::from("denied"), &context).await?, decision);
                Ok(None)
            })
        })),
        ..ToolHooks::default()
    };
    add_tool_hooks(&setup, "approval", hooks);
    let Run { harness, .. } = run(&setup, vec![calls(&[("echo", json!({}), "c1")]), done()]).await;
    assert_eq!(asked.load(Ordering::SeqCst), 1);
    harness.close(&ctx()).await.unwrap();
}

fn local_env(cwd: &str) -> SharedEnv {
    Arc::new(LocalExecutionEnv::new(LocalEnvOptions::new(cwd.to_owned())))
}

#[tokio::test]
async fn builds_the_environment_per_call_from_the_conversation_s_cwd_and_runs_commits_memos_and_child_tasks() {
    let setup = chat_setup(Default::default());
    let child = define_task(
        TaskDefinition::new("test.child", 1, |_| Ok(Json::from(json!({ "phase": "run" }))))
            .phase("run", |task, runtime, context| async move {
                let doubled = task.input["n"].as_u64().unwrap() * 2;
                let state =
                    TaskState::Terminal { outcome: TaskOutcome::Completed { result: Some(Json::from(doubled)) } };
                runtime.commit(move |_, _| async move { Ok(Some(state)) }, &context).await
            })
            .abort(|_, _, _| async { Ok(()) }),
    );
    setup.registry.install(Extension::new("task:test.child").task(child.clone())).unwrap();
    let seen = Arc::new(Mutex::new(Vec::<Value>::new()));
    let sink = seen.clone();
    let probe = tool("probe", move |_, api, context| {
        let (sink, child) = (sink.clone(), child.clone());
        async move {
            let mut seen = vec![json!(api.env.as_ref().map(|env| env.cwd()))];
            let tools: Vec<String> =
                api.agent(&context).await?.tools.iter().map(|tool| tool.name().to_owned()).collect();
            seen.push(json!(tools));
            seen.push(json!(api.registry().extension("tool:probe").is_some()));
            let (conversation_id, call_id) = (api.conversation_id, api.call_id.clone());
            let note = move |tx: Tx| async move {
                // The next call runs in the new directory: its environment is built when it executes.
                configure(
                    &tx,
                    conversation_id,
                    &AgentChange { cwd: Change::Set("/".into()), ..AgentChange::default() },
                )
                .await?;
                let draft = EntryDraft { data: Some(Json::from(call_id)), ..EntryDraft::new("test.note") };
                tx.append_entry(conversation_id, draft).await
            };
            let entry = api.commit(note, &context).await?;
            seen.push(json!(entry.by_task_id == Some(api.task_id)));
            seen.push(api.memo_or("m", Json::from(1), &context).await?.to_serde());
            seen.push(api.memo_or("m", Json::from(2), &context).await?.to_serde());
            let id = api
                .create_task(
                    child,
                    Json::from(json!({ "n": 21 })),
                    TaskOptions::conversation(conversation_id),
                    &context,
                )
                .await?;
            let done = api.wait_for_task(id, &context).await?;
            seen.push(serde_json::to_value(done.state.outcome()).unwrap());
            sink.lock().unwrap().extend(seen);
            Ok(empty_result())
        }
    });
    add_tool(&setup, probe);
    setup.faux.set_responses([calls(&[("probe", json!({}), "c1"), ("probe", json!({}), "c2")]), done()]);
    setup.settings.lock().unwrap().tool_execution = Some(ToolExecutionMode::Sequential);
    let targets = Arc::new(Mutex::new(Vec::new()));
    let sink = targets.clone();
    let options = setup.options().with_env(move |target: EnvTarget, _| {
        sink.lock().unwrap().push((target.conversation_id, target.cwd.clone()));
        let env = local_env(target.cwd.as_deref().unwrap_or("/tmp"));
        Box::pin(async move { Ok(Some(env)) })
    });
    let (harness, root) = open_chat_with(Arc::new(MemoryStorage::new()), &setup, options).await;
    root.configure(AgentChange { cwd: Change::Set("/tmp".into()), ..AgentChange::default() }, &ctx()).await.unwrap();
    root.submit(SubmissionDraft::input("go"), &ctx()).await.unwrap().wait(&ctx()).await.unwrap();
    let outcome = json!({ "status": "completed", "result": 42 });
    let call =
        |cwd: &str| vec![json!(cwd), json!(["probe"]), json!(true), json!(true), json!(1), json!(1), outcome.clone()];
    assert_eq!(*seen.lock().unwrap(), [call("/tmp"), call("/")].concat());
    let targets = targets.lock().unwrap().clone();
    assert!(
        targets.contains(&(root.id, Some("/tmp".into()))) && targets.contains(&(root.id, Some("/".into()))),
        "{targets:?}"
    );
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn answers_a_throwing_environment_with_a_tool_error_result_and_reports_it_once_while_preparing() {
    let setup = chat_setup(Default::default());
    add_tool(&setup, tool("probe", |_, _, _| async { Ok(ToolExecutionResult::text("ran")) }));
    setup.faux.set_responses([calls(&[("probe", json!({}), "c1")]), done()]);
    let options = setup.options().with_env(|_, _| Box::pin(async { Err(DurableError::error("no sandbox")) }));
    let (harness, root) = open_chat_with(Arc::new(MemoryStorage::new()), &setup, options).await;
    root.submit(SubmissionDraft::input("go"), &ctx()).await.unwrap().wait(&ctx()).await.unwrap();
    let result = &results(&all_entries(&root).await)[0];
    assert!(result.is_error);
    assert_eq!(result_text(result), "<harness>\n[error] no sandbox\n</harness>");
    // Each preparation reports the failure and renders without an environment.
    assert_eq!(setup.reports().iter().filter(|report| *report == "no sandbox").count(), 2);
    harness.close(&ctx()).await.unwrap();
}

async fn open_in(setup: &ChatSetup, directory: &tempfile::TempDir) -> (Harness, Conversation) {
    let options = setup.options().with_shared_env(local_env(&directory.path().to_string_lossy()));
    open_chat_with(Arc::new(MemoryStorage::new()), setup, options).await
}

#[tokio::test]
async fn answers_a_failing_command_with_its_retained_tail_and_diagnostics_in_order() {
    let directory = tempfile::tempdir().unwrap();
    let setup = chat_setup(Default::default());
    add_tool(&setup, create_bash_tool(BashToolOptions::default()));
    let command = "i=1; while [ $i -le 3000 ]; do echo line-$i; i=$((i + 1)); done; exit 7";
    setup.faux.set_responses([calls(&[("bash", json!({ "command": command }), "b")]), done()]);
    let (harness, root) = open_in(&setup, &directory).await;
    root.submit(SubmissionDraft::input("go"), &ctx()).await.unwrap().wait(&ctx()).await.unwrap();
    let entries = all_entries(&root).await;
    let entry = entries.iter().find(|entry| entry.kind == "pi.tool-result").unwrap();
    let result = &results(std::slice::from_ref(entry))[0];
    assert!(result.is_error);
    let text = result_text(result);
    assert!(text.starts_with("line-1001\n"), "{text}");
    let codes: Vec<Value> = entry.data.as_ref().unwrap().to_serde()["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .map(|diagnostic| diagnostic["code"].clone())
        .collect();
    assert_eq!(codes, [json!("full_output"), json!("exit_code"), json!("truncated")]);
    assert!(text.contains("line-3000\n|<harness>\n[info] Full output: "), "{text}");
    assert!(
        text.contains("\n[error] Command exited with code 7\n[warn] Output truncated to its end: 1000 lines, "),
        "{text}"
    );
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn reads_edits_and_runs_a_command_in_one_run_then_answers() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("notes.txt"), "hello world\n").unwrap();
    let setup = chat_setup(Default::default());
    let coding = Extension::new("coding")
        .tool(create_read_tool(ReadToolOptions::default()))
        .tool(create_edit_tool())
        .tool(create_bash_tool(BashToolOptions::default()));
    setup.registry.install(coding).unwrap();
    let edits = json!({ "path": "notes.txt", "edits": [{ "oldText": "world", "newText": "durable" }] });
    setup.faux.set_responses([
        calls(&[("read", json!({ "path": "notes.txt" }), "r")]),
        calls(&[("edit", edits, "e")]),
        calls(&[("bash", json!({ "command": "cat notes.txt" }), "b")]),
        done(),
    ]);
    let (harness, root) = open_in(&setup, &directory).await;
    let settled = root.submit(SubmissionDraft::input("go"), &ctx()).await.unwrap().wait(&ctx()).await.unwrap();
    assert_eq!(settled.status, SubmissionStatus::Done);
    let entries = all_entries(&root).await;
    let summary: Vec<(String, bool, String)> = results(&entries)
        .iter()
        .map(|result| (result.tool_name.clone(), result.is_error, result_text(result)))
        .collect();
    let expected = [
        ("read", false, "hello world\n"),
        ("edit", false, "Successfully replaced 1 block(s) in notes.txt."),
        ("bash", false, "hello durable\n"),
    ];
    let expected: Vec<(String, bool, String)> =
        expected.iter().map(|(name, error, text)| (name.to_string(), *error, text.to_string())).collect();
    assert_eq!(summary, expected);
    assert_eq!(entries.last().unwrap().kind, "pi.assistant");
    assert_eq!(std::fs::read_to_string(directory.path().join("notes.txt")).unwrap(), "hello durable\n");
    harness.close(&ctx()).await.unwrap();
}
