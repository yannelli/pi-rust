//! Experimental agent events (spec section 9.4, upstream `harness/events.ts`): one conversation's view
//! publications translated into events shaped like the coding agent's session events.

mod changes;

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::{Arc, Mutex};

use pi_chord::delta::Op;
use pi_chord::watch::Replace;
use pi_chord::{Context, Json, NativeWatch, NativeWatchChange};

use self::changes::{call_of, message_changes, object, same, tool_update};
use super::host::Harness;
use super::types::AgentState;
use super::usage::UsageState;
use super::util::{cancellation, lock, scan_all};
use super::view::ViewObserver;
use crate::documents::encode;
use crate::errors::{DurableError, DurableResult};
use crate::ids::{ConversationId, TaskId};
use crate::session::{ObservedCommit, Release, WatchEnd};
use crate::types::{CommitChange, DocumentCommitChange, TaskQuery, TaskRecord, TaskState, TaskStatus};

/// One agent event as JSON with its upstream `type` (`snapshot`, `run_start`, `message_update`,
/// `tool_execution_end`, and so on) and camelCase fields, as upstream `AgentEvent` serializes.
pub type AgentEvent = Json;

/// Serialized stream of one conversation's event batches, one per commit.
pub struct AgentEventStream {
    /// The `snapshot` event at attachment.
    pub snapshot: AgentEvent,
    watch: NativeWatch<Vec<AgentEvent>, DurableError>,
}

impl AgentEventStream {
    /// Install the sole listener; each call's `value` is one commit's batch, or a `snapshot` after an overflow.
    pub fn start<F, Fut>(&self, listener: F) -> DurableResult<()>
    where
        F: Fn(NativeWatchChange<Vec<AgentEvent>>, Context) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = DurableResult<()>> + Send + 'static,
    {
        self.watch.start(listener)
    }

    pub fn stop(&self) -> impl Future<Output = WatchEnd> + Send + use<> {
        self.watch.stop()
    }

    pub fn closed(&self) -> impl Future<Output = WatchEnd> + Send + use<> {
        self.watch.closed()
    }
}

/// The `{ live, inbox, agent, usage }` parts of a view the events read.
struct Parts<'a> {
    live: &'a Json,
    inbox: Option<&'a Json>,
    agent: Option<&'a Json>,
    usage: Option<&'a Json>,
}

static EMPTY: std::sync::LazyLock<Json> = std::sync::LazyLock::new(|| object([]));

fn parts(view: &Json) -> Parts<'_> {
    let docs = &view["docs"];
    Parts {
        live: docs.get("pi.live").unwrap_or(&EMPTY),
        inbox: docs.get("pi.inbox"),
        agent: docs.get("pi.agent"),
        usage: docs.get("pi.usage"),
    }
}

fn list(value: Option<&Json>) -> Vec<Json> {
    value.and_then(Json::as_array).cloned().unwrap_or_default()
}

fn queued(inbox: Option<&Json>) -> Json {
    let items = list(inbox.map(|inbox| &inbox["items"]));
    Json::array(items.iter().map(|item| object([("id", item["id"].clone()), ("mode", item["mode"].clone())])).collect())
}

fn agent_or_initial(agent: Option<&Json>) -> Json {
    agent.cloned().unwrap_or_else(|| encode(&AgentState::default()).unwrap_or(Json::Null))
}

fn usage_or_initial(usage: Option<&Json>) -> Json {
    usage.cloned().unwrap_or_else(|| encode(&UsageState::default()).unwrap_or(Json::Null))
}

fn snapshot_of(view: &Json) -> AgentEvent {
    let Parts { live, inbox, agent, usage } = parts(view);
    let mut fields = vec![("type", Json::from("snapshot")), ("entries", view["entries"].clone())];
    if let Some(run) = live.get("run") {
        fields.push(("run", object([("inputs", run["inputs"].clone())])));
    }
    if let Some(generation) = live.get("generation") {
        fields.push(("generation", generation.clone()));
    }
    fields.extend([
        ("tools", Json::array(list(live.get("tools")))),
        ("nestedTools", Json::array(list(live.get("nestedTools")))),
        ("compactions", Json::array(list(live.get("compactions")))),
    ]);
    if let Some(failure) = live.get("compactionFailure") {
        fields.push(("compactionFailure", failure.clone()));
    }
    fields.extend([("inbox", queued(inbox)), ("agent", agent_or_initial(agent)), ("usage", usage_or_initial(usage))]);
    object(fields)
}

