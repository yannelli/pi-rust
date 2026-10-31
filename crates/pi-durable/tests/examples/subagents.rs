//! Example 23: persistent background subagents whose answers come back to the main agent, across a restart.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::Duration;

use pi_ai::{Message, StopReason};
use pi_durable::harness::agent::configure;
use pi_durable::harness::live::LIVE_DOC;
use pi_durable::storage::sqlite::open_sqlite_storage;
use pi_durable::tasks::TaskOptions;
use pi_durable::testing::faux::{
    FauxMessageOptions, FauxResponseStep, RegisterFauxProviderOptions, faux_assistant_message, faux_tool_call,
};

use super::*;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Subagent {
    conversation_id: ConversationId,
    /// Answers already reported to the main agent: several messages can end in one answer, reported once.
    reported: Vec<EntryId>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
struct Subagents {
    agents: BTreeMap<String, Subagent>,
    reporters: BTreeMap<String, TaskId>,
}

static SUBAGENTS: LazyLock<DocToken<Subagents, LatestConversation>> = LazyLock::new(|| {
    define_doc(DocSpec::new(
        "app.subagents",
        1,
        LatestConversation(LatestFork::Initial),
        Subagents::default,
        |_, _, _| Ok(false),
    ))
    .unwrap()
});

fn state(value: Value) -> Option<TaskState> {
    Some(serde_json::from_value(value).unwrap())
}

fn aborted() -> Option<TaskState> {
    state(json!({ "status": "terminal", "outcome": { "status": "aborted" } }))
}

fn completed() -> Option<TaskState> {
    state(json!({ "status": "terminal", "outcome": { "status": "completed", "result": null } }))
}

/// Owns subagent conversations; completes at once, and its conversations live on with it.
fn anchor() -> Task {
    define_task(
        TaskDefinition::new("app.subagent-anchor", 1, |_| Ok(Json::from(json!({ "phase": "done" }))))
            .phase("done", |_, runtime, context| async move {
                runtime.commit(|_, _| async { Ok(completed()) }, &context).await
            })
            .abort(|_, runtime, context| async move { runtime.commit(|_, _| async { Ok(aborted()) }, &context).await }),
    )
}

fn report_next(report: Option<String>) -> Option<TaskState> {
    state(json!({ "status": "running", "checkpoint": { "phase": "report", "report": report } }))
}

/// Delivers one message to a subagent, then reports its answer to the main conversation.
fn reporter() -> Task {
    define_task(
        TaskDefinition::new("app.subagent-reporter", 1, |_| Ok(Json::from(json!({ "phase": "deliver" }))))
            .phase("deliver", |task, runtime, context| async move {
                let input = task.input.to_serde();
                let name = input["name"].as_str().unwrap().to_owned();
                let child = serde_json::from_value(input["conversationId"].clone()).unwrap();
                let when_busy = if input["followUp"] == true { WhenBusy::FollowUp } else { WhenBusy::Steer };
                let subagent = runtime.conversation(child, &context).await?.unwrap();
                let content = pi_ai::UserContent::Text(input["message"].as_str().unwrap().to_owned());
                let request = SubmissionDraft::Input {
                    request_id: Some(format!("subagent:{}", task.id)),
                    content,
                    when_busy: Some(when_busy),
                    entry: None,
                };
                let settled = subagent.submit(request, &context).await?.wait(&context).await?;
                let main = runtime.conversation_id();
                let change = move |tx: Tx, _| async move {
                    if settled.status == SubmissionStatus::Unanswered {
                        let reason = settled.reason.unwrap_or_default();
                        return Ok(report_next(
                            (reason != "aborted").then(|| format!("[subagent {name} failed: {reason}]")),
                        ));
                    }
                    let Some(answer) = settled.answer else { return Ok(report_next(None)) };
                    let mut fresh = false;
                    tx.doc(&SUBAGENTS, main).await?.edit(|registry| {
                        let reported = &mut registry.agents.get_mut(&name).unwrap().reported;
                        fresh = !reported.contains(&answer);
                        if fresh {
                            reported.push(answer);
                        }
                    })?;
                    if !fresh {
                        return Ok(report_next(None));
                    }
                    let entry = tx.entry(answer).await?;
                    let text =
                        text_of(entry.and_then(|entry| entry.model).unwrap_or_default().first()).unwrap_or_default();
                    Ok(report_next(Some(format!("[subagent {name} answered, no reply needed] {text}"))))
                };
                runtime.commit(change, &context).await
            })
            .phase("report", |task, runtime, context| async move {
                let report = task.state.checkpoint().and_then(|c| c.get("report")?.as_str().map(str::to_owned));
                if let Some(report) = report {
                    let main = runtime.conversation(runtime.conversation_id(), &context).await?.unwrap();
                    let content = pi_ai::UserContent::Text(report);
                    let request_id = Some(format!("subagent-report:{}", task.id));
                    main.submit(
                        SubmissionDraft::Input {
                            request_id,
                            content,
                            when_busy: Some(WhenBusy::FollowUp),
                            entry: None,
                        },
                        &context,
                    )
                    .await?;
                }
                runtime.commit(|_, _| async { Ok(completed()) }, &context).await
            })
            .abort(|_, runtime, context| async move { runtime.commit(|_, _| async { Ok(aborted()) }, &context).await }),
    )
}

fn subagent_tool(anchor: Task, reporter: Task) -> ToolRegistration {
    let parameters = json!({ "type": "object", "properties": {
        "action": { "enum": ["spawn", "send", "stop", "status"] }, "name": { "type": "string" },
        "message": { "type": "string" }, "followUp": { "type": "boolean" },
    }, "required": ["action"] });
    let description = "Manage persistent subagents that work in the background.";
    define_tool("subagent", description, parameters, move |args, api, call| {
        let (anchor, reporter) = (anchor.clone(), reporter.clone());
        async move {
            let field = |key: &str| args.get(key).and_then(Value::as_str).map(str::to_owned);
            let (action, name, message) = (field("action").unwrap_or_default(), field("name"), field("message"));
            let follow_up = args.get("followUp").and_then(Value::as_bool) == Some(true);
            let read = |value: Option<Json>| {
                value.map(|value| value.deserialize_into::<Subagents>().unwrap()).unwrap_or_default()
            };
            let registry = read(api.snapshot(&SUBAGENTS, api.conversation_id, &call).await?);
            let reply = |text: String| Ok(ToolExecutionResult::text(text));
            if action == "status" {
                let mut lines = Vec::new();
                for (each, found) in
                    registry.agents.iter().filter(|(each, _)| name.as_ref().is_none_or(|name| name == *each))
                {
                    let live = api.snapshot(&LIVE_DOC, found.conversation_id, &call).await?;
                    let busy = live.is_some_and(|live| live.get("run").is_some_and(|run| !run.is_null()));
                    lines.push(format!("{each}: {}", if busy { "working" } else { "idle" }));
                }
                return reply(if lines.is_empty() { "No subagents.".into() } else { lines.join("\n") });
            }
            let Some(name) = name else { return reply(format!("{action} needs a name.")) };
            let agent = registry.agents.get(&name).cloned();
            if action != "spawn" && agent.is_none() {
                return reply(format!("No subagent named {name}."));
            }
            if action == "stop" {
                api.conversation(agent.unwrap().conversation_id, &call).await?.unwrap().abort(&call, false).await?;
                return reply(format!("Stopped {name}."));
            }
            let Some(message) = message else { return reply(format!("{action} needs a message.")) };
            let (main, owner) = (api.conversation_id, api.task_id);
            let change = move |tx: Tx| async move {
                let background = TaskOptions { background: true, ..TaskOptions::conversation(main) };
                let mut state = tx.doc(&SUBAGENTS, main).await?.get()?;
                if action == "spawn" {
                    if state.agents.contains_key(&name) {
                        return Ok(format!("{name} already exists; use send."));
                    }
                    let anchor = tx.create_task(anchor, Json::Null, background.clone()).await?;
                    let child = tx.create_conversation(ConversationOwnership::Task { task_id: anchor }).await?;
                    let change = AgentChange {
                        extensions: Change::Set(ExtensionSelection::Edit {
                            add: None,
                            remove: Some(vec!["subagent-tools".into()]),
                        }),
                        instructions: Change::Set(format!(
                            "You are the subagent \"{name}\". Answer the main agent's requests."
                        )),
                        ..AgentChange::default()
                    };
                    configure(&tx, child.id, &change).await?;
                    state.agents.insert(name.clone(), Subagent { conversation_id: child.id, reported: Vec::new() });
                }
                let conversation_id = state.agents[&name].conversation_id;
                let input = json!({ "name": name, "conversationId": conversation_id, "message": message, "followUp": action == "send" && follow_up });
                let id = tx.create_task(reporter, Json::from(input), background).await?;
                state.reporters.insert(owner.to_string(), id);
                tx.doc(&SUBAGENTS, main).await?.edit(|registry| *registry = state)?;
                Ok(if action == "send" { format!("Sent to {name}.") } else { format!("Started {name}.") })
            };
            reply(api.commit(change, &call).await?)
        }
    })
}

fn route(messages: &[Message]) -> pi_ai::AssistantMessage {
    let call = |input: Value| {
        faux_assistant_message(
            vec![faux_tool_call("subagent", input)],
            FauxMessageOptions::stop_reason(StopReason::ToolUse),
        )
    };
    let answer = |text: &str| {
        faux_assistant_message(vec![pi_durable::testing::faux::faux_text(text)], FauxMessageOptions::default())
    };
    let last = messages.iter().rev().find(|message| !matches!(message, Message::System(_)));
    let text = match last {
        Some(Message::ToolResult(result)) => {
            let text: String = result.content.iter().filter_map(pi_ai::ContentBlock::as_text).collect();
            return answer(&format!("OK. {text}"));
        }
        other => text_of(other).unwrap_or_default(),
    };
    if text.contains("Start a subagent") {
        call(json!({ "action": "spawn", "name": "reader", "message": "Summarize the plot of Moby Dick." }))
    } else if text.contains("whale's name") {
        call(json!({ "action": "send", "name": "reader", "message": "What is the whale called?" }))
    } else if text.contains("every chapter") {
        call(json!({ "action": "send", "name": "reader", "message": "Now go through all chapters in detail." }))
    } else if text.contains("Stop reader") {
        call(json!({ "action": "stop", "name": "reader" }))
    } else if text.contains("my subagents") {
        call(json!({ "action": "status" }))
    } else if text.contains("[subagent") {
        answer("Noted.")
    } else if text.contains("Summarize the plot") {
        answer("A whale, a captain, an obsession.")
    } else if text.contains("whale called") {
        answer("Moby Dick.")
    } else {
        answer(&(1..=135).map(|index| format!("Chapter {index}: more whaling.")).collect::<Vec<_>>().join("\n"))
    }
}

#[path = "subagents_run.rs"]
mod run;
