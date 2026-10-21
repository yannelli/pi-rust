//! Built-in tool task (upstream `harness/tool.ts`): resolves the called tool among its phase agent's tools,
//! validates, runs `before_tool`, records intent, executes, runs `after_tool`, and settles the result. A
//! model-issued call appends its result entry; a nested call writes its result to its caller's `NESTED_CALL_DOC`.

mod api;
mod execute;
mod result;
mod settle;

use std::sync::{Arc, LazyLock};

use futures::future::BoxFuture;
use pi_ai::{JsonObject, ToolCall};
use pi_chord::{Context, Json};
use pi_telemetry::SpanAttributes;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use api::{ExecuteToolOptions, ToolExecutionApi};
pub use result::{append_tool_result, harness_error};

use super::scheduler::{HookApi, TaskRuntime};
use super::task::{Task, TaskDefinition, define_task};
use super::tool_registration::{ToolRegistration, ToolReplay};
use super::types::{NestedToolExecutionResult, ToolControl, ToolExecutionResult};
use crate::documents::{DocFamilySpec, DocFamilyToken, TaskScope, define_doc_family, encode};
use crate::entries::ASSISTANT_ENTRY;
use crate::errors::{DurableError, DurableResult};
use crate::ids::{EntryId, TaskId};
use crate::types::TaskRecord;

/// A tool call stored with its content `type`, as upstream keeps it.
mod tool_call_block {
    use pi_ai::{ContentBlock, ToolCall};
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(call: &ToolCall, serializer: S) -> Result<S::Ok, S::Error> {
        ContentBlock::ToolCall(call.clone()).serialize(serializer)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<ToolCall, D::Error> {
        match ContentBlock::deserialize(deserializer)? {
            ContentBlock::ToolCall(call) => Ok(call),
            _ => Err(serde::de::Error::custom("expected a toolCall block")),
        }
    }
}

/// A model-issued call, read from its assistant entry, or a nested call whose call the input carries.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
#[allow(clippy::large_enum_variant)]
pub enum ToolTaskInput {
    Model {
        assistant: EntryId,
        call_id: String,
    },
    Nested {
        parent: TaskId,
        parent_call_id: String,
        key: String,
        #[serde(with = "tool_call_block")]
        call: ToolCall,
        /// `false`: commit no running output, details, or diagnostics to the slot.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        progress: Option<bool>,
    },
}

/// Checkpoint of a tool task: before intent, or the recorded intent of an execution.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "camelCase")]
pub enum ToolTaskCheckpoint {
    Call,
    Execute { arguments: JsonObject, replay: ToolReplay },
}

/// Result of a tool task: a model-issued call's entry, or a nested call's receipt.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum ToolTaskResult {
    Model {
        entry_id: EntryId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        control: Option<ToolControl>,
    },
    Nested,
}

/// A call as tool hooks see it; `parent` is the calling task and call ID of a nested call.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolHookCall {
    pub call: ToolCall,
    pub parent: Option<(TaskId, String)>,
}

/// What `before_tool` decides: block the call, or replace its arguments.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BeforeToolDecision {
    pub arguments: Option<JsonObject>,
    pub block: Option<String>,
}

type Hook<A, R> = Arc<dyn Fn(A, HookApi, Context) -> BoxFuture<'static, DurableResult<R>> + Send + Sync>;

/// Hooks of the built-in tool task, for model-issued and nested calls.
#[derive(Clone, Default)]
pub struct ToolHooks {
    /// Before intent; the first block wins, otherwise arguments replace the call's. A failure blocks.
    pub before_tool: Option<Hook<ToolHookCall, Option<BeforeToolDecision>>>,
    /// After execution, before the result is committed; replaces the result.
    pub after_tool: Option<Hook<(ToolHookCall, ToolExecutionResult), Option<ToolExecutionResult>>>,
}

/// Memo of a tool task that made a nested call, in any attempt, committed before the first admission of each attempt:
/// only then can `pi.live` hold slots below it, which settling removes. A crash between the two only costs a scan.
pub(crate) const MADE_NESTED_CALLS: &str = "pi.tool.madeNestedCalls";

/// One nested call of a running tool: its task, and once it ended, its result.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NestedCallState {
    /// Absent only inside the commit that creates the call; a creation that fails leaves no member.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<TaskId>,
    /// Exactly what `execute_tool()` returns; absent while running, and for a call the scheduler faulted or orphaned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<NestedToolExecutionResult>,
}

