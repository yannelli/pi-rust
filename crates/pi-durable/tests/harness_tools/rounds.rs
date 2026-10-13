//! The remaining upstream `describe("tool round")` cases.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use pi_ai::Message;
use pi_durable::SubmissionStatus;
use pi_durable::harness::*;
use pi_durable::storage::MemoryStorage;
use pi_durable::testing::faux::FauxResponseStep;
use serde_json::{Value, json};

use super::support::harness_support::*;
use super::{Run, add_tool, call_message, calls, done, empty_result, result_text, results, run, tool};

/// The `toolsRemoved` or `toolsAdded` names of the last system entry.
fn last_system(entries: &[pi_durable::EntryRecord]) -> pi_ai::SystemMessage {
    let system = entries.iter().rev().find(|entry| entry.kind == "pi.system").unwrap();
    let Some(Message::System(message)) = system.model.as_ref().and_then(|model| model.first()).cloned() else {
        panic!("expected a system message");
    };
    message
}

fn removed(system: &pi_ai::SystemMessage) -> Value {
    serde_json::to_value(&system.tools_removed).unwrap()
}

#[tokio::test]
async fn answers_a_call_to_a_tool_deactivated_after_preparation_with_tool_unavailable() {
    let setup = chat_setup(Default::default());
    let seen = Arc::new(Mutex::new(Vec::<&str>::new()));
    let sink = seen.clone();
    add_tool(
        &setup,
        tool("echo", move |_, _, _| {
            sink.lock().unwrap().push("ran");
            async { Ok(empty_result()) }
        }),
    );
    let root_slot = Arc::new(Mutex::new(None::<Conversation>));
    let slot = root_slot.clone();
    let deactivate = FauxResponseStep::async_factory(move |_, _, _, _| {
        let root = slot.lock().unwrap().clone().unwrap();
        Box::pin(async move {
            let none = AgentChange { tools: Change::Set(ToolFilter::List(Vec::new())), ..AgentChange::default() };
            root.configure(none, &ctx()).await.map_err(|error| error.to_string())?;
            Ok(call_message(&[("echo", json!({}), "c1")]))
        })
    });
    setup.faux.set_responses([deactivate, done()]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    *root_slot.lock().unwrap() = Some(root.clone());
    root.submit(SubmissionDraft::input("go"), &ctx()).await.unwrap().wait(&ctx()).await.unwrap();
    let entries = all_entries(&root).await;
    assert!(seen.lock().unwrap().is_empty());
    assert_eq!(result_text(&results(&entries)[0]), "<harness>\n[error] Tool echo is not available\n</harness>");
    // The next preparation removes it.
    assert_eq!(removed(&last_system(&entries)), json!([{ "name": "echo" }]));
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn removes_unregistered_active_tools_from_the_offer_and_adds_them_back_after_re_registration() {
    let setup = chat_setup(Default::default());
    let echo = tool("echo", |_, _, _| async { Ok(empty_result()) });
    add_tool(&setup, echo.clone());
    let first = run(&setup, vec![done()]).await;
    setup.registry.uninstall("tool:echo").unwrap();
    setup.faux.set_responses([calls(&[("echo", json!({}), "c1")]), done()]);
    let second = first.root.submit(SubmissionDraft::input("again"), &ctx()).await.unwrap().wait(&ctx()).await.unwrap();
    assert_eq!(second.status, SubmissionStatus::Done);
    let entries = all_entries(&first.root).await;
    assert_eq!(removed(&last_system(&entries)), json!([{ "name": "echo" }]));
    assert!(results(&entries)[0].is_error);
    // The stored agent is not rewritten; the tool is only not resolved.
    assert!(first.root.agent(&ctx()).await.unwrap().tools.is_empty());

    add_tool(&setup, echo);
    setup.faux.set_responses([done()]);
    first.root.submit(SubmissionDraft::input("back"), &ctx()).await.unwrap().wait(&ctx()).await.unwrap();
    let entries = all_entries(&first.root).await;
    let added = last_system(&entries).tools_added.unwrap_or_default();
    assert_eq!(added.iter().map(|tool| tool.name.as_str()).collect::<Vec<_>>(), ["echo"]);
    first.harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn produces_tool_unavailable_when_the_implementation_is_unregistered_before_its_task_runs() {
    let setup = chat_setup(Default::default());
    let registry = setup.registry.clone();
    let first = tool("first", move |_, _, _| {
        let result = registry.uninstall("tool:second");
        async move {
            result?;
            Ok(empty_result())
        }
    });
    add_tool(&setup, first.with_execution_mode(ToolExecutionMode::Sequential));
    add_tool(&setup, tool("second", |_, _, _| async { Ok(ToolExecutionResult::text("ran")) }));
    let round = calls(&[("first", json!({}), "c1"), ("second", json!({}), "c2")]);
    let Run { harness, entries, status, .. } = run(&setup, vec![round, done()]).await;
    assert_eq!(status, SubmissionStatus::Done);
    assert_eq!(result_text(&results(&entries)[1]), "<harness>\n[error] Tool second is not available\n</harness>");
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn reads_the_execution_mode_when_a_round_starts_and_keeps_it_for_the_round() {
    let setup = chat_setup(Default::default());
    let events = Arc::new(Mutex::new(Vec::new()));
    for name in ["a", "b"] {
        let (events, settings) = (events.clone(), setup.settings.clone());
        add_tool(
            &setup,
            tool(name, move |_, _, _| {
                let (events, settings) = (events.clone(), settings.clone());
                async move {
                    events.lock().unwrap().push(format!("start {name}"));
                    // Changed after the round started: not seen by this round.
                    settings.lock().unwrap().tool_execution = Some(ToolExecutionMode::Parallel);
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    events.lock().unwrap().push(format!("end {name}"));
                    Ok(empty_result())
                }
            }),
        );
    }
    // Changed while the model request runs: the round that follows uses it.
    let settings = setup.settings.clone();
    let request = FauxResponseStep::factory(move |_, _, _, _| {
        settings.lock().unwrap().tool_execution = Some(ToolExecutionMode::Sequential);
        call_message(&[("a", json!({}), "c1"), ("b", json!({}), "c2")])
    });
    let Run { harness, .. } = run(&setup, vec![request, done()]).await;
    assert_eq!(*events.lock().unwrap(), ["start a", "end a", "start b", "end b"]);
    harness.close(&ctx()).await.unwrap();
}