/// The view observer of one stream: translates each publication into a batch for its watch.
struct EventsObserver {
    conversation_id: ConversationId,
    watch: NativeWatch<Vec<AgentEvent>, DurableError>,
    current: Arc<Mutex<Json>>,
    /// Generations whose held outcome already ended their turn.
    held: Mutex<HashSet<TaskId>>,
}

impl ViewObserver for EventsObserver {
    fn publication(&self, before: &Json, after: &Json, ops: &[Op], publication: &ObservedCommit, context: &Context) {
        *lock(&self.current) = after.clone();
        let events = translate(self.conversation_id, before, after, ops, publication, &mut lock(&self.held));
        if !events.is_empty() {
            self.watch.advance(events, context.clone());
        }
    }

    fn end(&self, failure: Option<DurableError>) {
        self.watch.end(failure);
    }
}

/// Experimental: attach to one conversation's agent events. The snapshot and the registration for later commits are
/// captured atomically on the Session line; overflow replaces undelivered batches with one snapshot.
pub async fn watch_events(
    harness: &Harness,
    conversation_id: ConversationId,
    context: &Context,
) -> DurableResult<AgentEventStream> {
    let session = harness.session().clone();
    let prepare = move |storage: Arc<dyn crate::types::Storage>, context: Context| async move {
        // Generations whose held outcome already ended their turn, read on the line with the snapshot.
        let query = TaskQuery {
            conversation_id: Some(conversation_id),
            kind: Some("pi.generation".into()),
            status: Some(TaskStatus::Completing),
            ..TaskQuery::default()
        };
        let (storage, query, context) = (&storage, &query, &context);
        let completing =
            scan_all(|cursor| async move { storage.scan_tasks(query, 100, cursor.as_ref(), context).await }).await?;
        Ok(completing.iter().map(|record| record.id).collect::<HashSet<_>>())
    };
    let create = move |initial: &Json, release: Arc<Release>, held| {
        let current = Arc::new(Mutex::new(initial.clone()));
        let latest = current.clone();
        // Batches are the watch's values, each delivered (folding would drop events); an overflow delivers a snapshot of
        // the newest view instead.
        let replace: Replace<Vec<AgentEvent>> = Arc::new(move || vec![snapshot_of(&lock(&latest))]);
        let report = move |error: &DurableError| session.report(error);
        let watch = NativeWatch::new(Vec::new(), move || release.run(), report, Some(replace));
        let observer = EventsObserver { conversation_id, watch: watch.clone(), current, held: Mutex::new(held) };
        (Arc::new(observer) as Arc<dyn ViewObserver>, (watch, snapshot_of(initial)))
    };
    let ((watch, snapshot), _) = harness.0.views.attach_with(conversation_id, context, prepare, create).await?;
    // Like a watch, the acquisition context governs the stream's lifetime.
    if context.is_cancelled() {
        watch.cancel();
        return Err(cancellation(context));
    }
    if let Some(signal) = context.abort_signal() {
        watch.observe_cancellation(&signal)?;
    }
    Ok(AgentEventStream { snapshot, watch })
}

fn event<'a>(kind: &str, fields: impl IntoIterator<Item = (&'a str, Json)>) -> Json {
    object([("type", Json::from(kind))].into_iter().chain(fields))
}

/// The tool result entry for `call_id` among `entries`.
fn result_of<'a>(entries: &'a [Json], call_id: &Json) -> Option<&'a Json> {
    entries.iter().find(|entry| {
        let message = &entry["model"][0];
        message["role"] == Json::from("toolResult") && &message["toolCallId"] == call_id
    })
}

fn task_id(value: &Json) -> Option<TaskId> {
    value.as_u64().map(TaskId)
}

