//! The scriptable `test.node` task and the shared helpers of the structured tests.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use futures::future::BoxFuture;
use pi_chord::{Context, Json};
use pi_durable::documents::{DocSpec, DocToken, TaskScope, define_doc};
use pi_durable::harness::*;
use pi_durable::session::Tx;
use pi_durable::storage::MemoryStorage;
use pi_durable::tasks::TaskOptions;
use pi_durable::*;
use serde_json::json;
use tokio::sync::watch;

use super::support::context;
use super::support::task_support::{aborted, eventually, open_tasks};

/// How a default run ends once its gate opens: its outcome, or a throw that faults it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Ending {
    Completed,
    Failed,
    Throw,
}

pub type Step = BoxFuture<'static, DurableResult<()>>;
type Handler = Arc<dyn Fn(TaskRuntime, Context) -> Step + Send + Sync>;
type Resume = Arc<dyn Fn(TaskRuntime, Context, u64) -> Step + Send + Sync>;

/// Per-name behavior of `test.node`; unscripted parts use the defaults.
#[derive(Clone, Default)]
struct Behavior {
    run: Option<Handler>,
    resume: Option<Resume>,
    abort: Option<Handler>,
}

struct Inner {
    behaviors: Mutex<HashMap<String, Behavior>>,
    gates: watch::Sender<HashMap<String, Ending>>,
    /// Handler starts in order, such as `run:a`, `abort:a`.
    log: Mutex<Vec<String>>,
}

/// The scriptable `test.node` task with its behaviors, gates, and log.
#[derive(Clone)]
pub struct Nodes {
    inner: Arc<Inner>,
    pub task: Task,
}

impl Nodes {
    pub fn new() -> Self {
        let inner = Arc::new(Inner {
            behaviors: Mutex::default(),
            gates: watch::channel(HashMap::new()).0,
            log: Mutex::default(),
        });
        Self { task: node_task(&inner), inner }
    }

    pub fn open(&self, name: &str, ending: Ending) {
        self.inner.gates.send_modify(|gates| {
            gates.insert(name.to_owned(), ending);
        });
    }

    pub fn open_ok(&self, name: &str) {
        self.open(name, Ending::Completed);
    }

    pub async fn gate(&self, name: &str) -> Ending {
        gate(&self.inner, name).await
    }

    pub fn push(&self, line: String) {
        self.inner.log.lock().unwrap().push(line);
    }

    pub fn log(&self) -> Vec<String> {
        self.inner.log.lock().unwrap().clone()
    }

    pub fn logged(&self, line: &str) -> bool {
        self.inner.log.lock().unwrap().iter().any(|logged| logged == line)
    }

    /// Logged lines starting with `prefix`.
    pub fn lines(&self, prefix: &str) -> Vec<String> {
        self.log().into_iter().filter(|line| line.starts_with(prefix)).collect()
    }

    fn edit(&self, name: &str, change: impl FnOnce(&mut Behavior)) {
        change(self.inner.behaviors.lock().unwrap().entry(name.to_owned()).or_default());
    }

    pub fn run<F, Fut>(&self, name: &str, run: F)
    where
        F: Fn(TaskRuntime, Context) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = DurableResult<()>> + Send + 'static,
    {
        let run: Handler = Arc::new(move |runtime, context| Box::pin(run(runtime, context)));
        self.edit(name, |behavior| behavior.run = Some(run));
    }

    pub fn resume<F, Fut>(&self, name: &str, resume: F)
    where
        F: Fn(TaskRuntime, Context, u64) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = DurableResult<()>> + Send + 'static,
    {
        let resume: Resume = Arc::new(move |runtime, context, round| Box::pin(resume(runtime, context, round)));
        self.edit(name, |behavior| behavior.resume = Some(resume));
    }

    pub fn abort<F, Fut>(&self, name: &str, abort: F)
    where
        F: Fn(TaskRuntime, Context) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = DurableResult<()>> + Send + 'static,
    {
        let abort: Handler = Arc::new(move |runtime, context| Box::pin(abort(runtime, context)));
        self.edit(name, |behavior| behavior.abort = Some(abort));
    }

    /// Script `name`'s abort handler to wait for the gate `abort.<name>`, then end aborted.
    pub fn slow_abort(&self, name: &str) {
        let (nodes, gate) = (self.clone(), format!("abort.{name}"));
        self.abort(name, move |runtime, context| {
            let (nodes, gate) = (nodes.clone(), gate.clone());
            async move {
                nodes.gate(&gate).await;
                runtime.commit(|_, _| async { Ok(aborted_state()) }, &context).await
            }
        });
    }

