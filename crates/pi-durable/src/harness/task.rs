//! Executable task definitions (upstream `TaskDefinition`, `Task`, and `defineTask`). Records carry JSON input and
//! checkpoints; typed handlers decode them with serde.

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::sync::Arc;

use futures::future::BoxFuture;
use pi_chord::{Context, Json};

use super::scheduler::TaskRuntime;
use crate::errors::{DurableError, DurableResult};
use crate::tasks::TaskTemplate;
use crate::types::{TaskRecord, TaskState};

/// Runs one checkpoint phase, or the abort handler. It must commit progress through `runtime.commit()`.
pub type PhaseHandler =
    Arc<dyn Fn(TaskRecord, TaskRuntime, Context) -> BoxFuture<'static, DurableResult<()>> + Send + Sync>;
type FirstCheckpointFn = Arc<dyn Fn(&Json) -> DurableResult<Json> + Send + Sync>;
/// Converts `(input, checkpoint, from_version)` stored by an older version to the current one.
pub type MigrateFn = Arc<dyn Fn(&Json, &Json, u32) -> DurableResult<(Json, Json)> + Send + Sync>;

/// Executable durable state machine definition, registered in the registry by `name`.
pub struct TaskDefinition {
    pub name: String,
    pub version: u32,
    first: FirstCheckpointFn,
    phases: HashMap<String, PhaseHandler>,
    abort: Option<PhaseHandler>,
    migrate: Option<MigrateFn>,
}

/// A shared task definition; compare definitions with [`Arc::ptr_eq`].
pub type Task = Arc<TaskDefinition>;

impl fmt::Debug for TaskDefinition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TaskDefinition").field("name", &self.name).field("version", &self.version).finish()
    }
}

/// A phase handler from an async closure.
pub fn handler<F, Fut>(run: F) -> PhaseHandler
where
    F: Fn(TaskRecord, TaskRuntime, Context) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = DurableResult<()>> + Send + 'static,
{
    Arc::new(move |task, runtime, context| Box::pin(run(task, runtime, context)))
}

impl TaskDefinition {
    /// A definition whose first checkpoint `first` computes from the input.
    pub fn new(
        name: impl Into<String>,
        version: u32,
        first: impl Fn(&Json) -> DurableResult<Json> + Send + Sync + 'static,
    ) -> Self {
        Self {
            name: name.into(),
            version,
            first: Arc::new(first),
            phases: HashMap::new(),
            abort: None,
            migrate: None,
        }
    }

    pub fn phase<F, Fut>(mut self, name: impl Into<String>, run: F) -> Self
    where
        F: Fn(TaskRecord, TaskRuntime, Context) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = DurableResult<()>> + Send + 'static,
    {
        self.phases.insert(name.into(), handler(run));
        self
    }

    /// Runs in a fresh invocation after an abort mark and must commit a terminal outcome.
    pub fn abort<F, Fut>(mut self, run: F) -> Self
    where
        F: Fn(TaskRecord, TaskRuntime, Context) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = DurableResult<()>> + Send + 'static,
    {
        self.abort = Some(handler(run));
        self
    }

    pub fn migrate(
        mut self,
        migrate: impl Fn(&Json, &Json, u32) -> DurableResult<(Json, Json)> + Send + Sync + 'static,
    ) -> Self {
        self.migrate = Some(Arc::new(migrate));
        self
    }

    pub fn has_migrate(&self) -> bool {
        self.migrate.is_some()
    }

    pub fn run_migrate(&self, input: &Json, checkpoint: &Json, from: u32) -> DurableResult<(Json, Json)> {
        match &self.migrate {
            Some(migrate) => migrate(input, checkpoint, from),
            None => Err(missing_migration(&self.name, self.version, from)),
        }
    }

    pub fn phase_handler(&self, phase: &str) -> Option<PhaseHandler> {
        self.phases.get(phase).cloned()
    }

    pub fn abort_handler(&self) -> Option<PhaseHandler> {
        self.abort.clone()
    }
}

pub(crate) fn missing_migration(kind: &str, version: u32, from: u32) -> DurableError {
    DurableError::error(format!("Task {kind} version {version} has no migration from {from}"))
}

impl TaskTemplate for TaskDefinition {
    fn name(&self) -> &str {
        &self.name
    }

    fn version(&self) -> u32 {
        self.version
    }

    fn initial(&self, input: &Json) -> DurableResult<Json> {
        (self.first)(input)
    }
}

/// Seal a definition (upstream `defineTask`). Register it in the registry so a Harness can run its tasks.
pub fn define_task(definition: TaskDefinition) -> Task {
    Arc::new(definition)
}

/// The next state a task commits for itself: only `Running`, `Waiting`, and `Terminal` are accepted.
pub type NextTaskState = TaskState;

/// The phase name of a checkpoint.
pub fn phase_of(checkpoint: &Json) -> String {
    checkpoint.get("phase").and_then(Json::as_str).unwrap_or_default().to_owned()
}
