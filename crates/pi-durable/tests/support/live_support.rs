//! What tests of renderers' state read: every `pi.live` value of a conversation, and the results of nested calls
//! (upstream `test/live-support.ts`).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use pi_chord::Context;
use pi_durable::errors::DurableResult;
use pi_durable::harness::Harness;
use pi_durable::session::{Observation, Observer, WatchCall, WatchEnd};
use pi_durable::types::{CommitChange, DocumentCommitChange};
use pi_durable::{ConversationId, TaskId};
use serde_json::{Value, json};

use super::session_support::{context as ctx, follow_commits};

/// Records every value an observer gets, unfolded.
struct Recorder(Arc<Mutex<Vec<Value>>>);

impl Observer for Recorder {
    fn advance(&self, call: &WatchCall, _context: &Context) -> DurableResult<()> {
        if let WatchCall::Value { value, .. } = call {
            self.0.lock().unwrap().push(value.to_serde());
        }
        Ok(())
    }

    fn end(&self, _end: WatchEnd) {}
}

/// Every value of a view or task graph from attachment on, one per commit that changes it.
pub struct History {
    values: Arc<Mutex<Vec<Value>>>,
    observation: Observation,
}

impl History {
    async fn attach(attach: impl AsyncFnOnce(Arc<dyn Observer>) -> Observation) -> Self {
        let values: Arc<Mutex<Vec<Value>>> = Arc::default();
        let observation = attach(Arc::new(Recorder(values.clone()))).await;
        values.lock().unwrap().insert(0, observation.value.to_serde());
        Self { values, observation }
    }

    /// Every value so far, the attachment's first.
    pub fn views(&self) -> Vec<Value> {
        self.values.lock().unwrap().clone()
    }

    /// The `pi.live` value of every view, `{}` where it is absent.
    pub fn live(&self) -> Vec<Value> {
        self.views().iter().map(|view| view["docs"].get("pi.live").cloned().unwrap_or_else(|| json!({}))).collect()
    }

    pub fn detach(&self) {
        self.observation.detach();
    }
}

/// Every view of `id` from now on, unfolded: a synchronous observer, so no transition is skipped.
pub async fn view_history(harness: &Harness, id: ConversationId) -> History {
    History::attach(async |observer| harness.observe_conversation(id, observer, &ctx(), None).await.unwrap()).await
}

/// Every task graph from now on, one per change, unfolded.
pub async fn graph_history(harness: &Harness) -> History {
    History::attach(async |observer| harness.observe_task_graph(observer, &ctx()).await.unwrap()).await
}

/// The live `pi.generation` nodes of `graph`, in task ID order.
pub fn generations(graph: &Value) -> Vec<Value> {
    let tasks = graph["tasks"].as_object().cloned().unwrap_or_default();
    let mut nodes: Vec<Value> = tasks.into_values().filter(|node| node["kind"] == "pi.generation").collect();
    nodes.sort_by_key(|node| node["id"].as_u64());
    nodes
}

/// The slots of tool call `call_id`, model-issued or nested, in each `pi.live` value; `None` where it is absent.
pub fn slots_of(history: &[Value], call_id: &str) -> Vec<Option<Value>> {
    let slots = |live: &Value| -> Vec<Value> {
        let tools = live["tools"].as_array().cloned().unwrap_or_default();
        tools.into_iter().chain(live["nestedTools"].as_array().cloned().unwrap_or_default()).collect()
    };
    history.iter().map(|live| slots(live).into_iter().find(|slot| slot["callId"] == call_id)).collect()
}

/// The index of the first value where call `call_id` ended: its slot is done, or gone after it was listed.
pub fn ended_at(history: &[Value], call_id: &str) -> Option<usize> {
    let slots = slots_of(history, call_id);
    let listed = slots.iter().position(Option::is_some)?;
    let ended = |slot: &Option<Value>| slot.as_ref().is_none_or(|slot| slot["status"] == "done");
    slots.iter().enumerate().position(|(index, slot)| index >= listed && ended(slot))
}

/// The calls whose slot went `running`, in the order they started.
pub fn start_order(history: &[Value]) -> Vec<String> {
    let mut started: Vec<String> = Vec::new();
    for live in history {
        let tools = live["tools"].as_array().into_iter().flatten();
        for slot in tools.chain(live["nestedTools"].as_array().into_iter().flatten()) {
            let call = slot["callId"].as_str().unwrap_or_default().to_owned();
            if slot["status"] == "running" && !started.contains(&call) {
                started.push(call);
            }
        }
    }
    started
}

pub type NestedResults = Arc<Mutex<HashMap<TaskId, Value>>>;

/// The result of every nested call from now on, by its tool task, read from the nested-call document its terminal
/// commit writes (spec section 7.3), the place a client reads a live caller's nested results from.
pub fn nested_results(harness: &Harness) -> NestedResults {
    let results = NestedResults::default();
    let sink = results.clone();
    let unsubscribe = follow_commits(harness, move |publication, _| {
        for change in &publication.changes {
            let CommitChange::Document(DocumentCommitChange::Change { record, value, .. }) = change else {
                continue;
            };
            let state = value.to_serde();
            if record.kind == "pi.tool.nested-call"
                && let (Some(task), Some(result)) = (state["taskId"].as_u64(), state.get("result"))
            {
                sink.lock().unwrap().insert(TaskId(task), result.clone());
            }
        }
        Ok(())
    })
    .unwrap();
    std::mem::forget(unsubscribe);
    results
}

/// The result of nested call `call_id`, found by the task its slot names.
pub fn nested_result_of(history: &[Value], results: &NestedResults, call_id: &str) -> Option<Value> {
    let slot = slots_of(history, call_id).into_iter().flatten().find(|slot| slot["taskId"].is_u64())?;
    results.lock().unwrap().get(&TaskId(slot["taskId"].as_u64()?)).cloned()
}
