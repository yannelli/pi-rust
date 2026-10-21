//! The loop's handlers: the VM's callbacks, replay of the journal, the release of live calls, and the journal pump.

use std::mem::take;

use pi_chord::{Context, Json};
use pi_codemode::js::js_stringify;
use pi_codemode::{CodemodeCallTarget, CodemodeHostCall, CodemodeOutcome};
use serde_json::{Map, Value};

use super::CodemodeGlobal;
use super::call::{CallResult, CodemodeCall, Done, Event, Finished, clock};
use super::docs::{CODEMODE_JOURNAL_DOC, CODEMODE_RUN_DOC, JournalEntry, JournalGlobal, StoredResult};
use super::outcome::{describe, outcome_json, outcome_of, same_json};
use crate::documents::encode;
use crate::errors::{DurableError, DurableResult};
use crate::harness::tool::{NESTED_CALL_DOC, NestedCallState, ToolTaskInput};
use crate::harness::{ExecuteToolOptions, ToolExecutionApi};
use crate::session::Tx;

impl CodemodeCall {
    pub(super) fn handle_event(&mut self, event: Event) {
        match event {
            Event::Started(fingerprint) => self.on_started(fingerprint),
            Event::Call(call) => self.on_call(call),
            Event::Idle(seq, issued) => self.on_idle(seq, issued),
        }
    }

    pub(super) fn handle_done(&mut self, done: Done) {
        match done {
            Done::Recorded(Ok(())) => self.open_gate(),
            Done::Recorded(Err(error)) | Done::Decided(Err(error)) => self.fail(error),
            Done::Decided(Ok(())) => self.cancel.cancel(),
            Done::Replayed(call, Ok(result)) => {
                let (outcome, _) = self.outcome(&call, result);
                self.ready.insert(call.id, outcome);
                self.deliver_replayed();
            }
            Done::Launched(call, Ok(result)) => {
                let (outcome, global) = self.outcome(&call, result);
                self.queue.push(Finished { id: call.id, at: clock(&self.api), outcome, global });
                self.pump();
            }
            Done::Replayed(call, Err(error)) | Done::Launched(call, Err(error)) => self.call_failed(&call, error),
            Done::Checked(calls, result) => self.checked(calls, result),
            Done::Pumped { result, batch, acks } => self.pumped(result, batch, acks),
        }
    }

    /// What the script gets for a call; a tool call's row gets its status.
    fn outcome(&mut self, call: &CodemodeHostCall, result: CallResult) -> (CodemodeOutcome, Option<JournalGlobal>) {
        match result {
            CallResult::Tool(result) => {
                let outcome = outcome_of(&call.name, &result);
                self.rows.finish(call.id, &outcome, result.duration_ms);
                (outcome, None)
            }
            CallResult::Rejected(outcome) => {
                self.rows.finish(call.id, &outcome, None);
                (outcome, None)
            }
            CallResult::Global(outcome) => {
                let global =
                    JournalGlobal { name: call.name.clone(), args: call.args.clone(), outcome: outcome_json(&outcome) };
                (outcome, Some(global))
            }
        }
    }

    /// A call that failed rather than resolved: the call is aborted, or its key names another tool or arguments.
    fn call_failed(&mut self, call: &CodemodeHostCall, error: DurableError) {
        if self.context.is_cancelled() {
            self.fail(error);
        } else {
            self.diverge(format!("call {} is {}: {error}", call.id, describe(call)));
        }
    }

    fn on_started(&mut self, fingerprint: String) {
        match self.start.fingerprint.clone() {
            Some(recorded) if recorded != fingerprint => {
                let message = format!(
                    "The script cannot resume: the sandbox changed since it started ({recorded} -> {fingerprint})."
                );
                self.end_with(StoredResult::error(message));
            }
            Some(_) => self.open_gate(),
            None => {
                let (api, context, task) = (self.api.clone(), self.context.clone(), self.api.task_id);
                let change = move |tx: Tx| async move {
                    let run = tx.doc(&CODEMODE_RUN_DOC, task).await?;
                    run.edit(|state| {
                        if let Some(start) = &mut state.start {
                            start.fingerprint = Some(fingerprint);
                        }
                    })
                };
                self.commits.push(Box::pin(async move { Done::Recorded(api.commit(change, &context).await) }));
            }
        }
    }

    /// Nothing was delivered before the gate: start now. A rerun without a journal releases its calls at the
    /// script's first acknowledgment instead.
    fn open_gate(&mut self) {
        self.gate_open = true;
        if self.replaying {
            self.deliver_replayed();
        } else if !self.rerun {
            self.release();
        }
        for call in take(&mut self.gated) {
            self.replay_tool(call);
        }
        self.deliver_replayed();
        if take(&mut self.release_requested) {
            self.release();
        }
    }

