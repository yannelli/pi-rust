//! The state of one execution of the code mode tool, and what changes it from more than one place.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use futures::future::BoxFuture;
use futures::stream::FuturesUnordered;
use pi_chord::Context;
use pi_codemode::{CodemodeHostCall, CodemodeOutcome, RunDeliverer};
use tokio_util::sync::CancellationToken;

use super::CodemodeToolOptions;
use super::details::Rows;
use super::docs::{CODEMODE_RUN_DOC, CodemodeStart, JournalEntry, JournalGlobal, JournalState, StoredResult};
use crate::errors::{DurableError, DurableResult};
use crate::harness::{NestedToolExecutionResult, ToolExecutionApi, ToolExecutionResult};
use crate::session::Tx;

/// What the VM reports, in the order it reports it.
pub(super) enum Event {
    Started(String),
    Call(CodemodeHostCall),
    Idle(u64, u64),
}

/// A finished live call waiting for its journal commit.
pub(super) struct Finished {
    pub id: u64,
    pub at: f64,
    pub outcome: CodemodeOutcome,
    pub global: Option<JournalGlobal>,
}

/// What a call's future produced: a nested call's result, a global's outcome, or a call refused before it ran.
pub(super) enum CallResult {
    Tool(NestedToolExecutionResult),
    Global(CodemodeOutcome),
    Rejected(CodemodeOutcome),
}

/// A future of the loop that finished.
pub(super) enum Done {
    Recorded(DurableResult<()>),
    Decided(DurableResult<()>),
    Replayed(CodemodeHostCall, DurableResult<CallResult>),
    Launched(CodemodeHostCall, DurableResult<CallResult>),
    Checked(Vec<CodemodeHostCall>, DurableResult<Option<String>>),
    Pumped { result: DurableResult<()>, batch: Vec<Finished>, acks: Vec<(u64, u64)> },
}

pub(super) type Pending = FuturesUnordered<BoxFuture<'static, Done>>;

pub(super) struct CodemodeCall {
    pub options: Arc<CodemodeToolOptions>,
    pub api: ToolExecutionApi,
    pub context: Context,
    /// Ends the VM with an error the script cannot catch.
    pub cancel: CancellationToken,
    pub run: Option<RunDeliverer>,
    pub start: CodemodeStart,
    /// The journal as this execution found it; replay delivers exactly these.
    pub replayed: Vec<JournalEntry>,
    /// Entries in the journal, replayed and live; the next delivery is `length + 1`.
    pub journal_length: u64,
    /// Replayed entries delivered so far.
    pub next_replayed: usize,
    pub replaying: bool,
    /// Whether an earlier execution ran this call: its nested call keys may already exist.
    pub rerun: bool,
    /// Whether live calls may launch; before, they are held (see `release()`).
    pub released: bool,
    /// `release()` checks the held calls against earlier executions.
    pub releasing: bool,
    /// `release()` was asked for before the gate opened.
    pub release_requested: bool,
    /// The count an earlier execution recorded after the script's first run.
    pub first_issued: Option<u64>,
    /// The highest issued count known to be committed: calls up to it may launch.
    pub durable_issued: u64,
    /// Live calls waiting for their issued count to be committed.
    pub waiting: Vec<CodemodeHostCall>,
    /// Journal index of each replayed call ID.
    pub replay_index: HashMap<u64, usize>,
    /// Calls the VM issued so far; its call IDs are 1, 2, ...
    pub issued: u64,
    /// Replay: results of journaled calls, ready to deliver in journal order.
    pub ready: HashMap<u64, CodemodeOutcome>,
    /// Replay: calls the journal does not have, admitted once replay is done.
    pub held: Vec<CodemodeHostCall>,
    /// Replay: journaled tool calls the VM made before the gate opened.
    pub gated: Vec<CodemodeHostCall>,
    /// Live: finished calls waiting for their journal commit.
    pub queue: Vec<Finished>,
    /// Acknowledged counts to record, by result number (0: the script's first run).
    pub acks: BTreeMap<u64, u64>,
    pub committing: bool,
    /// Set once the call decided an outcome other than the script's own (divergence, mismatch).
    pub ended: Option<StoredResult>,
    pub failure: Option<DurableError>,
    /// Whether the VM's fingerprint is checked or recorded; nothing is delivered or admitted before.
    pub gate_open: bool,
    /// Commits in flight; they finish before the call returns.
    pub commits: Pending,
    /// Calls and checks in flight; dropped when the script ends.
    pub calls: Pending,
    /// The script's tool calls for `details`.
    pub rows: Rows,
}

