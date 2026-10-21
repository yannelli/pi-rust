//! What runs a script's calls outside the loop: nested tool calls, with a slot for model calls, globals, the journal
//! edit, and the check of the calls an earlier execution made under the same keys.

use std::sync::Arc;

use pi_chord::{Context, Json};
use pi_codemode::js::js_stringify;
use pi_codemode::{CodemodeCallTarget, CodemodeHostCall, CodemodeModels, CodemodeOutcome, RunDeliverer};
use serde_json::{Map, Value};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use super::CodemodeGlobal;
use super::call::CallResult;
use super::models::{CATALOG_GLOBALS, CatalogError, catalog_call};
use super::outcome::{describe, same_json};
use crate::errors::{DurableError, DurableResult};
use crate::harness::tool::{NESTED_CALL_DOC, NestedCallState, ToolTaskInput};
use crate::harness::{ExecuteToolOptions, ToolExecutionApi};

/// Slots of `models.classify()` and `models.generateImages()` calls, and whether the call stopped meanwhile.
#[derive(Clone)]
pub(super) struct ModelSlots {
    pub slots: Arc<Semaphore>,
    /// Cancelled once the call decided an outcome or failed.
    pub halt: CancellationToken,
    pub run: Option<RunDeliverer>,
}

impl ModelSlots {
    fn stopped(&self) -> bool {
        self.halt.is_cancelled() || self.run.as_ref().is_some_and(RunDeliverer::finished)
    }
}

/// Run a nested tool call under key `c<id>`; a model call waits for a slot first.
pub(super) async fn call_tool(
    api: &ToolExecutionApi,
    context: &Context,
    call: &CodemodeHostCall,
    model: Option<ModelSlots>,
) -> DurableResult<CallResult> {
    let args = match &call.args {
        None => Map::new(),
        Some(Value::Object(args)) => args.clone(),
        // A nested call takes an object; validation would refuse anything else.
        Some(other) => {
            let message =
                format!("Validation failed for tool \"{}\": arguments must be an object, got {other}", call.name);
            return Ok(CallResult::Rejected(CodemodeOutcome::Err { message, properties: None }));
        }
    };
    let permit = match model {
        Some(model) => {
            let permit =
                model.slots.clone().acquire_owned().await.map_err(|error| DurableError::error(error.to_string()))?;
            // The call may have stopped while this one waited: a finished script admits no new paid call.
            if model.stopped() {
                return Err(DurableError::error("The script has ended"));
            }
            Some(permit)
        }
        None => None,
    };
    let options = ExecuteToolOptions { key: Some(format!("c{}", call.id)), ..ExecuteToolOptions::default() };
    let result = api.execute_tool(&call.name, args, context, options).await;
    drop(permit);
    Ok(CallResult::Tool(result?))
}

/// What a host global runs: a discovery global's answer, computed from the start data, a catalog call, or one of
/// the options' globals.
pub(super) enum GlobalCall {
    Answered(Result<Option<Value>, String>),
    Catalog(Arc<dyn CodemodeModels>),
    Option(CodemodeGlobal),
    Unknown,
}

pub(super) async fn call_global(
    api: &ToolExecutionApi,
    context: &Context,
    call: &CodemodeHostCall,
    global: GlobalCall,
) -> DurableResult<CallResult> {
    let failed = |message: String| Ok(CallResult::Global(CodemodeOutcome::Err { message, properties: None }));
    // `None` stays undefined: the journal omits the value, and a replay resolves with undefined again.
    let value = match global {
        GlobalCall::Unknown => return failed(format!("Unknown global {}", call.name)),
        GlobalCall::Answered(answer) => answer,
        GlobalCall::Catalog(models) if CATALOG_GLOBALS.contains(&call.name.as_str()) => {
            match catalog_call(&call.name, call.args.as_ref(), &*models, context).await {
                Ok(value) => Ok(value),
                Err(CatalogError::Aborted(error)) => return Err(error),
                Err(CatalogError::Script(message)) => Err(message),
            }
        }
        GlobalCall::Catalog(_) => return failed(format!("Unknown global {}", call.name)),
        GlobalCall::Option(global) => match (global.execute)(call.args.clone(), api.clone(), context.clone()).await {
            Ok(value) => Ok(value),
            Err(error) if context.is_cancelled() => return Err(error),
            Err(error) => Err(error.to_string()),
        },
    };
    match value {
        Ok(value) => Ok(CallResult::Global(CodemodeOutcome::Ok(value))),
        Err(message) if context.is_cancelled() => Err(DurableError::error(message)),
        Err(message) => failed(message),
    }
}

/// Record acknowledged counts that are still absent, then append the entries; edits the journal in place.
pub(super) fn record(journal: &mut Json, acks: &[(u64, u64)], entries: Vec<Json>) {
    let Some(state) = journal.as_object_mut() else { return };
    for &(seq, issued) in acks {
        if seq == 0 {
            if !state.contains_key("firstIssued") {
                state.insert("firstIssued", issued as f64);
            }
            continue;
        }
        let entry = state
            .get_mut("entries")
            .and_then(Json::as_array_mut)
            .and_then(|entries| entries.get_mut(seq as usize - 1))
            .and_then(Json::as_object_mut);
        if let Some(entry) = entry
            && !entry.contains_key("issuedAfter")
        {
            entry.insert("issuedAfter", issued as f64);
        }
    }
    if let Some(list) = state.get_mut("entries").and_then(Json::as_array_mut) {
        list.extend(entries);
    }
}

/// The first held call an earlier execution made as another call, as its divergence message.
pub(super) async fn first_other(
    api: &ToolExecutionApi,
    context: &Context,
    calls: &[CodemodeHostCall],
) -> DurableResult<Option<String>> {
    for call in calls {
        if let Some(other) = earlier(api, context, call).await? {
            return Ok(Some(format!("call {} is {}, but was {other}", call.id, describe(call))));
        }
    }
    Ok(None)
}

/// The call an earlier execution made under this call's key, when it is another one: another target, tool, or
/// arguments.
async fn earlier(api: &ToolExecutionApi, context: &Context, call: &CodemodeHostCall) -> DurableResult<Option<String>> {
    let key = format!("c{}", call.id);
    let Some(stored) = api.family_snapshot(&NESTED_CALL_DOC, api.task_id, &key, context).await? else {
        return Ok(None);
    };
    let Some(task_id) = stored.deserialize_into::<NestedCallState>()?.task_id else { return Ok(None) };
    let Some(record) = api.get_task(task_id, context).await? else { return Ok(None) };
    let Ok(ToolTaskInput::Nested { call: made, .. }) = record.input.deserialize_into::<ToolTaskInput>() else {
        return Ok(None);
    };
    let made_args = Value::Object(made.arguments);
    let args = call.args.clone().unwrap_or_else(|| Value::Object(Map::new()));
    if call.target == CodemodeCallTarget::Tool && made.name == call.name && same_json(Some(&made_args), Some(&args)) {
        return Ok(None);
    }
    Ok(Some(format!("tools.{}({})", made.name, js_stringify(&made_args))))
}