    fn on_call(&mut self, call: CodemodeHostCall) {
        // The VM made this call before it finished, so only the call's own end stops it.
        if self.ended.is_some() || self.failure.is_some() {
            return;
        }
        if call.target == CodemodeCallTarget::Tool {
            self.rows.started(&call);
        }
        self.issued = self.issued.max(call.id);
        let index = if self.replaying { self.replay_index.get(&call.id).copied() } else { None };
        let Some(index) = index else {
            if self.released {
                self.waiting.push(call);
                self.launch_covered();
            } else {
                self.held.push(call);
            }
            return;
        };
        if let Some(global) = &self.replayed[index].global {
            let same = call.target == CodemodeCallTarget::Global
                && call.name == global.name
                && same_json(call.args.as_ref(), global.args.as_ref());
            if !same {
                let message = format!("call {} is {}, but was a call to {}", call.id, describe(&call), global.name);
                self.diverge(message);
                return;
            }
            let outcome = super::outcome::outcome_from_json(&global.outcome);
            self.ready.insert(call.id, outcome);
            if self.gate_open {
                self.deliver_replayed();
            }
            return;
        }
        if call.target != CodemodeCallTarget::Tool {
            self.diverge(format!("call {} is {}, but was a tool call", call.id, describe(&call)));
            return;
        }
        if self.gate_open {
            self.replay_tool(call);
        } else {
            self.gated.push(call);
        }
    }

    /// Reattach a journaled tool call to its nested call; its result is delivered in journal order.
    fn replay_tool(&mut self, call: CodemodeHostCall) {
        let (api, context) = (self.api.clone(), self.context.clone());
        self.calls.push(Box::pin(async move {
            let result = call_tool(&api, &context, &call).await;
            Done::Replayed(call, result)
        }));
    }

    /// Replay: deliver journaled results in journal order, each once the VM has issued its call.
    pub(super) fn deliver_replayed(&mut self) {
        while self.next_replayed < self.replayed.len() && !self.stopped() {
            let entry = &self.replayed[self.next_replayed];
            if entry.id > self.issued {
                return;
            }
            let (id, at) = (entry.id, entry.at);
            let Some(outcome) = self.ready.remove(&id) else { return };
            self.next_replayed += 1;
            self.deliver(self.next_replayed as u64, at, id, outcome);
        }
    }

    fn on_idle(&mut self, seq: u64, issued: u64) {
        if self.stopped() {
            return;
        }
        // A count an earlier execution recorded must match; a missing one is recorded now.
        if self.rerun && seq <= self.replayed.len() as u64 {
            let recorded = match seq {
                0 => self.first_issued,
                _ => self.replayed[seq as usize - 1].issued_after,
            };
            if let Some(recorded) = recorded
                && recorded != issued
            {
                self.diverge(match seq {
                    0 => format!("its first run made {issued} calls, where it had made {recorded}"),
                    _ => format!("after result {seq} it had made {issued} calls, where it had made {recorded}"),
                });
                return;
            }
        }
        self.acks.insert(seq, issued);
        if !self.replaying {
            // A rerun without a journal: every call an earlier execution could have admitted is made by now.
            if seq == 0 && self.rerun {
                self.release();
            }
            // Committed at once when the script has processed everything delivered; otherwise with the next commit.
            if seq == self.journal_length {
                self.pump();
            }
            return;
        }
        // Caught up: the VM processed everything delivered and waits.
        if seq != self.next_replayed as u64 {
            return;
        }
        if let Some(next) = self.replayed.get(seq as usize) {
            if next.id > issued {
                let message = format!("it waits without making call {}, which it made before", next.id);
                self.diverge(message);
            }
            return;
        }
        // The last replayed result is acknowledged: continue live, admitting the calls the journal did not have.
        self.replaying = false;
        self.pump();
        self.release();
    }

    /// Let the held calls launch, and later live calls, each once its issued count is committed. On a rerun, an
    /// earlier execution may have admitted some of them under their keys, as other calls: all held calls are checked
    /// first, and a divergence launches none. Calls made after this are new.
    fn release(&mut self) {
        if !self.gate_open {
            self.release_requested = true;
            return;
        }
        if self.stopped() || self.released || self.releasing {
            return;
        }
        let calls = take(&mut self.held);
        if !self.rerun || calls.is_empty() {
            self.finish_release(calls);
            return;
        }
        self.releasing = true;
        let (api, context) = (self.api.clone(), self.context.clone());
        self.calls.push(Box::pin(async move {
            let result = first_other(&api, &context, &calls).await;
            Done::Checked(calls, result)
        }));
    }

