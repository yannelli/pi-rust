//! The remaining upstream `describe("tool results")` cases.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pi_ai::{ContentBlock, Message};
use pi_chord::{Context, Json};
use pi_durable::env::{ShellOutputSkip, ShellOutputWindow};
use pi_durable::harness::generation::{GenerationHooks, generation_task};
use pi_durable::harness::tool::{BeforeToolDecision, ToolHookCall, ToolHooks};
use pi_durable::harness::*;
use pi_durable::session::Tx;
use pi_durable::tasks::TaskOptions;
use pi_durable::*;
use serde_json::{Value, json};

use super::support::harness_support::*;
use super::{Run, add_tool, add_tool_hooks, calls, done, empty_result, result_text, results, run, tool};

fn limits(max_lines: usize, retain: Retain) -> ToolOutputLimits {
    ToolOutputLimits { max_lines: Some(max_lines), retain: Some(retain), ..ToolOutputLimits::default() }
}

#[tokio::test]
async fn offers_the_tail_window_with_the_configured_pace_not_a_head_window_and_accepts_skipped_output() {
    let setup = chat_setup(Default::default());
    setup.settings.lock().unwrap().progress =
        Some(ProgressOverrides { output_interval_ms: Some(250), ..Default::default() });
    let windows = Arc::new(Mutex::new(Vec::new()));
    let sink = windows.clone();
    let tailed = tool("tailed", move |_, api, _| {
        sink.lock().unwrap().push(api.output_window);
        let written = api.output("dropped\n").and_then(|()| {
            let skip = ShellOutputSkip { bytes: 8, newlines: 1, ends_with_newline: true };
            api.output_bytes(b"x\ny\n", Some(skip))
        });
        async move {
            written?;
            Ok(ToolExecutionResult::default())
        }
    });
    add_tool(&setup, tailed.with_output_limits(limits(1, Retain::Tail)));
    let sink = windows.clone();
    let headed = tool("headed", move |_, api, _| {
        sink.lock().unwrap().push(api.output_window);
        async { Ok(ToolExecutionResult::default()) }
    });
    add_tool(&setup, headed.with_output_limits(limits(1, Retain::Head)));
    let round = calls(&[("tailed", json!({}), "c1"), ("headed", json!({}), "c2")]);
    let Run { harness, entries, .. } = run(&setup, vec![round, done()]).await;
    let windows = windows.lock().unwrap().clone();
    let tail = ShellOutputWindow {
        max_bytes: 50 * 1024,
        max_lines: 1,
        min_interval_ms: 250.0,
        bytes_per_second: 100.0 * 1024.0,
    };
    assert!(windows.contains(&Some(tail)), "{windows:?}");
    assert!(windows.contains(&None));
    // 8 bytes written, 8 skipped, then "x\n" dropped by the window: 3 lines, 18 bytes in all.
    let tailed = results(&entries).into_iter().find(|result| result.tool_call_id == "c1").unwrap();
    let expected = "y\n|<harness>\n[warn] Output truncated to its end: 3 lines, 18 bytes dropped\n</harness>";
    assert_eq!(result_text(&tailed), expected);
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn bounds_explicit_text_content_and_keeps_other_content() {
    let setup = chat_setup(Default::default());
    let big = tool("big", |_, _, _| async {
        let image = ContentBlock::Image { data: "AAAA".into(), mime_type: "image/png".into() };
        let output = vec![ContentBlock::text("a\nb\n"), image, ContentBlock::text("c\nd\n")];
        Ok(ToolExecutionResult { output: Some(output), ..ToolExecutionResult::default() })
    });
    add_tool(&setup, big.with_output_limits(limits(2, Retain::Tail)));
    let Run { harness, entries, .. } = run(&setup, vec![calls(&[("big", json!({}), "c1")]), done()]).await;
    let expected = "[image]|c\nd\n|<harness>\n[warn] Output truncated to its end: 2 lines, 4 bytes dropped\n</harness>";
    assert_eq!(result_text(&results(&entries)[0]), expected);
    harness.close(&ctx()).await.unwrap();
}

// Upstream #10549.
#[tokio::test]
async fn records_how_long_execute_took_excluding_hooks_and_nothing_for_calls_that_did_not_run() {
    let setup = chat_setup(Default::default());
    add_tool(
        &setup,
        tool("slow", |_, _, _| async {
            tokio::time::sleep(Duration::from_millis(30)).await;
            Ok(empty_result())
        }),
    );
    add_tool(
        &setup,
        tool("thrower", |_, _, _| async {
            tokio::time::sleep(Duration::from_millis(30)).await;
            Err(DurableError::error("boom"))
        }),
    );
    let hooks = ToolHooks {
        before_tool: Some(Arc::new(|call: ToolHookCall, _, _| {
            Box::pin(async move {
                tokio::time::sleep(Duration::from_millis(100)).await;
                let block = (call.call.id == "blocked").then(|| "no".to_owned());
                Ok(block.map(|block| BeforeToolDecision { block: Some(block), ..Default::default() }))
            })
        })),
        ..ToolHooks::default()
    };
    add_tool_hooks(&setup, "hooks", hooks);
    let round =
        calls(&[("slow", json!({}), "slow"), ("thrower", json!({}), "thrower"), ("slow", json!({}), "blocked")]);
    let Run { harness, entries, .. } = run(&setup, vec![round, done()]).await;
    let by_id: HashMap<String, pi_ai::ToolResultMessage> =
        results(&entries).into_iter().map(|result| (result.tool_call_id.clone(), result)).collect();
    for id in ["slow", "thrower"] {
        let duration = by_id[id].duration_ms.unwrap();
        assert!((25..100).contains(&duration), "{id}: {duration}");
    }
    assert!(by_id["thrower"].is_error);
    assert_eq!(by_id["blocked"].duration_ms, None);
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn repairs_arguments_with_prepare_arguments_before_validation_and_a_throwing_repair_is_invalid() {
    let setup = chat_setup(Default::default());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = seen.clone();
    let mut echo = tool("echo", move |args, _, _| {
        sink.lock().unwrap().push(Value::Object(args));
        async { Ok(empty_result()) }
    });
    echo.prepare_arguments = Some(Arc::new(|args: &Value| match &args["text"] {
        Value::String(text) if text == "throw" => Err(DurableError::error("cannot repair")),
        Value::Number(number) => Ok(json!({ "text": format!("#{number}") })),
        _ => Ok(args.clone()),
    }));
    add_tool(&setup, echo);
    let round = calls(&[("echo", json!({ "text": 7 }), "fixed"), ("echo", json!({ "text": "throw" }), "broken")]);
    let Run { harness, entries, .. } = run(&setup, vec![round, done()]).await;
    let by_id: HashMap<String, String> =
        results(&entries).iter().map(|result| (result.tool_call_id.clone(), result_text(result))).collect();
    assert_eq!(by_id["fixed"], "");
    assert_eq!(by_id["broken"], "<harness>\n[error] cannot repair\n</harness>");
    assert_eq!(*seen.lock().unwrap(), [json!({ "text": "#7" })]);
    // The stored call keeps what the model sent.
    let Some(Message::Assistant(assistant)) = entries[2].model.as_ref().and_then(|model| model.first()).cloned() else {
        panic!("expected an assistant message");
    };
    let call = assistant.content.iter().find_map(|block| match block {
        ContentBlock::ToolCall(call) => Some(call.arguments.clone()),
        _ => None,
    });
    assert_eq!(call.map(Value::Object), Some(json!({ "text": 7 })));
    harness.close(&ctx()).await.unwrap();
}

fn blocking_hook(asked: &Arc<Mutex<Vec<&'static str>>>, name: &'static str) -> ToolHooks {
    let sink = asked.clone();
    ToolHooks {
        before_tool: Some(Arc::new(move |_, _, _| {
            sink.lock().unwrap().push(name);
            let block = Some(format!("{name} says no"));
            Box::pin(async move { Ok(Some(BeforeToolDecision { block, ..Default::default() })) })
        })),
        ..ToolHooks::default()
    }
}

#[tokio::test]
async fn lets_the_first_before_tool_block_win_and_skips_later_handlers() {
    let setup = chat_setup(Default::default());
    add_tool(&setup, tool("echo", |_, _, _| async { Ok(empty_result()) }));
    let asked = Arc::new(Mutex::new(Vec::new()));
    add_tool_hooks(&setup, "hooks:first", blocking_hook(&asked, "first"));
    add_tool_hooks(&setup, "hooks:second", blocking_hook(&asked, "second"));
    let Run { harness, entries, .. } = run(&setup, vec![calls(&[("echo", json!({}), "c1")]), done()]).await;
    assert_eq!(*asked.lock().unwrap(), ["first"]);
    let expected = "<harness>\n[error] Tool call blocked: first says no\n</harness>";
    assert_eq!(result_text(&results(&entries)[0]), expected);
    harness.close(&ctx()).await.unwrap();
}

fn text_of_output(result: &ToolExecutionResult) -> String {
    let output = result.output.clone().unwrap_or_default();
    let texts = output.iter().map(|block| match block {
        ContentBlock::Text { text, .. } => text.clone(),
        _ => String::new(),
    });
    texts.collect::<Vec<_>>().join("|")
}

#[tokio::test]
async fn chains_after_tool_replacements_and_observes_the_round_with_after_tools() {
    let setup = chat_setup(Default::default());
    add_tool(&setup, tool("echo", |_, _, _| async { Ok(ToolExecutionResult::text("raw")) }));
    let first = ToolHooks {
        after_tool: Some(Arc::new(|(_, result): (ToolHookCall, ToolExecutionResult), _, _| {
            Box::pin(async move {
                Ok(Some(ToolExecutionResult { output: Some(vec![ContentBlock::text("first")]), ..result }))
            })
        })),
        ..ToolHooks::default()
    };
    add_tool_hooks(&setup, "hooks:first", first);
    let second = ToolHooks {
        after_tool: Some(Arc::new(|(_, result): (ToolHookCall, ToolExecutionResult), _, _| {
            let details = Some(Json::from(json!({ "replaced": text_of_output(&result) })));
            Box::pin(async move { Ok(Some(ToolExecutionResult { details, ..result })) })
        })),
        ..ToolHooks::default()
    };
    add_tool_hooks(&setup, "hooks:second", second);
    let observed = Arc::new(Mutex::new(Vec::new()));
    let sink = observed.clone();
    let hooks = GenerationHooks {
        after_tools: Some(Arc::new(move |round: (EntryId, Vec<EntryId>), _, _| {
            sink.lock().unwrap().push(round);
            Box::pin(async { Ok(()) })
        })),
        ..GenerationHooks::default()
    };
    setup.registry.install(Extension::new("observer").hook(hook(&generation_task(), hooks))).unwrap();
    let Run { harness, entries, .. } = run(&setup, vec![calls(&[("echo", json!({}), "c1")]), done()]).await;
    let result = &results(&entries)[0];
    assert_eq!(result_text(result), "first");
    assert_eq!(result.details, Some(json!({ "replaced": "first" })));
    let result_entry = entries.iter().find(|entry| entry.kind == "pi.tool-result").unwrap();
    assert_eq!(*observed.lock().unwrap(), [(entries[2].id, vec![result_entry.id])]);
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn runs_the_hooks_of_the_selected_extensions_a_task_owned_child_copies_its_owner_s_selection() {
    let setup = chat_setup(Default::default());
    let called_in = Arc::new(Mutex::new(Vec::new()));
    let echo = Extension::new("echo").tool(tool("echo", |_, _, _| async { Ok(empty_result()) })).into_arc();
    let sink = called_in.clone();
    let audit_hooks = ToolHooks {
        before_tool: Some(Arc::new(move |_, api: HookApi, _: Context| {
            sink.lock().unwrap().push(api.conversation_id());
            Box::pin(async { Ok(None) })
        })),
        ..ToolHooks::default()
    };
    let audit = Extension::new("audit").hook(hook(&pi_durable::harness::tool::tool_task(), audit_hooks));
    setup.registry.install(echo.clone()).unwrap();
    setup.registry.install(audit).unwrap();
    // Audit is installed but not in the default selection.
    setup.settings.lock().unwrap().extensions = Some(vec![echo]);
    let first = run(&setup, vec![done()]).await;
    let add = ExtensionSelection::Edit { add: Some(vec!["audit".into()]), remove: None };
    first.root.configure(AgentChange { extensions: Change::Set(add), ..AgentChange::default() }, &ctx()).await.unwrap();
    // An owner task no registered definition takes stays live and pending.
    let owner = define_task(
        TaskDefinition::new("test.owner", 1, |_| Ok(Json::from(json!({ "phase": "never" }))))
            .phase("never", |_, _, _| async { Ok(()) })
            .abort(|_, _, _| async { Ok(()) }),
    );
    let root_id = first.root.id;
    let stage = move |tx: Tx| async move {
        let task_id = tx.create_task(owner, Json::from(json!({})), TaskOptions::conversation(root_id)).await?;
        Ok(tx.create_conversation(ConversationOwnership::Task { task_id }).await?.id)
    };
    let child_id = first.harness.commit(stage, &ctx()).await.unwrap();
    let child = first.harness.conversation(child_id, &ctx()).await.unwrap().unwrap();
    let options = CreateOptions {
        ownership: ConversationOwnership::Ownerless,
        ..CreateOptions::default().with_agent(AgentChange::model(faux_model()))
    };
    let other = first.harness.create_conversation(options, &ctx()).await.unwrap();
    for conversation in [&first.root, &child, &other] {
        setup.faux.set_responses([calls(&[("echo", json!({}), "c1")]), done()]);
        conversation.submit(SubmissionDraft::input("go"), &ctx()).await.unwrap().wait(&ctx()).await.unwrap();
    }
    assert_eq!(*called_in.lock().unwrap(), [first.root.id, child.id]);
    first.harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn applies_add_tools_and_terminates_only_when_every_result_of_the_round_asks_to() {
    let setup = chat_setup(Default::default());
    let control = |control: ToolControl| ToolExecutionResult {
        output: Some(Vec::new()),
        control: Some(control),
        ..Default::default()
    };
    let stop =
        tool(
            "stop",
            move |_, _, _| async move { Ok(control(ToolControl { terminate: Some(true), ..Default::default() })) },
        );
    let grow = tool("grow", move |_, _, _| async move {
        Ok(control(ToolControl { add_tools: Some(vec!["extra".into(), "stop".into()]), ..Default::default() }))
    });
    add_tool(&setup, stop);
    add_tool(&setup, grow);
    add_tool(&setup, tool("extra", |_, _, _| async { Ok(empty_result()) }));
    setup.faux.set_responses([calls(&[("stop", json!({}), "c1")])]);
    let (harness, root) = open_chat(Arc::new(pi_durable::storage::MemoryStorage::new()), &setup).await;
    let tools = ToolFilter::List(vec!["stop".into(), "grow".into()]);
    root.configure(AgentChange { tools: Change::Set(tools), ..AgentChange::default() }, &ctx()).await.unwrap();
    let settled = root.submit(SubmissionDraft::input("go"), &ctx()).await.unwrap().wait(&ctx()).await.unwrap();
    assert_eq!(settled.status, SubmissionStatus::Done);
    assert_eq!(all_entries(&root).await.last().unwrap().kind, "pi.tool-result");
    let tasks = conversation_tasks(&harness, &root).await;
    assert!(tasks.iter().all(|task| task.state.status() == TaskStatus::Terminal));

    setup.faux.set_responses([calls(&[("stop", json!({}), "c1"), ("grow", json!({}), "c2")]), done()]);
    let second = root.submit(SubmissionDraft::input("again"), &ctx()).await.unwrap().wait(&ctx()).await.unwrap();
    assert_eq!(second.status, SubmissionStatus::Done);
    assert_eq!(all_entries(&root).await.last().unwrap().kind, "pi.assistant");
    // addTools appends to the stored tool array, skipping names it already holds.
    let agent = harness.snapshot(&AGENT_DOC, root.id, &ctx()).await.unwrap().unwrap();
    assert_eq!(agent["tools"].to_serde(), json!(["stop", "grow", "extra"]));
    harness.close(&ctx()).await.unwrap();
}
