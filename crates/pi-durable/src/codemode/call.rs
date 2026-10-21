//! One execution of the code mode tool: a first run, or a rerun after a restart that replays the journal. The VM's
//! callbacks become events of one loop, which owns the call's state; commits and calls are futures of that loop.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use futures::future::BoxFuture;
use futures::stream::FuturesUnordered;
use pi_chord::{AbortSignal, Context};
use pi_codemode::{
    CodemodeDeclaredGlobal, CodemodeDeclaredTool, CodemodeHostCall, CodemodeOutcome, CodemodeResult,
    CodemodeRunOptions, RunDeliverer, Timeout, parse_codemode_source, to_codemode_identifier,
};
use tokio::sync::mpsc::unbounded_channel;
use tokio_util::sync::CancellationToken;

use super::docs::{
    CODEMODE_JOURNAL_DOC, CODEMODE_RUN_DOC, CODEMODE_STORE_DOC, CodemodeStart, DeclaredGlobal, DeclaredTool,
    JournalEntry, JournalGlobal, JournalState, StoredResult,
};
use super::outcome::tool_result;
use super::{CODEMODE_MODULE_VERSION, CodemodeToolOptions};
use crate::errors::{DurableError, DurableResult};
use crate::harness::{NestedToolExecutionResult, ToolExecutionApi, ToolExecutionResult};
use crate::session::Tx;
use crate::util::random_bytes;

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
}

/// Cancels the run when the call's context aborts, until dropped.
struct AbortLink(Option<(AbortSignal, u64)>);

impl AbortLink {
    fn new(context: &Context, cancel: CancellationToken) -> Self {
        Self(context.abort_signal().map(|signal| {
            let id = signal.add_listener(move |_| cancel.cancel());
            (signal, id)
        }))
    }
}

impl Drop for AbortLink {
    fn drop(&mut self) {
        if let Some((signal, id)) = self.0.take() {
            signal.remove_listener(id);
        }
    }
}

enum Found {
    Done(StoredResult),
    Run { start: CodemodeStart, journal: JournalState, rerun: bool },
}

