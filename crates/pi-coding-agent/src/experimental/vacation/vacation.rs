//! Port of upstream `vacation/vacation.ts`: the `search` tool, the background `research` subagent and its
//! report task, and the planner prompt.

use std::sync::{Arc, LazyLock};
use std::time::Duration;

use pi_ai::UserContent;
use pi_chord::{ChordError, Context, Json, await_with_context};
use pi_durable::harness::agent::configure;
use pi_durable::harness::task::{Task, TaskDefinition, define_task};
use pi_durable::harness::tool::ToolExecutionApi;
use pi_durable::harness::tool_registration::ToolReplay;
use pi_durable::harness::types::{
    AgentChange, Change, ExtensionSelection, SubmissionDraft, ToolExecutionResult, WhenBusy,
};
use pi_durable::harness::{Extension, TaskRuntime, define_tool, section};
use pi_durable::tasks::TaskOptions;
use pi_durable::{
    ConversationOwnership, ConversationQuery, DurableError, DurableResult, SubmissionStatus, SubmissionType,
    TaskRecord, TaskState,
};
use serde_json::{Value, json};

use crate::experimental::durable::subagent::assistant_text;

pub const VACATION_EXTENSION: &str = "vacation";
pub const SEARCH_EXTENSION: &str = "vacation-search";

/// Canned results; each topic takes a different time, so a crash can land between them.
fn canned(topic: &str) -> Option<(u64, &'static [&'static str])> {
    match topic {
        "weather" => Some((6, &["Saturday: 24°C and sunny", "Sunday: 21°C, a short shower around 3 pm"])),
        "museums" => Some((
            10,
            &[
                "Kunsthistorisches Museum: open 10-18, book a time slot",
                "Belvedere: Klimt's The Kiss, quietest before 11",
                "Albertina: Monet to Picasso, open until 21 on Friday",
            ],
        )),
        "trains" => Some((30, &["Railjet from Salzburg: 2h 22m, every 30 min", "Nightjet from Munich: arrives 06:20"])),
        _ => None,
    }
}

async fn run_search(
    topic: String,
    city: String,
    api: ToolExecutionApi,
    context: Context,
) -> DurableResult<ToolExecutionResult> {
    let Some((seconds, results)) = canned(&topic) else {
        return Err(DurableError::Error(format!("Unknown topic: {topic}")));
    };
    let steps = seconds.div_ceil(2);
    for step in 1..=steps {
        api.output(&format!("searching {topic} in {city}: source {step}/{steps}\n"))?;
        let sleep = async {
            tokio::time::sleep(Duration::from_secs(2)).await;
            Ok::<_, ChordError>(())
        };
        await_with_context(sleep, &context).await?;
    }
    for result in results {
        api.output(&format!("- {result}\n"))?;
    }
    Ok(ToolExecutionResult::default())
}

/// What the research subagent gets: only `search`.
pub fn search_extension() -> Extension {
    let parameters = json!({
        "type": "object",
        "properties": {
            "topic": { "anyOf": [{ "const": "weather", "type": "string" }, { "const": "museums", "type": "string" }, { "const": "trains", "type": "string" }] },
            "city": { "type": "string" },
        },
        "required": ["topic", "city"],
    });
    let description =
        "Search travel information about a city. Topics: weather, museums, trains. Slow; call several in parallel.";
    let tool = define_tool("search", description, parameters, |args, api, context| {
        let field = |key: &str| args.get(key).and_then(Value::as_str).unwrap_or_default().to_string();
        run_search(field("topic"), field("city"), api, context)
    })
    // Only reads, so a call cut off by a crash simply runs again.
    .with_replay(ToolReplay::Safe);
    Extension::new(SEARCH_EXTENSION).tool(tool)
}

fn next(state: Value) -> DurableResult<Option<TaskState>> {
    serde_json::from_value(state).map(Some).map_err(|error| DurableError::Error(error.to_string()))
}