/// The nested calls of a tool call, a task-scoped family of the calling task keyed by the nested call's key: so a rerun
/// finds the calls it already made, and the caller reads each result. Created with the nested task; the nested call's
/// terminal commit adds its result. One document per call, so no commit rewrites the others. Retires with the caller.
pub static NESTED_CALL_DOC: LazyLock<DocFamilyToken<NestedCallState, (), TaskScope>> = LazyLock::new(|| {
    let spec =
        DocFamilySpec::new("pi.tool.nested-call", 1, TaskScope, |()| NestedCallState::default(), |_, _, _| Ok(false));
    define_doc_family(spec).unwrap_or_else(|_| unreachable!("pi.tool.nested-call is a valid definition"))
});

static TOOL_TASK: LazyLock<Task> = LazyLock::new(|| {
    let definition = TaskDefinition::new("pi.tool", 2, |_| encode(&ToolTaskCheckpoint::Call))
        .migrate(|input, checkpoint, _| {
            // Version 1 had only model-issued calls.
            let mut input = input.clone();
            if let Some(map) = input.as_object_mut() {
                map.insert("kind", "model");
            }
            Ok((input, checkpoint.clone()))
        })
        .phase("call", execute::call)
        .phase("execute", execute::execute)
        .abort(execute::abort);
    define_task(definition)
});

/// The built-in `pi.tool` task.
pub fn tool_task() -> Task {
    TOOL_TASK.clone()
}

pub(crate) fn decode_input(task: &TaskRecord) -> DurableResult<ToolTaskInput> {
    Ok(task.input.deserialize_into()?)
}

/// The called tool: among the tools offered to the model for a model-issued call, else among those tools call.
pub(crate) async fn resolve_tool(
    runtime: &TaskRuntime,
    input: &ToolTaskInput,
    name: &str,
    context: &Context,
) -> DurableResult<Option<Arc<ToolRegistration>>> {
    let agent = runtime.agent(context).await?;
    let tools = if matches!(input, ToolTaskInput::Model { .. }) { agent.tools } else { agent.callable };
    Ok(tools.into_iter().find(|tool| tool.name() == name))
}

/// The call: from the input for a nested call, else from the assistant entry.
pub(crate) async fn read_call(
    runtime: &TaskRuntime,
    input: &ToolTaskInput,
    context: &Context,
) -> DurableResult<ToolHookCall> {
    let call = match input {
        ToolTaskInput::Nested { call, parent, parent_call_id, .. } => {
            ToolHookCall { call: call.clone(), parent: Some((*parent, parent_call_id.clone())) }
        }
        ToolTaskInput::Model { assistant, call_id } => {
            let entry = runtime.typed_entry(&ASSISTANT_ENTRY, *assistant, context).await?;
            let model = entry.as_ref().and_then(|entry| entry.record.model.as_ref()).and_then(|model| model.first());
            let call = match model {
                Some(pi_ai::Message::Assistant(message)) => {
                    message.tool_calls().find(|call| call.id == *call_id).cloned()
                }
                _ => None,
            };
            let call =
                call.ok_or_else(|| DurableError::error(format!("Entry {assistant} has no tool call {call_id}")))?;
            ToolHookCall { call, parent: None }
        }
    };
    let attributes =
        SpanAttributes::new().with("tool.name", call.call.name.as_str()).with("tool.call_id", call.call.id.as_str());
    context.telemetry().set_attributes(attributes);
    Ok(call)
}

/// The call's arguments as repaired by the tool; a failing repair makes them invalid.
pub(crate) fn prepare(tool: &ToolRegistration, args: &JsonObject) -> Result<JsonObject, String> {
    let Some(prepare) = &tool.prepare_arguments else { return Ok(args.clone()) };
    match prepare(&Value::Object(args.clone())) {
        Ok(Value::Object(map)) => Ok(map),
        Ok(other) => Err(format!("Prepared arguments must be an object, got {other}")),
        Err(error) => Err(error.to_string()),
    }
}

/// Arguments validated and coerced against the implementation's schema.
pub(crate) fn validate(tool: &ToolRegistration, call: &ToolCall, args: JsonObject) -> Result<JsonObject, String> {
    let call = ToolCall { arguments: args, ..call.clone() };
    crate::util::validate_tool_arguments(&tool.tool, &call)
}

/// Structural JSON equality; object key order does not matter.
pub(crate) fn json_equal(a: &JsonObject, b: &JsonObject) -> bool {
    Json::from(Value::Object(a.clone())) == Json::from(Value::Object(b.clone()))
}