/// Every event one publication causes, in the order of spec section 9.4.
fn translate(
    conversation_id: ConversationId,
    before: &Json,
    after: &Json,
    view_ops: &[Op],
    publication: &ObservedCommit,
    held: &mut HashSet<TaskId>,
) -> Vec<AgentEvent> {
    let mut entries = Vec::new();
    let mut tasks: Vec<&TaskRecord> = Vec::new();
    let mut submissions = Vec::new();
    // Nested results this commit stored, by nested task ID; read now, since the caller's documents may retire next.
    let mut nested_results: HashMap<u64, Json> = HashMap::new();
    for change in publication.changes() {
        match change {
            CommitChange::Document(DocumentCommitChange::Document {
                record,
                conversation_id: owner,
                value: Some(value),
                ..
            }) if record.kind == "pi.tool.nested-call" && *owner == Some(conversation_id) => {
                if let (Some(task_id), result) = (value["taskId"].as_u64(), &value["result"])
                    && !result.is_null()
                {
                    nested_results.insert(task_id, result.clone());
                }
            }
            CommitChange::Entry(entry) if entry.conversation_id == conversation_id => {
                entries.push(encode(entry).unwrap_or(Json::Null));
            }
            CommitChange::Task(task) if task.conversation_id == conversation_id => {
                match tasks.iter_mut().find(|other| other.id == task.id) {
                    Some(slot) => *slot = task,
                    None => tasks.push(task),
                }
            }
            CommitChange::Submission(record) if record.conversation_id == conversation_id => submissions.push(record),
            _ => {}
        }
    }
    if view_ops.is_empty() && entries.is_empty() && tasks.is_empty() && submissions.is_empty() {
        return Vec::new();
    }
    // Entries are appended in ID order; submission records are published in the order the commit first touched them.
    submissions.sort_by_key(|record| record.id);
    let task = |id: Option<TaskId>| id.and_then(|id| tasks.iter().find(|task| task.id == id).copied());
    let (was, now) = (parts(before), parts(after));
    let mut events = Vec::new();

    // Progress: tool starts, the in-flight message, tool updates, retry and deferred state.
    let slots_before = list(was.live.get("tools"));
    let slots = list(now.live.get("tools"));
    let nested_before = list(was.live.get("nestedTools"));
    let nested = list(now.live.get("nestedTools"));
    let find = |list: &[Json], key: &str, value: &Json| list.iter().find(|slot| &slot[key] == value).cloned();
    let previous_of = |slot: &Json| {
        if slot.get("parentCallId").is_some() {
            find(&nested_before, "taskId", &slot["taskId"])
        } else {
            find(&slots_before, "callId", &slot["callId"])
        }
    };
    let running = Json::from("running");
    let done = Json::from("done");
    for slot in slots.iter().chain(&nested) {
        let previous = previous_of(slot);
        if slot["status"] != running || previous.is_some_and(|previous| previous["status"] == running) {
            continue;
        }
        // A nested slot carries the arguments the call runs with; a model-issued call's are in its intent checkpoint.
        let args = if slot.get("parentCallId").is_some() {
            slot["arguments"].clone()
        } else {
            let checkpoint = task(task_id(&slot["taskId"])).and_then(|task| task.state.checkpoint().cloned());
            checkpoint.and_then(|checkpoint| checkpoint.get("arguments").cloned()).unwrap_or_else(|| object([]))
        };
        events.push(event("tool_execution_start", call_of(slot).into_iter().chain([("args", args)])));
    }
    let generation_before = was.live.get("generation");
    let generation = now.live.get("generation");
    let partial_before = generation_before.and_then(|generation| generation.get("message"));
    let partial = generation.and_then(|generation| generation.get("message"));
    if let Some(partial) = partial {
        if partial_before.is_none() {
            events.push(event("message_start", [("message", partial.clone())]));
        } else if !same(Some(partial), partial_before) {
            let changes = Json::array(message_changes(view_ops, partial));
            events.push(event("message_update", [("usage", partial["usage"].clone()), ("changes", changes)]));
        }
    }
    for (at, list) in [("tools", &slots), ("nestedTools", &nested)] {
        for (index, slot) in list.iter().enumerate() {
            let Some(previous) = previous_of(slot) else { continue };
            if slot["status"] != running || previous["status"] != running {
                continue;
            }
            let Some(update) = tool_update(view_ops, (at, index), slot, &previous) else { continue };
            events.push(event("tool_execution_update", call_of(slot).into_iter().chain(update)));
        }
    }
    let retry_before = generation_before.and_then(|generation| generation.get("retry"));
    let retry = generation.and_then(|generation| generation.get("retry"));
    if let (Some(retry), None) = (retry, retry_before) {
        let attempt = generation.map(|generation| generation["attempt"].clone()).unwrap_or(Json::Null);
        let fields = [("attempt", attempt), ("at", retry["at"].clone()), ("errorMessage", retry["error"].clone())];
        events.push(event("auto_retry_start", fields));
    }
    if let (Some(_), None) = (retry_before, retry) {
        let attempt = generation_before.map(|generation| generation["attempt"].clone()).unwrap_or(Json::Null);
        events.push(event("auto_retry_end", [("attempt", attempt)]));
    }
    if let Some(deferred) = generation.and_then(|generation| generation.get("deferred")) {
        let before =
            generation_before.and_then(|generation| generation.get("deferred")).map(|deferred| &deferred["pollAt"]);
        if Some(&deferred["pollAt"]) != before {
            events.push(event("deferred_poll", [("pollAt", deferred["pollAt"].clone())]));
        }
    }

    // Tools that end in this commit: a slot that becomes done, one created done (a call not offered), or an unfinished
    // one that vanishes because its run ended. A done slot that vanishes ended earlier.
    let mut tool_ends: Vec<(Json, Option<Json>)> = Vec::new();
    let mut end_tool = |slot: &Json, entry_id: Option<&Json>| {
        let entry = entry_id.and_then(|id| entries.iter().find(|entry| &entry["id"] == id)).cloned();
        let mut fields = call_of(slot);
        if let Some(entry) = &entry {
            fields.push(("entry", entry.clone()));
        }
        tool_ends.push((event("tool_execution_end", fields), entry.map(|entry| entry["id"].clone())));
    };
    for previous in &slots_before {
        if previous["status"] == done {
            continue;
        }
        match find(&slots, "callId", &previous["callId"]) {
            Some(slot) if slot["status"] == done => end_tool(&slot, slot.get("entry")),
            // A slot whose run ended in this commit may have had its result appended with it, as for unstarted calls.
            None => end_tool(previous, result_of(&entries, &previous["callId"]).map(|entry| &entry["id"])),
            Some(_) => {}
        }
    }
    for slot in &slots {
        if slot["status"] == done && find(&slots_before, "callId", &slot["callId"]).is_none() {
            end_tool(slot, slot.get("entry"));
        }
    }
    // Nested calls that end in this commit, the same way, children before the calls that made them: the lists hold
    // parents first, so walk them backwards.
    let mut end_nested = |slot: &Json| {
        let mut fields = call_of(slot);
        if let Some(result) = slot["taskId"].as_u64().and_then(|id| nested_results.get(&id)) {
            fields.push(("result", result.clone()));
        }
        events.push(event("tool_execution_end", fields));
    };
    for previous in nested_before.iter().rev() {
        if previous["status"] == done {
            continue;
        }
        match find(&nested, "taskId", &previous["taskId"]) {
            Some(slot) if slot["status"] == done => end_nested(&slot),
            None => end_nested(previous),
            Some(_) => {}
        }
    }
    for slot in nested.iter().rev() {
        if slot["status"] == done && find(&nested_before, "taskId", &slot["taskId"]).is_none() {
            end_nested(slot);
        }
    }

    // Entries in append order; a tool's end directly precedes its result's message, as in the coding agent.
    let mut assistant_appended = false;
    for entry in &entries {
        let id = &entry["id"];
        events.extend(tool_ends.iter().filter(|(_, at)| at.as_ref() == Some(id)).map(|(end, _)| end.clone()));
        let Some(message) = entry["model"].as_array().and_then(|model| model.first()) else {
            events.push(event("entry_appended", [("entry", entry.clone())]));
            continue;
        };
        // A streamed answer already started with its first partial.
        let assistant = message["role"] == Json::from("assistant");
        let streamed = assistant && partial_before.is_some() && !assistant_appended;
        assistant_appended |= assistant;
        if !streamed {
            events.push(event("message_start", [("message", message.clone())]));
        }
        events.push(event("message_end", [("entry", entry.clone())]));
    }
    // Ends without a result entry: a faulted or orphaned tool, or one whose run ended.
    events.extend(tool_ends.iter().filter(|(_, at)| at.is_none()).map(|(end, _)| end.clone()));

    // Compaction ends, task failures, then turn and run ends.
    let compactions_before = list(was.live.get("compactions"));
    let compactions = list(now.live.get("compactions"));
    let has = |list: &[Json], status: &Json| list.iter().any(|other| other["taskId"] == status["taskId"]);
    let failure = now.live.get("compactionFailure");
    for status in &compactions_before {
        if !has(&compactions, status) {
            let mut fields = vec![("taskId", status["taskId"].clone()), ("reason", status["reason"].clone())];
            if let Some(failure) = failure.filter(|failure| failure["taskId"] == status["taskId"]) {
                fields.push(("error", failure["error"].clone()));
            }
            events.push(event("compaction_end", fields));
        }
    }
    // A generation's turn ends when its outcome is committed: at a `completing` hold or at terminal, whichever comes
    // first, so a successor created at the hold starts after it.
    let mut turn_ended = false;
    for task in &tasks {
        let generation = task.kind == "pi.generation";
        if generation && matches!(task.state, TaskState::Completing { .. }) && held.insert(task.id) {
            turn_ended = true;
        }
        let TaskState::Terminal { outcome } = &task.state else { continue };
        if generation && !held.remove(&task.id) {
            turn_ended = true;
        }
        let outcome = encode(outcome).unwrap_or(Json::Null);
        let message = match outcome["status"].as_str() {
            Some("faulted") => outcome["error"]["message"].clone(),
            Some("orphaned") => outcome["reason"].clone(),
            _ => continue,
        };
        let fields =
            [("taskId", Json::from(task.id.0)), ("kind", Json::from(task.kind.as_str())), ("message", message)];
        events.push(event("task_failed", fields));
    }
    if turn_ended {
        events.push(event("turn_end", []));
    }
    let run = now.live.get("run");
    let run_before = was.live.get("run");
    let first_input = |run: Option<&Json>| run.map(|run| run["inputs"][0].clone()).filter(|input| !input.is_null());
    let run_changed = first_input(run) != first_input(run_before);
    if let Some(run_before) = run_before.filter(|_| run_changed) {
        events.push(event("run_end", [("inputs", run_before["inputs"].clone())]));
    }

    // Submissions, document state, then what began.
    for record in submissions {
        events.push(event("submission", [("record", encode(record).unwrap_or(Json::Null))]));
    }
    if !same(now.inbox, was.inbox) {
        events.push(event("inbox_update", [("items", queued(now.inbox))]));
    }
    // A retired document reads as its starting value, as in a snapshot.
    if !same(now.agent, was.agent) {
        events.push(event("agent_changed", [("agent", agent_or_initial(now.agent))]));
    }
    if !same(now.usage, was.usage) {
        events.push(event("usage_changed", [("usage", usage_or_initial(now.usage))]));
    }
    for status in &compactions {
        if !has(&compactions_before, status) {
            let fields = [
                ("taskId", status["taskId"].clone()),
                ("reason", status["reason"].clone()),
                ("blocking", status["blocking"].clone()),
            ];
            events.push(event("compaction_start", fields));
        }
    }
    if let Some(run) = run.filter(|_| run_changed) {
        events.push(event("run_start", [("inputs", run["inputs"].clone())]));
    }
    if let Some(run) = run {
        let started = run_before.is_none_or(|before| before["taskId"] != run["taskId"]);
        if started && task(task_id(&run["taskId"])).is_some_and(|task| task.kind == "pi.generation") {
            events.push(event("turn_start", []));
        }
    }
    events
}
