//! One execution of the code mode tool: a first run, or a rerun after a restart that replays the journal. The VM's
//! callbacks become events of one loop, which owns the call's state; commits and calls are futures of that loop.

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use futures::stream::FuturesUnordered;
use pi_chord::{AbortSignal, Context};
use pi_codemode::{
    CodemodeDeclaredGlobal, CodemodeDeclaredTool, CodemodeErrorKind, CodemodeResult, CodemodeRunOptions, Timeout,
    parse_codemode_source, to_codemode_identifier,
};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};
use tokio_util::sync::CancellationToken;

use super::call::{CodemodeCall, Event, clock};
use super::docs::{
    CODEMODE_JOURNAL_DOC, CODEMODE_RUN_DOC, CODEMODE_STORE_DOC, CodemodeStart, DeclaredGlobal, DeclaredTool,
    JournalState, StoredResult,
};
use super::outcome::tool_result;
use super::{CODEMODE_MODULE_VERSION, CodemodeToolOptions};
use crate::errors::{DurableError, DurableResult};
use crate::harness::{ToolExecutionApi, ToolExecutionResult};
use crate::session::Tx;
use crate::util::random_bytes;

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
    fn run_options(&self, remaining: Option<f64>) -> (CodemodeRunOptions, UnboundedReceiver<Event>) {
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
        // Aborted, but not by this call: the host closed the sandbox, for example while shutting down. That decides
        // nothing about the script: store nothing and end with this invocation, so a later execution replays it.
        if let Err(error) = &result.outcome
            && error.kind == CodemodeErrorKind::Aborted
            && !self.cancel.is_cancelled()
        {
            if let Some(signal) = self.context.abort_signal() {
                signal.cancelled().await;
                signal.throw_if_aborted()?;
            }
            return Err(DurableError::error("The code mode sandbox was closed while the script ran"));
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
        let tool = StoredResult { details: Some(self.rows.details()), ..tool_result(&result) };
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
}
