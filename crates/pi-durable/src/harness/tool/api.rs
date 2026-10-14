//! Operations available to one tool invocation (upstream `ToolExecutionApi`). Every operation fails after the
//! invocation ends.

use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use futures::FutureExt;
use futures::future::{BoxFuture, Shared};
use pi_ai::JsonObject;
use pi_chord::{Context, Json};

use super::result::{check_key, fallback_result};
use super::{NESTED_RESULT_DOC, NestedResult};
use crate::documents::{DocFamilyToken, DocScope, DocToken, DocValue, RewindableDocScope};
use crate::env::{SharedEnv, ShellOutputSkip, ShellOutputWindow};
use crate::errors::{DurableError, DurableResult};
use crate::harness::output::{OutputBuffer, OutputChunk, OutputLimits};
use crate::harness::progress::Progress;
use crate::harness::registry::RegistrySnapshot;
use crate::harness::scheduler::{ConversationHandle, TaskRuntime};
use crate::harness::task::Task;
use crate::harness::types::{Agent, NestedToolExecutionResult, ToolDiagnostic};
use crate::harness::util::{cancellation, lock, with_context};
use crate::ids::{ConversationId, EntryId, TaskId};
use crate::models::Models;
use crate::session::{CommittedWatch, Tx};
use crate::tasks::{TaskOptions, TaskOwnership};
use crate::types::TaskRecord;

/// What a running tool reported through its api: output, the last details, and diagnostics.
pub(crate) struct Reported {
    pub output: OutputBuffer,
    pub limits: OutputLimits,
    pub diagnostics: Vec<ToolDiagnostic>,
    pub details: Option<Json>,
}

pub(crate) type Admission = Shared<BoxFuture<'static, DurableResult<TaskId>>>;

/// State one execution shares between its api and the tool task.
pub(crate) struct CallState {
    pub reported: Mutex<Reported>,
    pub ended: AtomicBool,
    /// Nested call admissions this invocation started; cleanup waits for them.
    pub admissions: Mutex<Vec<Admission>>,
    /// Default keys: the order of this invocation's nested calls.
    pub sequence: AtomicU32,
    /// Whether the call reruns after a restart (`replay: safe`).
    pub resumes: bool,
}

/// Options of [`ToolExecutionApi::execute_tool`].
#[derive(Clone, Debug, Default)]
pub struct ExecuteToolOptions {
    /// The nested call's key; default its position among this invocation's nested calls.
    pub key: Option<String>,
    /// `Some(false)`: commit no running progress to the nested call's slot.
    pub progress: Option<bool>,
}

/// Operations available to one tool invocation; cheap to clone.
#[derive(Clone)]
pub struct ToolExecutionApi {
    pub task_id: TaskId,
    pub conversation_id: ConversationId,
    pub call_id: String,
    /// Built by `HarnessOptions::env` for this call.
    pub env: Option<SharedEnv>,
    /// The tail this call's output keeps and the pace of its progress commits; `None` for head retention.
    pub output_window: Option<ShellOutputWindow>,
    pub(crate) runtime: TaskRuntime,
    pub(crate) state: Arc<CallState>,
    pub(crate) progress: Progress,
}

impl ToolExecutionApi {
    fn assert_live(&self) -> DurableResult<()> {
        if self.state.ended.load(Ordering::SeqCst) {
            return Err(DurableError::error(format!("Tool call {} has settled", self.call_id)));
        }
        Ok(())
    }

    /// The tool task's phase snapshot.
    pub fn registry(&self) -> RegistrySnapshot {
        self.runtime.registry()
    }

    /// The calling conversation's agent, as the tool task's phase resolved it.
    pub async fn agent(&self, context: &Context) -> DurableResult<Agent> {
        self.runtime.agent(context).await
    }

    pub fn models(&self) -> Arc<dyn Models> {
        self.runtime.models()
    }

    /// Append running text output; it becomes the result's output when the result omits it.
    pub fn output(&self, chunk: &str) -> DurableResult<()> {
        self.push(OutputChunk::Text(chunk), None)
    }

    /// Append running byte output; `skipped` counts output omitted before the chunk (tail retention only).
    pub fn output_bytes(&self, chunk: &[u8], skipped: Option<ShellOutputSkip>) -> DurableResult<()> {
        self.push(OutputChunk::Bytes(chunk), skipped)
    }

    fn push(&self, chunk: OutputChunk<'_>, skipped: Option<ShellOutputSkip>) -> DurableResult<()> {
        self.assert_live()?;
        if lock(&self.state.reported).output.push(chunk, skipped)? {
            self.progress.mark();
        }
        Ok(())
    }

    /// The output kept so far, as the model will see it when the result omits output, and whether any was dropped.
    pub fn retained_output(&self) -> DurableResult<(String, bool)> {
        self.assert_live()?;
        let kept = lock(&self.state.reported).output.snapshot();
        Ok((kept.text, kept.dropped_bytes > 0))
    }

    /// Record a model-visible remark about this call.
    pub fn diagnostic(&self, diagnostic: ToolDiagnostic) -> DurableResult<()> {
        self.assert_live()?;
        lock(&self.state.reported).diagnostics.push(diagnostic);
        self.progress.mark();
        Ok(())
    }

