//! The example 23 script: spawn, report, stop, status, and a report delivered after a restart.

use super::*;
use crate::support::{ValueCall, on_value};

type Lines = Arc<Mutex<Vec<String>>>;

/// One printed line per message the main conversation commits; subagents work in their own conversations.
fn line(message: &Message) -> Vec<String> {
    match message {
        Message::User(_) => {
            let text = text_of(Some(message)).unwrap_or_default();
            // A report `[subagent <name> <status>] <text>` prints as `> <name>: <text or status>`.
            let report = text.strip_prefix("[subagent ").and_then(|rest| {
                let (head, tail) = rest.split_once(']')?;
                let (name, status) = head.split_once(' ')?;
                let tail = tail.trim_start();
                Some(format!("> {name}: {}", if tail.is_empty() { status } else { tail }))
            });
            vec![report.unwrap_or_else(|| format!("> {text}"))]
        }
        Message::Assistant(assistant) => {
            let calls = assistant.tool_calls().map(|call| {
                let field = |key: &str| call.arguments.get(key).and_then(Value::as_str);
                let name = field("name").map(|name| format!(" {name}")).unwrap_or_default();
                let quoted = field("message").map(|sent| format!(" {}", json!(sent))).unwrap_or_default();
                format!("  {} {}{name}{quoted}", call.name, field("action").unwrap_or_default())
            });
            let text = text_of(Some(message)).filter(|text| !text.is_empty());
            calls.chain(text).collect()
        }
        Message::ToolResult(result) => {
            vec![format!("  → {}", result.content.iter().filter_map(pi_ai::ContentBlock::as_text).collect::<String>())]
        }
        _ => Vec::new(),
    }
}

async fn follow(harness: &Harness, id: ConversationId, lines: &Lines) -> ConversationWatch {
    let conversation = harness.conversation(id, &ctx()).await.unwrap().unwrap();
    let watch = conversation.watch(&ctx(), None).await.unwrap();
    let newest_of = |view: &Value| view["entries"].as_array().and_then(|e| e.last()).and_then(|e| e["id"].as_u64());
    let newest = Mutex::new(newest_of(&watch.value().to_serde()).unwrap_or(0));
    let sink = lines.clone();
    watch
        .start(on_value(move |ValueCall { value, .. }, _| {
            let value = value.to_serde();
            let mut newest = newest.lock().unwrap();
            let since = *newest;
            let entries = value["entries"].as_array().into_iter().flatten();
            for entry in entries.filter(|entry| entry["id"].as_u64().is_some_and(|id| id > since)) {
                if let Ok(message) = serde_json::from_value::<Message>(entry["model"][0].clone()) {
                    sink.lock().unwrap().extend(line(&message));
                }
            }
            *newest = newest_of(&value).unwrap_or(since).max(since);
            async { Ok(()) }
        }))
        .unwrap();
    watch
}

struct Run {
    harness: Harness,
    root: Conversation,
}

impl Run {
    async fn say(&self, text: &str) {
        self.root.submit(SubmissionDraft::input(text), &ctx()).await.unwrap().wait(&ctx()).await.unwrap();
    }

    async fn registry(&self) -> Subagents {
        let value = self.harness.snapshot(&SUBAGENTS, self.root.id, &ctx()).await.unwrap();
        value.map(|value| value.deserialize_into().unwrap()).unwrap_or_default()
    }

    /// Wait until every message to a subagent was answered and reported, and the main agent has reacted.
    async fn settle(&self) {
        for id in self.registry().await.reporters.into_values() {
            self.harness.wait_for_task(id, &ctx()).await.unwrap();
        }
        self.root.wait_for_idle(&ctx()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    async fn working(&self, name: &str) -> bool {
        let Some(agent) = self.registry().await.agents.get(name).cloned() else { return false };
        let live = self.harness.snapshot(&LIVE_DOC, agent.conversation_id, &ctx()).await.unwrap();
        live.is_some_and(|live| live.get("run").is_some_and(|run| !run.is_null()))
    }
}

fn take(lines: &Lines) -> Vec<String> {
    std::mem::take(&mut *lines.lock().unwrap())
}

#[tokio::test]
async fn ex23_subagent_background_reports_answers_stops_and_resumes_after_restart() {
    let setup = chat_setup(RegisterFauxProviderOptions { tokens_per_second: Some(50.0), ..Default::default() });
    let steps = (0..40).map(|_| {
        FauxResponseStep::async_factory(|transcript, _, _, _| {
            let message = route(&transcript.messages);
            Box::pin(async move { Ok(message) })
        })
    });
    setup.faux.set_responses(steps.collect::<Vec<_>>());
    let (anchor, reporter) = (anchor(), reporter());
    let tools = Extension::new("subagent-tools").task(anchor.clone()).task(reporter.clone());
    setup.registry.install(tools.tool(subagent_tool(anchor, reporter))).unwrap();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.sqlite").to_string_lossy().into_owned();
    let open = || async {
        let storage = Arc::new(open_sqlite_storage(&path).await.unwrap());
        let (harness, root) = open_chat_with(storage, &setup, setup.options()).await;
        Run { harness, root }
    };
    let lines = Lines::default();
    let run = open().await;
    let _stream = follow(&run.harness, run.root.id, &lines).await;

    run.say("Start a subagent named reader that summarizes Moby Dick.").await;
    run.settle().await;
    let spawned = [
        "> Start a subagent named reader that summarizes Moby Dick.",
        "  subagent spawn reader \"Summarize the plot of Moby Dick.\"",
        "  → Started reader.",
        "OK. Started reader.",
        "> reader: A whale, a captain, an obsession.",
        "Noted.",
    ];
    assert_eq!(take(&lines), spawned);

    // A stopped subagent's message settles `aborted`, which the reporter does not report.
    run.say("Ask reader to summarize every chapter.").await;
    wait_for(|| run.working("reader")).await;
    run.say("Stop reader.").await;
    run.settle().await;
    let stopped = [
        "> Ask reader to summarize every chapter.",
        "  subagent send reader \"Now go through all chapters in detail.\"",
        "  → Sent to reader.",
        "OK. Sent to reader.",
        "> Stop reader.",
        "  subagent stop reader",
        "  → Stopped reader.",
        "OK. Stopped reader.",
    ];
    assert_eq!(take(&lines), stopped);
    run.say("What are my subagents doing?").await;
    run.settle().await;
    let status = ["> What are my subagents doing?", "  subagent status", "  → reader: idle", "OK. reader: idle"];
    assert_eq!(take(&lines), status);

    // The process stops once the reporter for the whale question exists; the next process delivers its report.
    let before = run.registry().await.reporters.len();
    run.root.submit(SubmissionDraft::input("Ask reader for the whale's name."), &ctx()).await.unwrap();
    wait_for(|| async { run.registry().await.reporters.len() > before }).await;
    run.harness.close(&ctx()).await.unwrap();
    take(&lines);
    let run = open().await;
    let _stream = follow(&run.harness, run.root.id, &lines).await;
    run.settle().await;
    let after = take(&lines);
    assert!(after.contains(&"> reader: Moby Dick.".to_owned()), "{after:?}");
    assert_eq!(after.last().map(String::as_str), Some("Noted."));
    run.harness.close(&ctx()).await.unwrap();
}
