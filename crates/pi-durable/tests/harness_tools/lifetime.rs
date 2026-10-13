//! Upstream `describe("tool progress and lifetime")`.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use pi_ai::Message;
use pi_chord::Json;
use pi_durable::harness::generation::{GenerationHooks, generation_task};
use pi_durable::harness::live::LIVE_DOC;
use pi_durable::harness::tool::{ToolHookCall, ToolHooks};
use pi_durable::harness::*;
use pi_durable::storage::MemoryStorage;
use pi_durable::tasks::TaskOptions;
use pi_durable::*;
use serde_json::json;
use tokio::task::JoinHandle;

use super::support::Deferred;
use super::support::harness_support::*;
use super::support::task_support::aborted;
use super::{Run, add_tool, add_tool_hooks, calls, done, empty_result, result_text, results, run, tool};

fn one_call(name: &str) -> Vec<pi_durable::testing::faux::FauxResponseStep> {
    vec![calls(&[(name, json!({}), "c1")]), done()]
}

#[tokio::test]
async fn applies_the_default_output_limits() {
    let setup = chat_setup(Default::default());
    add_tool(
        &setup,
        tool("lines", |_, api, _| {
            let written: DurableResult<()> = (1..=2500).try_for_each(|index| api.output(&format!("{index}\n")));
            async move {
                written?;
                Ok(ToolExecutionResult::default())
            }
        }),
    );
    let Run { harness, entries, .. } = run(&setup, one_call("lines")).await;
    let text = result_text(&results(&entries)[0]);
    assert!(text.starts_with("1\n2\n"));
    let end = "\n2000\n|<harness>\n[warn] Output truncated to its beginning: 500 lines, 2500 bytes dropped\n</harness>";
    assert!(text.ends_with(end), "{}", &text[text.len() - 200..]);
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn sanitizes_running_output_but_keeps_explicit_result_content_as_the_tool_returned_it() {
    let setup = chat_setup(Default::default());
    let slot_output = Arc::new(Mutex::new(None));
    let sink = slot_output.clone();
    add_tool(
        &setup,
        tool("noisy", move |_, api, context| {
            let sink = sink.clone();
            async move {
                api.output("a\u{7}b\r\n")?;
                api.details(Json::from(json!({ "ready": true })), &context).await?;
                let live = api.snapshot(&LIVE_DOC, api.conversation_id, &context).await?.unwrap();
                *sink.lock().unwrap() = live["tools"][0]["output"].as_str().map(str::to_owned);
                Ok(ToolExecutionResult::default())
            }
        }),
    );
    add_tool(&setup, tool("explicit", |_, _, _| async { Ok(ToolExecutionResult::text("c\u{1b}d")) }));
    let round = calls(&[("noisy", json!({}), "c1"), ("explicit", json!({}), "c2")]);
    let Run { harness, entries, .. } = run(&setup, vec![round, done()]).await;
    assert_eq!(slot_output.lock().unwrap().as_deref(), Some("ab\n"));
    let text = |id: &str| result_text(results(&entries).iter().find(|result| result.tool_call_id == id).unwrap());
    assert_eq!((text("c1"), text("c2")), ("ab\n".to_owned(), "c\u{1b}d".to_owned()));
    harness.close(&ctx()).await.unwrap();
}

// Upstream sets `terminate: undefined`; a Rust `ToolControl` has no undefined, so `None` stands in for it.
#[tokio::test]
async fn drops_control_keys_set_to_undefined_instead_of_faulting() {
    let setup = chat_setup(Default::default());
    add_tool(
        &setup,
        tool("grow", |_, _, _| async {
            let control = ToolControl { add_tools: Some(vec!["extra".into()]), terminate: None, handoff: None };
            Ok(ToolExecutionResult { control: Some(control), ..empty_result() })
        }),
    );
    add_tool(&setup, tool("extra", |_, _, _| async { Ok(empty_result()) }));
    setup.faux.set_responses(one_call("grow"));
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    let remove = ToolFilter::Remove { remove: vec!["extra".into()] };
    root.configure(AgentChange { tools: Change::Set(remove), ..AgentChange::default() }, &ctx()).await.unwrap();
    let settled = root.submit(SubmissionDraft::input("go"), &ctx()).await.unwrap().wait(&ctx()).await.unwrap();
    assert_eq!(settled.status, SubmissionStatus::Done);
    // addTools deletes the name from a stored `{ remove }` filter.
    let agent = harness.snapshot(&AGENT_DOC, root.id, &ctx()).await.unwrap().unwrap();
    assert_eq!(agent["tools"].to_serde(), json!({ "remove": [] }));
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn uses_explicit_null_details_instead_of_the_last_reported_value() {
    let setup = chat_setup(Default::default());
    add_tool(
        &setup,
        tool("null", |_, api, context| async move {
            api.details(Json::from(json!({ "old": 1 })), &context).await?;
            Ok(ToolExecutionResult { details: Some(Json::Null), ..empty_result() })
        }),
    );
    let Run { harness, entries, .. } = run(&setup, one_call("null")).await;
    assert_eq!(results(&entries)[0].details, Some(serde_json::Value::Null));
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn settles_details_promises_with_coalesced_progress_commits_and_the_terminal_commit() {
    let setup = chat_setup(Default::default());
    let settled = Arc::new(Mutex::new(Vec::new()));
    let sink = settled.clone();
    add_tool(
        &setup,
        tool("details", move |_, api, context| {
            let sink = sink.clone();
            async move {
                // Three updates in one throttle window coalesce; the last is still pending when execute() returns.
                let update = |n: u64, label: &'static str| {
                    let (api, context, sink) = (api.clone(), context.clone(), sink.clone());
                    tokio::spawn(async move {
                        let result = api.details(Json::from(json!({ "n": n })), &context).await;
                        sink.lock().unwrap().push(label);
                        result
                    })
                };
                let first = update(1, "first");
                tokio::task::yield_now().await;
                let _second = update(2, "second");
                first.await.unwrap()?;
                tokio::task::yield_now().await;
                let _third = update(3, "third");
                tokio::task::yield_now().await;
                Ok(empty_result())
            }
        }),
    );
    let Run { harness, entries, .. } = run(&setup, one_call("details")).await;
    wait_for(|| async { settled.lock().unwrap().len() == 3 }).await;
    assert_eq!(*settled.lock().unwrap(), ["first", "second", "third"]);
    assert_eq!(results(&entries)[0].details, Some(json!({ "n": 3 })));
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn finishes_a_call_under_the_implementation_it_resolved_when_the_tool_is_replaced_mid_call() {
    let setup = chat_setup(Default::default());
    let (started, finish) = (Deferred::default(), Deferred::default());
    let (signal, gate) = (started.clone(), finish.clone());
    let v1 = tool("work", move |_, _, _| {
        let (started, finish) = (signal.clone(), gate.clone());
        async move {
            started.resolve();
            finish.wait().await;
            Ok(ToolExecutionResult::text("v1"))
        }
    });
    add_tool(&setup, v1);
    setup.faux.set_responses(one_call("work"));
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    let submission = root.submit(SubmissionDraft::input("go"), &ctx()).await.unwrap();
    started.wait().await;
    // The same extension name replaces the old one in place.
    add_tool(&setup, tool("work", |_, _, _| async { Ok(ToolExecutionResult::text("v2")) }));
    finish.resolve();
    submission.wait(&ctx()).await.unwrap();
    assert_eq!(result_text(&results(&all_entries(&root).await)[0]), "v1");
    harness.close(&ctx()).await.unwrap();
}

fn prompt(version: &'static str, requests: &Arc<Mutex<Vec<&'static str>>>) -> Extension {
    let sink = requests.clone();
    let hooks = GenerationHooks {
        before_request: Some(Arc::new(move |_, _, _| {
            sink.lock().unwrap().push(version);
            Box::pin(async { Ok(None) })
        })),
        ..GenerationHooks::default()
    };
    Extension::new("prompt").section(static_section("mode", version)).hook(hook(&generation_task(), hooks))
}

#[tokio::test]
async fn uses_a_section_and_hook_extension_reloaded_mid_run_from_the_run_s_next_request() {
    let setup = chat_setup(Default::default());
    let requests = Arc::new(Mutex::new(Vec::new()));
    setup.registry.install(prompt("v1", &requests)).unwrap();
    let (running, reloaded) = (Deferred::default(), Deferred::default());
    let (signal, gate) = (running.clone(), reloaded.clone());
    add_tool(
        &setup,
        tool("work", move |_, _, _| {
            let (running, reloaded) = (signal.clone(), gate.clone());
            async move {
                running.resolve();
                reloaded.wait().await;
                Ok(empty_result())
            }
        }),
    );
    setup.faux.set_responses(one_call("work"));
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    let submission = root.submit(SubmissionDraft::input("go"), &ctx()).await.unwrap();
    running.wait().await;
    setup.registry.install(prompt("v2", &requests)).unwrap();
    reloaded.resolve();
    submission.wait(&ctx()).await.unwrap();
    let sections: Vec<serde_json::Value> = all_entries(&root)
        .await
        .iter()
        .filter_map(|entry| match entry.model.as_ref()?.first()? {
            Message::System(system) => system.sections.as_ref().map(|sections| serde_json::to_value(sections).unwrap()),
            _ => None,
        })
        .collect();
    assert_eq!(sections, [json!({ "mode": "<mode>\nv1\n</mode>" }), json!({ "mode": "<mode>\nv2\n</mode>" })]);
    assert_eq!(*requests.lock().unwrap(), ["v1", "v2"]);
    harness.close(&ctx()).await.unwrap();
}

type Detached = Arc<Mutex<Option<(JoinHandle<DurableResult<TaskRecord>>, JoinHandle<pi_durable::session::WatchEnd>)>>>;

#[tokio::test]
async fn rejects_invocation_bound_waits_and_stops_watches_when_the_tool_s_invocation_ends() {
    let setup = chat_setup(Default::default());
    let never = define_task(
        TaskDefinition::new("test.never", 1, |_| Ok(Json::from(json!({ "phase": "never" }))))
            .phase("never", |_, _, _| async { Ok(()) })
            .abort(|_, _, _| async { Ok(()) }),
    );
    let detached = Detached::default();
    let sink = detached.clone();
    add_tool(
        &setup,
        tool("detach", move |_, api, context| {
            let (sink, never) = (sink.clone(), never.clone());
            async move {
                // The child's definition is not registered, so it stays pending.
                let options = TaskOptions::conversation(api.conversation_id);
                let child = api.create_task(never, Json::from(json!({})), options, &context).await?;
                let (waiter, wait_context) = (api.clone(), context.clone());
                let wait = tokio::spawn(async move { waiter.wait_for_task(child, &wait_context).await });
                let watch = api.watch_doc(&LIVE_DOC, api.conversation_id, &context).await?.unwrap();
                let closed = tokio::spawn(watch.closed());
                *sink.lock().unwrap() = Some((wait, closed));
                Ok(empty_result())
            }
        }),
    );
    let Run { harness, .. } = run(&setup, one_call("detach")).await;
    let (wait, closed) = detached.lock().unwrap().take().unwrap();
    let error = wait.await.unwrap().unwrap_err();
    assert!(error.to_string().contains("invocation has ended"), "{error}");
    closed.await.unwrap();
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn rejects_details_still_waiting_when_the_call_is_aborted_during_after_tool() {
    let setup = chat_setup(Default::default());
    let pending = Arc::new(Mutex::new(None::<JoinHandle<DurableResult<()>>>));
    let sink = pending.clone();
    add_tool(
        &setup,
        tool("slow", move |_, api, context| {
            let sink = sink.clone();
            async move {
                api.output("first\n")?;
                // The output commit is in flight, so these details wait for the next throttle window.
                let (api, context) = (api.clone(), context.clone());
                let details =
                    tokio::spawn(async move { api.details(Json::from(json!({ "step": 1 })), &context).await });
                *sink.lock().unwrap() = Some(details);
                Ok(ToolExecutionResult::default())
            }
        }),
    );
    let in_after_tool = Deferred::default();
    let signal = in_after_tool.clone();
    let hooks = ToolHooks {
        after_tool: Some(Arc::new(move |_: (ToolHookCall, ToolExecutionResult), _, context: pi_chord::Context| {
            let signal = signal.clone();
            Box::pin(async move {
                signal.resolve();
                aborted(context.abort_signal().unwrap().clone()).await;
                Ok(None)
            })
        })),
        ..ToolHooks::default()
    };
    add_tool_hooks(&setup, "hooks", hooks);
    setup.faux.set_responses(one_call("slow"));
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    let submission = root.submit(SubmissionDraft::input("go"), &ctx()).await.unwrap();
    in_after_tool.wait().await;
    let live = harness.snapshot(&LIVE_DOC, root.id, &ctx()).await.unwrap().unwrap();
    let task_id: TaskId = live["tools"][0]["taskId"].deserialize_into().unwrap();
    harness.abort_task(task_id, &ctx()).await.unwrap();
    let details = pending.lock().unwrap().take().unwrap();
    assert!(details.await.unwrap().is_err());
    submission.wait(&ctx()).await.unwrap();
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn answers_an_aborted_tool_with_only_its_durable_output_discarding_buffered_output() {
    let setup = chat_setup(Default::default());
    let buffered = Deferred::default();
    let reached = buffered.clone();
    add_tool(
        &setup,
        tool("slow", move |_, api, context| {
            let reached = reached.clone();
            async move {
                api.output("durable\n")?;
                // The first output commits at once; this one waits for the next throttle window.
                tokio::time::sleep(Duration::from_millis(20)).await;
                api.output("buffered\n")?;
                reached.resolve();
                let signal = context.abort_signal().unwrap().clone();
                aborted(signal.clone()).await;
                Err(signal.reason().map_or_else(DurableError::aborted, DurableError::from))
            }
        }),
    );
    setup.faux.set_responses(one_call("slow"));
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    let submission = root.submit(SubmissionDraft::input("go"), &ctx()).await.unwrap();
    buffered.wait().await;
    let live = harness.snapshot(&LIVE_DOC, root.id, &ctx()).await.unwrap().unwrap();
    let task_id: TaskId = live["tools"][0]["taskId"].deserialize_into().unwrap();
    harness.abort_task(task_id, &ctx()).await.unwrap();
    submission.wait(&ctx()).await.unwrap();
    let text = result_text(&results(&all_entries(&root).await)[0]);
    assert_eq!(text, "durable\n|<harness>\n[error] Tool slow was aborted\n</harness>");
    harness.close(&ctx()).await.unwrap();
}