    /// Replace running details; the last value becomes the result details when the result omits them.
    pub async fn details(&self, value: Json, context: &Context) -> DurableResult<()> {
        self.assert_live()?;
        if context.is_cancelled() {
            return Err(cancellation(context));
        }
        lock(&self.state.reported).details = Some(value);
        let committed = self.progress.mark_and_wait();
        let wait = async move { committed.await.unwrap_or(Ok(())) };
        with_context(wait, context).await
    }

    pub async fn commit<T, F, Fut>(&self, change: F, context: &Context) -> DurableResult<T>
    where
        T: Send + 'static,
        F: FnOnce(Tx) -> Fut + Send + 'static,
        Fut: Future<Output = DurableResult<T>> + Send + 'static,
    {
        self.runtime.commit_map(change, context).await
    }

    pub async fn memo(&self, name: &str) -> DurableResult<Option<Json>> {
        self.runtime.memo(name).await
    }

    pub async fn memo_or(&self, name: &str, candidate: Json, context: &Context) -> DurableResult<Json> {
        self.runtime.memo_or(name, candidate, context).await
    }

    /// Create a task in one commit. A child of a tool that is not replay-safe is abandoned on restart.
    pub async fn create_task(
        &self,
        task: Task,
        input: Json,
        options: TaskOptions,
        context: &Context,
    ) -> DurableResult<TaskId> {
        let mut options = TaskOptions { conversation_id: None, ..options };
        if matches!(options.ownership, TaskOwnership::Task(_)) && !self.state.resumes {
            options.abandon_on_restart = true;
        }
        self.runtime.commit_map(move |tx| async move { tx.create_task(task, input, options).await }, context).await
    }

    pub async fn get_task(&self, id: TaskId, context: &Context) -> DurableResult<Option<TaskRecord>> {
        self.runtime.get_task(id, context).await
    }

    pub async fn wait_for_task(&self, id: TaskId, context: &Context) -> DurableResult<TaskRecord> {
        self.runtime.wait_for_task(id, context).await
    }

    pub async fn conversation(
        &self,
        id: ConversationId,
        context: &Context,
    ) -> DurableResult<Option<ConversationHandle>> {
        self.runtime.conversation(id, context).await
    }

    pub async fn snapshot<T: DocValue, S: DocScope>(
        &self,
        token: &DocToken<T, S>,
        owner: S::Owner,
        context: &Context,
    ) -> DurableResult<Option<Json>> {
        self.runtime.snapshot(token, owner, context).await
    }

    pub async fn family_snapshot<T: DocValue, I: DocValue, S: DocScope>(
        &self,
        token: &DocFamilyToken<T, I, S>,
        owner: S::Owner,
        key: &str,
        context: &Context,
    ) -> DurableResult<Option<Json>> {
        self.runtime.family_snapshot(token, owner, key, context).await
    }

    pub async fn snapshot_as_of<T: DocValue, S: RewindableDocScope>(
        &self,
        token: &DocToken<T, S>,
        conversation_id: ConversationId,
        at: EntryId,
        context: &Context,
    ) -> DurableResult<Option<Json>> {
        self.runtime.snapshot_as_of(token, conversation_id, at, context).await
    }

    /// A watch of a committed document; it stops when the tool's invocation ends.
    pub async fn watch_doc<T: DocValue, S: DocScope>(
        &self,
        token: &DocToken<T, S>,
        owner: S::Owner,
        context: &Context,
    ) -> DurableResult<Option<CommittedWatch>> {
        self.runtime.watch_doc(token, owner, context).await
    }

    /// Run tool `name` as a nested call of this one and wait for its result. Its call ID is `<callId>/<key>`.
    pub async fn execute_tool(
        &self,
        name: &str,
        args: JsonObject,
        context: &Context,
        options: ExecuteToolOptions,
    ) -> DurableResult<NestedToolExecutionResult> {
        self.assert_live()?;
        let key = match options.key {
            Some(key) => {
                check_key(&key)?;
                key
            }
            None => (self.state.sequence.fetch_add(1, Ordering::SeqCst) + 1).to_string(),
        };
        let (runtime, parent, name_owned) = (self.runtime.clone(), self.call_id.clone(), name.to_owned());
        let abandon = !self.state.resumes;
        let admission_context = context.clone();
        let admission: Admission = async move {
            super::settle::start_nested_call(
                &runtime,
                &parent,
                &name_owned,
                args,
                &key,
                options.progress,
                abandon,
                &admission_context,
            )
            .await
        }
        .boxed()
        .shared();
        lock(&self.state.admissions).push(admission.clone());
        let id = admission.await?;
        let settled = self.runtime.wait_for_task(id, context).await?;
        let stored = self.runtime.family_snapshot(&NESTED_RESULT_DOC, self.task_id, &id.to_string(), context).await?;
        match stored {
            Some(stored) => Ok(stored.deserialize_into::<NestedResult>()?.result),
            None => Ok(fallback_result(id, name, settled.state.outcome())),
        }
    }
}