/// The Harness clock in whole milliseconds, as `Date.now()` reads.
pub(super) fn clock(api: &ToolExecutionApi) -> f64 {
    api.now().floor()
}

/// Store an outcome the call decided, unless one is stored already.
pub(super) fn store_decision(
    api: &ToolExecutionApi,
    context: &Context,
    result: StoredResult,
) -> BoxFuture<'static, DurableResult<()>> {
    let (api, context, task) = (api.clone(), context.clone(), api.task_id);
    Box::pin(async move {
        let change = move |tx: Tx| async move {
            let run = tx.doc(&CODEMODE_RUN_DOC, task).await?;
            run.edit(|state| {
                state.result.get_or_insert(result);
            })
        };
        api.commit(change, &context).await
    })
}

impl CodemodeCall {
    pub(super) fn new(
        options: Arc<CodemodeToolOptions>,
        api: ToolExecutionApi,
        context: Context,
        start: CodemodeStart,
        journal: JournalState,
        rerun: bool,
    ) -> Self {
        let replay_index = journal.entries.iter().enumerate().map(|(index, entry)| (entry.id, index)).collect();
        let first = journal.first_issued.unwrap_or(0);
        let durable_issued = journal.entries.iter().filter_map(|entry| entry.issued_after).fold(first, u64::max);
        Self {
            options,
            api,
            context,
            cancel: CancellationToken::new(),
            run: None,
            start,
            journal_length: journal.entries.len() as u64,
            replaying: !journal.entries.is_empty(),
            replayed: journal.entries,
            next_replayed: 0,
            rerun,
            released: false,
            releasing: false,
            release_requested: false,
            first_issued: journal.first_issued,
            durable_issued,
            waiting: Vec::new(),
            replay_index,
            issued: 0,
            ready: HashMap::new(),
            held: Vec::new(),
            gated: Vec::new(),
            queue: Vec::new(),
            acks: BTreeMap::new(),
            committing: false,
            ended: None,
            failure: None,
            gate_open: false,
            commits: FuturesUnordered::new(),
            calls: FuturesUnordered::new(),
            rows: Rows::default(),
        }
    }

    /// Store an outcome the call decided, with the details so far; returns it.
    pub(super) fn decide(
        &self,
        result: StoredResult,
    ) -> impl Future<Output = DurableResult<ToolExecutionResult>> + Send + use<> {
        let result = StoredResult { details: Some(self.rows.details()), ..result };
        let stored = store_decision(&self.api, &self.context, result.clone());
        async move {
            stored.await?;
            Ok(result.into_tool_result())
        }
    }
    /// Whether nothing more may happen: the run has its outcome, or the call decided or failed.
    pub(super) fn stopped(&self) -> bool {
        self.ended.is_some() || self.failure.is_some() || self.run.as_ref().is_some_and(RunDeliverer::finished)
    }

    pub(super) fn fail(&mut self, error: DurableError) {
        if self.failure.is_some() {
            return;
        }
        self.failure = Some(error);
        self.cancel.cancel();
    }

    /// End the call with `message`: store it as the outcome, then stop the VM with an error the script cannot catch.
    pub(super) fn diverge(&mut self, message: String) {
        if self.ended.is_some() || self.failure.is_some() {
            return;
        }
        let message = format!("The script took a different path when resumed after a restart: {message}");
        self.end_with(StoredResult::error(message));
    }

    /// Set the call's outcome and store it; the VM stops once it is stored.
    pub(super) fn end_with(&mut self, result: StoredResult) {
        let stored = StoredResult { details: Some(self.rows.details()), ..result.clone() };
        self.ended = Some(result);
        let decision = store_decision(&self.api, &self.context, stored);
        self.commits.push(Box::pin(async move { Done::Decided(decision.await) }));
    }

    /// Answer call `id` as result number `seq`; does nothing once the run has its outcome.
    pub(super) fn deliver(&mut self, seq: u64, at: f64, id: u64, outcome: CodemodeOutcome) {
        let Some(run) = &self.run else { return };
        if let Err(error) = run.deliver(seq, at, id, outcome) {
            self.fail(DurableError::error(error.0));
        }
    }
}
