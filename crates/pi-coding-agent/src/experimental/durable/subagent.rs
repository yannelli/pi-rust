//! Port of upstream `durable/subagent.ts`: the foreground subagent tool.

use pi_ai::{ContentBlock, Message, UserContent};
use pi_chord::{Context, Json};
use pi_durable::harness::agent::configure;
use pi_durable::harness::tool::ToolExecutionApi;
use pi_durable::harness::tool_registration::ToolReplay;
use pi_durable::harness::types::{AgentChange, Change, ExtensionSelection, SubmissionDraft, ToolExecutionResult};
use pi_durable::harness::{Extension, define_tool};
use pi_durable::{
    ConversationId, ConversationOwnership, ConversationQuery, DurableError, DurableResult, EntryId, SubmissionStatus,
    SubmissionType,
};
use serde_json::json;

pub const SUBAGENT_EXTENSION: &str = "subagent";

/// The text of an assistant entry's message.
pub(crate) fn assistant_text(message: Option<&Message>) -> String {
    match message {
        Some(Message::Assistant(message)) => message
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect(),
        _ => String::new(),
    }
}

async fn answer_text(api: &ToolExecutionApi, answer: EntryId, context: &Context) -> DurableResult<String> {
    let entry = api.commit(move |tx| async move { tx.entry(answer).await }, context).await?;
    let entry = entry.filter(|entry| entry.kind == "pi.assistant");
    Ok(assistant_text(entry.and_then(|entry| entry.model).unwrap_or_default().first()))
}

async fn run_subagent(task: String, api: ToolExecutionApi, context: Context) -> DurableResult<ToolExecutionResult> {
    let task_id = api.task_id;
    let child: ConversationId = api
        .commit(
            move |tx| async move {
                let query = ConversationQuery { owner_task_id: Some(task_id), ..ConversationQuery::default() };
                if let Some(existing) = tx.scan_conversations(query, 1, None).await?.items.first() {
                    return Ok(existing.id);
                }
                // Starts as a copy of this conversation's agent; without this extension it cannot delegate further.
                let created = tx.create_conversation(ConversationOwnership::Task { task_id }).await?;
                let remove = ExtensionSelection::Edit { add: None, remove: Some(vec![SUBAGENT_EXTENSION.into()]) };
                let change = AgentChange { extensions: Change::Set(remove), ..AgentChange::default() };
                configure(&tx, created.id, &change).await?;
                Ok(created.id)
            },
            &context,
        )
        .await?;
    api.details(Json::from(json!({ "conversationId": child })), &context).await?;
    let handle = api
        .conversation(child, &context)
        .await?
        .ok_or_else(|| DurableError::Error(format!("Subagent {child} does not exist")))?;
    let request = SubmissionDraft::Input {
        request_id: Some(format!("subagent:{task_id}")),
        content: UserContent::Text(task),
        when_busy: None,
    };
    let settled = handle.submit(request, &context).await?.wait(&context).await?;
    let answer = settled.answer.filter(|_| settled.status == SubmissionStatus::Done);
    let Some(answer) = answer.filter(|_| settled.kind == SubmissionType::Input) else {
        let status = serde_json::to_value(settled.status).unwrap_or_default();
        return Err(DurableError::Error(format!("Subagent {child} failed: {}", status.as_str().unwrap_or("?"))));
    };
    let text = answer_text(&api, answer, &context).await?;
    let mut result = ToolExecutionResult::text(text);
    result.details = Some(Json::from(json!({ "conversationId": child })));
    Ok(result)
}

/// A foreground subagent: each call creates a child conversation owned by the call's task, runs the task
/// there, and returns the child's answer. Aborting the call aborts the child. The child outlives the call,
/// so the user can switch to it and keep talking.
pub fn subagent() -> Extension {
    let parameters = json!({
        "type": "object",
        "properties": { "task": { "type": "string", "description": "What the subagent should do" } },
        "required": ["task"],
    });
    let description = "Delegate a self-contained task to a subagent with the same tools and get its answer back. Give it everything it needs to know; it does not see this conversation.";
    let tool = define_tool("subagent", description, parameters, |args, api, context| {
        let task = args.get("task").and_then(|task| task.as_str()).unwrap_or_default().to_string();
        run_subagent(task, api, context)
    })
    // A rerun after a crash finds the child it created and the submission it made.
    .with_replay(ToolReplay::Safe);
    Extension::new(SUBAGENT_EXTENSION).tool(tool)
}