    /// Script `name`'s run to end with `next` in one commit.
    pub fn run_commit(&self, name: &str, next: impl Fn() -> Option<TaskState> + Send + Sync + 'static) {
        let next = Arc::new(next);
        self.run(name, move |runtime, context| {
            let state = next();
            async move { runtime.commit(move |_, _| async move { Ok(state) }, &context).await }
        });
    }
}

async fn gate(inner: &Inner, name: &str) -> Ending {
    let mut receiver = inner.gates.subscribe();
    let gates = receiver.wait_for(|gates| gates.contains_key(name)).await.unwrap();
    gates[name]
}

fn behavior(inner: &Inner, name: &str) -> Behavior {
    inner.behaviors.lock().unwrap().get(name).cloned().unwrap_or_default()
}

pub fn name_of(task: &TaskRecord) -> String {
    task.input["name"].as_str().unwrap_or_default().to_owned()
}

/// A task that runs its name's behavior.
fn node_task(inner: &Arc<Inner>) -> Task {
    let (run_inner, resume_inner, abort_inner) = (inner.clone(), inner.clone(), inner.clone());
    define_task(
        TaskDefinition::new("test.node", 1, |_| Ok(Json::from(json!({ "phase": "run" }))))
            .phase("run", move |task, runtime, context| {
                let inner = run_inner.clone();
                async move {
                    let name = name_of(&task);
                    inner.log.lock().unwrap().push(format!("run:{name}"));
                    if let Some(run) = behavior(&inner, &name).run {
                        return run(runtime, context).await;
                    }
                    // Wait for the gate or the abort signal, then end as the gate says.
                    let ending = tokio::select! {
                        ending = gate(&inner, &name) => ending,
                        () = aborted(runtime.signal()) => Ending::Failed,
                    };
                    if ending == Ending::Throw {
                        return Err(DurableError::error(format!("{name} threw")));
                    }
                    runtime.commit(move |_, _| async move { Ok(end(ending, &name)) }, &context).await
                }
            })
            .phase("resume", move |task, runtime, context| {
                let inner = resume_inner.clone();
                async move {
                    let name = name_of(&task);
                    inner.log.lock().unwrap().push(format!("resume:{name}"));
                    if let Some(resume) = behavior(&inner, &name).resume {
                        let round = task.state.checkpoint().and_then(|checkpoint| checkpoint["round"].as_u64());
                        return resume(runtime, context, round.unwrap_or_default()).await;
                    }
                    runtime.commit(move |_, _| async move { Ok(end(Ending::Completed, &name)) }, &context).await
                }
            })
            .abort(move |task, runtime, context| {
                let inner = abort_inner.clone();
                async move {
                    let name = name_of(&task);
                    inner.log.lock().unwrap().push(format!("abort:{name}"));
                    if let Some(abort) = behavior(&inner, &name).abort {
                        return abort(runtime, context).await;
                    }
                    let result = Some(Json::from(name));
                    let state = TaskState::Terminal { outcome: TaskOutcome::Aborted { reason: None, result } };
                    runtime.commit(move |_, _| async move { Ok(Some(state)) }, &context).await
                }
            }),
    )
}

pub fn end(ending: Ending, name: &str) -> Option<TaskState> {
    let outcome = if ending == Ending::Completed {
        TaskOutcome::Completed { result: Some(Json::from(name)) }
    } else {
        TaskOutcome::Failed { error: TaskOutcomeError::new(format!("{name} failed")), result: None }
    };
    Some(TaskState::Terminal { outcome })
}

pub fn aborted_state() -> Option<TaskState> {
    Some(TaskState::Terminal { outcome: TaskOutcome::Aborted { reason: None, result: None } })
}

pub fn resume_at(round: u64) -> Option<TaskState> {
    Some(TaskState::Running { checkpoint: Json::from(json!({ "phase": "resume", "round": round })) })
}

/// Wait on `on`, resuming in round `round`.
pub fn wait_on(on: Vec<TaskId>, policy: JoinPolicy, round: u64) -> Option<TaskState> {
    let checkpoint = Json::from(json!({ "phase": "resume", "round": round }));
    Some(TaskState::Waiting { checkpoint, on, policy })
}

/// Never registered: aborting it can only orphan it.
pub fn unregistered() -> Task {
    define_task(
        TaskDefinition::new("test.unregistered", 1, |_| Ok(Json::from(json!({ "phase": "run" }))))
            .phase("run", |_, _, _| async { Ok(()) })
            .abort(|_, _, _| async { Ok(()) }),
    )
}

pub static TASK_NOTES: LazyLock<DocToken<Json, TaskScope>> = LazyLock::new(|| {
    define_doc(DocSpec::new("test.task-notes", 1, TaskScope, || Json::from(json!({ "text": "" })))).unwrap()
});

pub fn named(name: &str) -> Json {
    Json::from(json!({ "name": name }))
}