/// Deliver the task to its subagent and keep the report as the next checkpoint.
async fn deliver(task: TaskRecord, runtime: TaskRuntime, context: Context) -> DurableResult<()> {
    // The subagent's conversation is owned by this task. No Session call may run inside a commit.
    let owner = task.id;
    let owned = runtime
        .commit_map(
            move |tx| async move {
                let query = ConversationQuery { owner_task_id: Some(owner), ..ConversationQuery::default() };
                Ok(tx.scan_conversations(query, 1, None).await?.items.first().map(|record| record.id))
            },
            &context,
        )
        .await?;
    let owned = owned.ok_or_else(|| DurableError::Error("Research subagent conversation is missing".into()))?;
    let child = runtime.conversation(owned, &context).await?;
    let child = child.ok_or_else(|| DurableError::Error("Research subagent conversation is missing".into()))?;
    // A rerun after a crash gets the same submission back.
    let content = task.input.get("task").and_then(Json::as_str).unwrap_or_default().to_string();
    let request = SubmissionDraft::Input {
        request_id: Some(format!("research:{}", task.id)),
        content: UserContent::Text(content),
        when_busy: None,
        entry: None,
    };
    let settled = child.submit(request, &context).await?.wait(&context).await?;
    runtime
        .commit(
            move |tx, _| async move {
                let reason = match settled.status {
                    SubmissionStatus::Unanswered => settled.reason.clone().unwrap_or_default(),
                    _ => "?".into(),
                };
                let mut report = format!("[research report] The research failed: {reason}");
                let answer = settled.answer.filter(|_| settled.status == SubmissionStatus::Done);
                if let Some(answer) = answer.filter(|_| settled.kind == SubmissionType::Input) {
                    let entry = tx.entry(answer).await?.filter(|entry| entry.kind == "pi.assistant");
                    report = format!(
                        "[research report] {}",
                        assistant_text(entry.and_then(|e| e.model).unwrap_or_default().first())
                    );
                }
                next(json!({ "status": "running", "checkpoint": { "phase": "report", "report": report } }))
            },
            &context,
        )
        .await
}

/// Post the report to the main conversation as a new message.
async fn report(task: TaskRecord, runtime: TaskRuntime, context: Context) -> DurableResult<()> {
    let main = runtime.conversation(runtime.conversation_id(), &context).await?;
    let main = main.ok_or_else(|| DurableError::Error("Main conversation is missing".into()))?;
    let text = task.state.checkpoint().and_then(|checkpoint| checkpoint.get("report")?.as_str().map(str::to_owned));
    let request = SubmissionDraft::Input {
        request_id: Some(format!("research-report:{}", task.id)),
        content: UserContent::Text(text.unwrap_or_default()),
        when_busy: Some(WhenBusy::FollowUp),
        entry: None,
    };
    main.submit(request, &context).await?;
    let completed = json!({ "status": "terminal", "outcome": { "status": "completed", "result": null } });
    runtime.commit(move |_, _| async move { next(completed) }, &context).await
}

/// Delivers the task to its subagent and posts the subagent's report to the main conversation.
pub static RESEARCH: LazyLock<Task> = LazyLock::new(|| {
    define_task(
        TaskDefinition::new("vacation.research", 1, |_| Ok(Json::from(json!({ "phase": "deliver" }))))
            .phase("deliver", deliver)
            .phase("report", report)
            .abort(|_, runtime, context| async move {
                let aborted = json!({ "status": "terminal", "outcome": { "status": "aborted" } });
                runtime.commit(move |_, _| async move { next(aborted) }, &context).await
            }),
    )
});

const RESEARCH_INSTRUCTIONS: &str = "You are a research subagent for a vacation planner. Search weather, museums, and trains in parallel, in one step, then report the findings in a few short bullet points.";

async fn start_research(task: String, api: ToolExecutionApi, context: Context) -> DurableResult<ToolExecutionResult> {
    let main = api.conversation_id;
    api.commit(
        move |tx| async move {
            // Background: the main conversation stays free while the research runs.
            let options = TaskOptions { background: true, ..TaskOptions::conversation(main) };
            let research: Arc<dyn pi_durable::tasks::TaskTemplate> = RESEARCH.clone();
            let owner = tx.create_task(research, Json::from(json!({ "task": task })), options).await?;
            let child = tx.create_conversation(ConversationOwnership::Task { task_id: owner }).await?;
            let change = AgentChange {
                extensions: Change::Set(ExtensionSelection::List(vec![SEARCH_EXTENSION.into()])),
                instructions: Change::Set(RESEARCH_INSTRUCTIONS.into()),
                ..AgentChange::default()
            };
            configure(&tx, child.id, &change).await
        },
        &context,
    )
    .await?;
    Ok(ToolExecutionResult::text("Research started in the background."))
}

const PREAMBLE: &str = "You are a friendly vacation planning assistant. Keep answers short. Hand all research to the research tool; while it runs, keep chatting with the user. When a [research report] arrives, turn it into a short plan.";

/// What the main conversation gets: the prompt, `research`, and the task that delivers its report.
pub fn vacation_extension() -> Extension {
    let parameters = json!({
        "type": "object",
        "properties": { "task": { "type": "string", "description": "What to research, with the city and dates" } },
        "required": ["task"],
    });
    let description = "Start a research subagent in the background. It searches while you keep talking with the user; its report arrives later as a message starting with [research report].";
    let research = define_tool("research", description, parameters, |args, api, context| {
        let task = args.get("task").and_then(Value::as_str).unwrap_or_default().to_string();
        start_research(task, api, context)
    });
    let preamble = section("preamble", |_, _| async { Ok(Some(PREAMBLE.to_string())) }, Some(false));
    Extension::new(VACATION_EXTENSION).tool(research).task(RESEARCH.clone()).section(preamble)
}
