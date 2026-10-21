//! The loop's handlers: the VM's callbacks, replay of the journal, the release of live calls, and the journal pump.

use std::mem::take;

use pi_chord::Json;
use pi_codemode::{CodemodeCallTarget, CodemodeHostCall, CodemodeOutcome};

use super::call::{CallResult, CodemodeCall, Done, Event, Finished, clock};
use super::calls::{GlobalCall, ModelSlots, call_global, call_tool, first_other, record};
use super::discover::{DISCOVERY_GLOBALS, as_tool_call, discover};
use super::docs::{CODEMODE_JOURNAL_DOC, CODEMODE_RUN_DOC, JournalEntry, JournalGlobal, StoredResult};
use super::models::{CATALOG_GLOBALS, CODEMODE_INTERNAL_PREFIX, GENERATE_IMAGES_TOOL, count_images};
use super::outcome::{describe, error_diagnostics, outcome_json, outcome_of, same_json};
use crate::documents::encode;
use crate::errors::{DurableError, DurableResult};
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
                if call.name == GENERATE_IMAGES_TOOL {
                    self.generated_images += count_images(result.structured_output.as_ref());
                }
                let tool = self.start.tools.iter().find(|tool| tool.name == call.name);
                let outcome = outcome_of(&call.name, tool.is_some_and(|tool| tool.structured), &result);
                let error = match &outcome {
                    CodemodeOutcome::Err { message, .. } => message.clone(),
                    CodemodeOutcome::Ok(_) => error_diagnostics(&result),
                };
                self.rows.finish(call.id, result.is_error, &error, result.duration_ms);
                (outcome, None)
            }
            CallResult::Rejected(outcome) => {
                if let CodemodeOutcome::Err { message, .. } = &outcome {
                    self.rows.finish(call.id, true, message, None);
                }
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
    /// Stopped meanwhile, nothing changes: the outcome is decided, or a waiting model call gave up.
    fn call_failed(&mut self, call: &CodemodeHostCall, error: DurableError) {
        if self.stopped() {
            return;
        }
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

    fn on_call(&mut self, made: CodemodeHostCall) {
        // The VM made this call before it finished, so only the call's own end stops it.
        if self.ended.is_some() || self.failure.is_some() {
            return;
        }
        let call = as_tool_call(made);
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
        let (api, context, slots) = (self.api.clone(), self.context.clone(), self.model_slots(&call));
        self.calls.push(Box::pin(async move {
            let result = call_tool(&api, &context, &call, slots).await;
            Done::Replayed(call, result)
        }));
    }

    /// A model call's slots; `None` for other calls.
    fn model_slots(&self, call: &CodemodeHostCall) -> Option<ModelSlots> {
        call.name.starts_with(CODEMODE_INTERNAL_PREFIX).then(|| ModelSlots {
            slots: self.model_slots.clone(),
            halt: self.halt.clone(),
            run: self.run.clone(),
        })
    }

    /// What a global call runs; a discovery global is answered now, from the start data.
    fn global_call(&self, call: &CodemodeHostCall) -> GlobalCall {
        let name = call.name.as_str();
        if DISCOVERY_GLOBALS.contains(&name) {
            return GlobalCall::Answered(discover(&self.start, call));
        }
        if CATALOG_GLOBALS.contains(&name) {
            return self.options.models.clone().map_or(GlobalCall::Unknown, GlobalCall::Catalog);
        }
        let global = self.options.globals.iter().find(|global| global.name == name);
        global.cloned().map_or(GlobalCall::Unknown, GlobalCall::Option)
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
        let (slots, global) = match call.target {
            CodemodeCallTarget::Tool => (self.model_slots(&call), None),
            CodemodeCallTarget::Global => (None, Some(self.global_call(&call))),
        };
        self.calls.push(Box::pin(async move {
            let result = match global {
                None => call_tool(&api, &context, &call, slots).await,
                Some(global) => call_global(&api, &context, &call, global).await,
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