/// A child task named `name`, owned by `owner`.
pub async fn spawn(tx: &Tx, nodes: &Nodes, owner: TaskId, name: &str) -> DurableResult<TaskId> {
    tx.create_task(nodes.task.clone(), named(name), TaskOptions::owned_by(owner)).await
}

pub async fn start_with(conversation: &Conversation, nodes: &Nodes, name: &str, options: TaskOptions) -> TaskId {
    let (task, input) = (nodes.task.clone(), named(name));
    let create = move |tx: Tx| async move { tx.create_task(task, input, options).await };
    conversation.commit(create, &context()).await.unwrap()
}

pub async fn start(conversation: &Conversation, nodes: &Nodes, name: &str) -> TaskId {
    start_with(conversation, nodes, name, TaskOptions::conversation(conversation.id)).await
}

pub struct Opened {
    pub harness: Harness,
    pub root: Conversation,
    pub registry: Arc<Registry>,
    pub reports: Arc<Mutex<Vec<DurableError>>>,
    pub nodes: Nodes,
}

pub async fn open_nodes_in(storage: Arc<dyn Storage>, nodes: &Nodes) -> Opened {
    let opened = open_tasks(storage, vec![nodes.task.clone()]).await;
    let root = opened.harness.root(&context(), CreateOptions::default()).await.unwrap();
    opened.harness.resume().unwrap();
    Opened { harness: opened.harness, root, registry: opened.registry, reports: opened.reports, nodes: nodes.clone() }
}

pub async fn open_nodes() -> Opened {
    open_nodes_in(Arc::new(MemoryStorage::new()), &Nodes::new()).await
}

pub async fn state(harness: &Harness, id: TaskId) -> TaskState {
    harness.get_task(id, &context()).await.unwrap().unwrap().state
}

pub async fn status(harness: &Harness, id: TaskId) -> TaskStatus {
    state(harness, id).await.status()
}

pub async fn marked(harness: &Harness, id: TaskId) -> bool {
    harness.get_task(id, &context()).await.unwrap().unwrap().abort_requested
}

pub async fn outcome_of(harness: &Harness, id: TaskId) -> &'static str {
    harness.wait_for_task(id, &context()).await.unwrap().state.outcome().unwrap().status()
}

pub async fn outcome_json(harness: &Harness, id: TaskId) -> serde_json::Value {
    serde_json::to_value(harness.wait_for_task(id, &context()).await.unwrap().state.outcome()).unwrap()
}

pub async fn until_status(harness: &Harness, id: TaskId, expected: TaskStatus) {
    eventually(|| async { status(harness, id).await == expected }).await;
}

/// Whether the spawned `future` settles within a few scheduler turns.
pub async fn settled<T: Send + 'static>(future: impl Future<Output = T> + Send + 'static) -> bool {
    tokio::time::timeout(Duration::from_millis(20), tokio::spawn(future)).await.is_ok()
}

pub fn wait_task(harness: &Harness, id: TaskId) -> impl Future<Output = DurableResult<TaskRecord>> + Send + 'static {
    let harness = harness.clone();
    async move { harness.wait_for_task(id, &context()).await }
}

pub type Ids = Arc<Mutex<Vec<TaskId>>>;

pub fn ids(slot: &Ids) -> Vec<TaskId> {
    slot.lock().unwrap().clone()
}

/// Assert that `id` settled faulted with a message containing `expected`.
pub async fn assert_faulted(harness: &Harness, id: TaskId, expected: &str) {
    let outcome = outcome_json(harness, id).await;
    assert_eq!(outcome["status"], "faulted", "{outcome}");
    assert!(outcome["error"]["message"].as_str().unwrap_or_default().contains(expected), "{outcome}");
}

/// Script `name` to spawn `child`, then go on in round 1 (`wait`: wait on the child with allSettled instead).
pub fn spawn_child(nodes: &Nodes, name: &str, child: &'static str, wait: bool) -> Ids {
    let found = Ids::default();
    let (sink, spawner) = (found.clone(), nodes.clone());
    nodes.run(name, move |runtime, context| {
        let (sink, nodes) = (sink.clone(), spawner.clone());
        async move {
            let owner = runtime.task_id();
            let change = move |tx: Tx, _| async move {
                let id = spawn(&tx, &nodes, owner, child).await?;
                sink.lock().unwrap().push(id);
                Ok(if wait { wait_on(vec![id], JoinPolicy::AllSettled, 1) } else { resume_at(1) })
            };
            runtime.commit(change, &context).await
        }
    });
    found
}

/// Script `name`'s resume phase to wait for its abort signal.
pub fn resume_until_aborted(nodes: &Nodes, name: &str) {
    nodes.resume(name, |runtime, _, _| async move {
        aborted(runtime.signal()).await;
        Ok(())
    });
}