    fn checked(&mut self, calls: Vec<CodemodeHostCall>, result: DurableResult<Option<String>>) {
        self.releasing = false;
        match result {
            Err(error) => self.fail(error),
            Ok(_) if self.stopped() => {}
            Ok(Some(message)) => self.diverge(message),
            Ok(None) => self.finish_release(calls),
        }
    }

    fn finish_release(&mut self, calls: Vec<CodemodeHostCall>) {
        self.released = true;
        self.waiting.extend(calls);
        let held = take(&mut self.held);
        self.waiting.extend(held);
        self.launch_covered();
    }

    /// Launch the waiting calls whose issued count is committed.
    fn launch_covered(&mut self) {
        if !self.released || self.stopped() {
            return;
        }
        let durable = self.durable_issued;
        let (covered, waiting): (Vec<_>, Vec<_>) =
            take(&mut self.waiting).into_iter().partition(|call| call.id <= durable);
        self.waiting = waiting;
        for call in covered {
            self.launch(call);
        }
    }

    /// Run one live call and journal its result.
    fn launch(&mut self, call: CodemodeHostCall) {
        if self.stopped() {
            return;
        }
        let (api, context) = (self.api.clone(), self.context.clone());
        let global = match call.target {
            CodemodeCallTarget::Global => self.options.globals.iter().find(|global| global.name == call.name).cloned(),
            CodemodeCallTarget::Tool => None,
        };
        self.calls.push(Box::pin(async move {
            let result = match call.target {
                CodemodeCallTarget::Tool => call_tool(&api, &context, &call).await,
                CodemodeCallTarget::Global => call_global(&api, &context, &call, global).await,
            };
            Done::Launched(call, result)
        }));
    }

    /// Journal finished calls and acknowledged counts in one commit, then deliver the calls and launch the calls the
    /// counts cover; one commit in flight at a time, so what arrives meanwhile goes into the next.
    pub(super) fn pump(&mut self) {
        if self.committing || self.stopped() || (self.queue.is_empty() && self.acks.is_empty()) {
            return;
        }
        self.committing = true;
        let batch = take(&mut self.queue);
        let acks: Vec<(u64, u64)> = take(&mut self.acks).into_iter().collect();
        let entries: Vec<JournalEntry> = batch
            .iter()
            .map(|item| JournalEntry { id: item.id, at: item.at, issued_after: None, global: item.global.clone() })
            .collect();
        let (api, context, task) = (self.api.clone(), self.context.clone(), self.api.task_id);
        let counts = acks.clone();
        let change = move |tx: Tx| async move {
            let journal = tx.doc(&CODEMODE_JOURNAL_DOC, task).await?;
            let entries = entries.iter().map(encode).collect::<DurableResult<Vec<Json>>>()?;
            journal.update(|value| record(value, &counts, entries))
        };
        self.commits.push(Box::pin(async move {
            let result = api.commit(change, &context).await;
            Done::Pumped { result, batch, acks }
        }));
    }

    fn pumped(&mut self, result: DurableResult<()>, batch: Vec<Finished>, acks: Vec<(u64, u64)>) {
        if let Err(error) = result {
            self.fail(error);
            return;
        }
        for (_, issued) in acks {
            self.durable_issued = self.durable_issued.max(issued);
        }
        for item in batch {
            self.journal_length += 1;
            self.deliver(self.journal_length, item.at, item.id, item.outcome);
        }
        self.launch_covered();
        self.committing = false;
        self.pump();
    }
}

/// Record acknowledged counts that are still absent, then append the entries; edits the journal in place.
fn record(journal: &mut Json, acks: &[(u64, u64)], entries: Vec<Json>) {
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

async fn call_tool(api: &ToolExecutionApi, context: &Context, call: &CodemodeHostCall) -> DurableResult<CallResult> {
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
    let options = ExecuteToolOptions { key: Some(format!("c{}", call.id)), ..ExecuteToolOptions::default() };
    Ok(CallResult::Tool(api.execute_tool(&call.name, args, context, options).await?))
}

async fn call_global(
    api: &ToolExecutionApi,
    context: &Context,
    call: &CodemodeHostCall,
    global: Option<CodemodeGlobal>,
) -> DurableResult<CallResult> {
    let Some(global) = global else {
        let message = format!("Unknown global {}", call.name);
        return Ok(CallResult::Global(CodemodeOutcome::Err { message, properties: None }));
    };
    // `None` stays undefined: the journal omits the value, and a replay resolves with undefined again.
    match (global.execute)(call.args.clone(), api.clone(), context.clone()).await {
        Ok(value) => Ok(CallResult::Global(CodemodeOutcome::Ok(value))),
        Err(error) if context.is_cancelled() => Err(error),
        Err(error) => Ok(CallResult::Global(CodemodeOutcome::Err { message: error.to_string(), properties: None })),
    }
}

/// The first held call an earlier execution made as another call, as its divergence message.
async fn first_other(
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