fn random_seed() -> u32 {
    let mut bytes = [0_u8; 4];
    random_bytes(&mut bytes);
    u32::from_le_bytes(bytes)
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

pub(super) async fn execute(
    options: Arc<CodemodeToolOptions>,
    api: ToolExecutionApi,
    context: Context,
    source: &str,
) -> DurableResult<ToolExecutionResult> {
    let parsed = parse_codemode_source(source).map_err(|error| DurableError::error(error.0))?;
    let agent = api.agent(&context).await?;
    let tools: Vec<DeclaredTool> = agent
        .callable
        .iter()
        .map(|tool| DeclaredTool {
            name: tool.name().to_owned(),
            js_name: to_codemode_identifier(tool.name()),
            description: tool.tool.description.clone(),
        })
        .collect();
    let globals: Vec<DeclaredGlobal> = options
        .globals
        .iter()
        .map(|global| DeclaredGlobal { name: global.name.clone(), spread: global.spread })
        .collect();
    let now = clock(&api);
    let seed = random_seed();
    let timeout = parsed.options.timeout_ms;
    let (task, conversation) = (api.task_id, api.conversation_id);
    let change = move |tx: Tx| async move {
        let run = tx.doc(&CODEMODE_RUN_DOC, task).await?;
        let state = run.get()?;
        if let Some(result) = state.result {
            return Ok(Found::Done(result));
        }
        let rerun = state.start.is_some();
        let start = match state.start {
            Some(start) => start,
            None => {
                let store = tx.doc(&CODEMODE_STORE_DOC, conversation).await?.get()?;
                let start = CodemodeStart {
                    version: CODEMODE_MODULE_VERSION,
                    started_at: now,
                    seed,
                    fingerprint: None,
                    deadline: timeout.map(|timeout| now + timeout as f64),
                    tools,
                    globals,
                    store: store.values,
                };
                run.edit(|state| state.start = Some(start.clone()))?;
                start
            }
        };
        let journal = tx.doc(&CODEMODE_JOURNAL_DOC, task).await?.get()?;
        Ok(Found::Run { start, journal, rerun })
    };
    let (start, journal, rerun) = match api.commit(change, &context).await? {
        Found::Done(result) => return Ok(result.into_tool_result()),
        Found::Run { start, journal, rerun } => (start, journal, rerun),
    };
    let call = CodemodeCall::new(options, api, context, start, journal, rerun);
    if call.start.version != CODEMODE_MODULE_VERSION {
        let message = format!(
            "The script cannot resume: it started under code mode version {}, this is {CODEMODE_MODULE_VERSION}.",
            call.start.version
        );
        return call.decide(StoredResult::error(message)).await;
    }
    let remaining = call.start.deadline.map(|deadline| deadline - clock(&call.api));
    if remaining.is_some_and(|remaining| remaining <= 0.0) {
        let message = "Script timed out: its deadline passed while it was interrupted.";
        return call.decide(StoredResult::error(message)).await;
    }
    call.run(&parsed.code, remaining).await
}

impl CodemodeCall {
    fn new(
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
        }
    }

    /// Store an outcome the call decided; returns it.
    fn decide(&self, result: StoredResult) -> impl Future<Output = DurableResult<ToolExecutionResult>> + Send + use<> {
        let stored = store_decision(&self.api, &self.context, result.clone());
        async move {
            stored.await?;
            Ok(result.into_tool_result())
        }
    }

    fn run_options(&self, remaining: Option<f64>) -> (CodemodeRunOptions, tokio::sync::mpsc::UnboundedReceiver<Event>) {
        let (sender, events) = unbounded_channel();
        let on_call = sender.clone();
        let mut options = CodemodeRunOptions::new(self.start.started_at, self.start.seed, move |call| {
            let _ = on_call.send(Event::Call(call));
        });
        options.tools = self
            .start
            .tools
            .iter()
            .map(|tool| CodemodeDeclaredTool {
                name: tool.name.clone(),
                js_name: Some(tool.js_name.clone()),
                description: Some(tool.description.clone()),
            })
            .collect();
        options.globals = self
            .start
            .globals
            .iter()
            .map(|global| CodemodeDeclaredGlobal { name: global.name.clone(), spread: global.spread })
            .collect();
        options.store = self.start.store.clone();
        options.cancel = Some(self.cancel.clone());
        options.timeout = match remaining {
            Some(remaining) => Timeout::After(Duration::from_millis(remaining.ceil() as u64)),
            None => Timeout::Never,
        };
        let on_started = sender.clone();
        options.on_started = Some(Arc::new(move |fingerprint: &str| {
            let _ = on_started.send(Event::Started(fingerprint.to_owned()));
        }));
        options.on_idle = Some(Arc::new(move |seq, issued| {
            let _ = sender.send(Event::Idle(seq, issued));
        }));
        (options, events)
    }

    async fn run(mut self, code: &str, remaining: Option<f64>) -> DurableResult<ToolExecutionResult> {
        let _link = AbortLink::new(&self.context, self.cancel.clone());
        let (options, mut events) = self.run_options(remaining);
        let run = self.options.sandbox.start(code, options).map_err(|error| DurableError::error(error.0))?;
        self.run = Some(run.deliverer());
        let mut result = run.result;
        let result = loop {
            tokio::select! {
                biased;
                Some(event) = events.recv() => self.handle_event(event),
                Some(done) = self.commits.next(), if !self.commits.is_empty() => self.handle_done(done),
                Some(done) = self.calls.next(), if !self.calls.is_empty() => self.handle_done(done),
                result = &mut result => break result,
            }
        };
        // Commits in flight finish; calls still running are dropped, and the call's cleanup aborts their tasks.
        while let Some(done) = self.commits.next().await {
            self.handle_done(done);
        }
        self.calls = FuturesUnordered::new();
        self.finish(result).await
    }

    async fn finish(mut self, result: CodemodeResult) -> DurableResult<ToolExecutionResult> {
        if let Some(failure) = self.failure.take() {
            return Err(failure);
        }
        if let Some(ended) = self.ended.take() {
            return Ok(ended.into_tool_result());
        }
        if let Some(signal) = self.context.abort_signal() {
            signal.throw_if_aborted()?;
        }
        // A replay that ends where the live run went on is a divergence (design section 4.3). Within the replayed
        // part, the first run included: a rerun without journal entries replays that much too.
        if self.rerun
            && let Some(during) = result.during
            && during.seq <= self.replayed.len() as u64
        {
            // The live run went on: it acknowledged that result, or it made a call that later results answer.
            let seq = during.seq as usize;
            let acknowledged = match seq {
                0 => !self.replayed.is_empty() || self.first_issued.is_some(),
                _ => self.replayed[seq - 1].issued_after.is_some(),
            };
            if acknowledged || self.replayed[seq..].iter().any(|entry| entry.id > during.issued) {
                let message = format!(
                    "The script took a different path when resumed after a restart: it ended after result {seq}, where it went on before."
                );
                return self.decide(StoredResult::error(message)).await;
            }
        }
        self.complete(result).await
    }

    /// The script's own outcome, stored with its store writes in one transaction.
    fn complete(
        &self,
        result: CodemodeResult,
    ) -> impl Future<Output = DurableResult<ToolExecutionResult>> + Send + use<> {
        let tool = tool_result(&result);
        let writes = result.outcome.ok().map(|success| success.store_writes).filter(|writes| !writes.is_empty());
        let (task, conversation) = (self.api.task_id, self.api.conversation_id);
        let change = move |tx: Tx| async move {
            let run = tx.doc(&CODEMODE_RUN_DOC, task).await?;
            if let Some(existing) = run.get()?.result {
                return Ok(existing);
            }
            run.edit(|state| state.result = Some(tool.clone()))?;
            if let Some(writes) = writes {
                let store = tx.doc(&CODEMODE_STORE_DOC, conversation).await?;
                store.edit(|state| {
                    for key in &writes.delete {
                        state.values.shift_remove(key);
                    }
                    for (key, value) in writes.set {
                        state.values.insert(key, value);
                    }
                })?;
            }
            Ok(tool)
        };
        let (api, context) = (self.api.clone(), self.context.clone());
        async move { Ok(api.commit(change, &context).await?.into_tool_result()) }
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
        self.ended = Some(result.clone());
        let decision = store_decision(&self.api, &self.context, result);
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
