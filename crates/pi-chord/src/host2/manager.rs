//! Port of upstream `manager.ts`: runs a child host process and keeps at most one instance of it alive. The child
//! side is [`ChildControl`]; [`ForkDriver`] spawns real processes; [`process_lock`](super::process_lock) keeps one
//! live child per lock name.

mod control;
mod fork;
mod os;

use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};
use tokio::time::{Instant, MissedTickBehavior};

pub use control::{ChildControl, ControlMessage, IPC_ENV, LOCK_ENV, ShutdownHandler, StatusReport};
pub use fork::{ForkDriver, ForkDriverOptions, MessageTap, OutputStream, OutputTap};

use super::errors::{ChordErrorCode, ErrorOptions};
use super::link::Link;
use crate::error::{ChordError, ChordResult};
use crate::sync::{BoxFuture, lock};

/// What the manager knows of its child's work, for [`HostManagerOptions::keep_alive`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Liveness {
    /// Started but not ready yet.
    pub booting: bool,
    pub active_calls: u64,
    /// Viewers the child reports (it counts heartbeating subscribers).
    pub viewers: u64,
    pub durable_tasks: u64,
    /// Time since the child last reported work (calls, viewers or tasks), or since it became ready.
    pub idle_for: Duration,
}

/// How a child ended: its exit code, or the name of the signal that ended it (`SIGKILL`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Exit {
    pub code: Option<i32>,
    pub signal: Option<String>,
}

impl Exit {
    /// Upstream's `exit.code ?? exit.signal`.
    fn describe(&self) -> String {
        match (&self.code, &self.signal) {
            (Some(code), _) => code.to_string(),
            (None, Some(signal)) => signal.clone(),
            (None, None) => "null".into(),
        }
    }
}

pub type ControlListener = Arc<dyn Fn(&ControlMessage) + Send + Sync>;

/// A running child: its link (for the host's `accept`) and its exit.
pub trait Instance: Send + Sync {
    fn pid(&self) -> u32;
    fn link(&self) -> Arc<dyn Link>;
    /// Resolves when the child exited (at once when it already has).
    fn exited(&self) -> BoxFuture<Exit>;
    fn send(&self, message: &ControlMessage);
    /// Kills the child and everything it spawned.
    fn kill_tree(&self);
    fn on_control(&self, listener: ControlListener);
}

pub trait Driver: Send + Sync {
    fn spawn(&self) -> ChordResult<Arc<dyn Instance>>;
}

pub type KeepAlive = Arc<dyn Fn(&Liveness) -> bool + Send + Sync>;
pub type ExitListener = Arc<dyn Fn(&Exit) + Send + Sync>;

pub struct HostManagerOptions {
    pub driver: Arc<dyn Driver>,
    /// Evaluated every `check_interval` while running; `false` stops the child.
    pub keep_alive: Option<KeepAlive>,
    pub check_interval: Duration,
    /// Time a child may take to shut down before its process tree is killed.
    pub grace: Duration,
    /// Time to wait for `ready`.
    pub ready_timeout: Duration,
    /// The child exited on its own (crash, fatal) or after a stop.
    pub on_exit: Option<ExitListener>,
}

impl HostManagerOptions {
    /// Upstream's defaults: checks every second, 5 s grace, 30 s to get ready.
    pub fn new(driver: Arc<dyn Driver>) -> Self {
        Self {
            driver,
            keep_alive: None,
            check_interval: Duration::from_secs(1),
            grace: Duration::from_secs(5),
            ready_timeout: Duration::from_secs(30),
            on_exit: None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Stopped,
    Starting,
    Running,
    Stopping,
}

#[derive(Clone, Copy, Default)]
struct Report {
    active_calls: u64,
    viewers: u64,
    durable_tasks: u64,
}

impl Report {
    fn busy(&self) -> bool {
        self.active_calls + self.viewers + self.durable_tasks > 0
    }
}

struct State {
    status: Status,
    instance: Option<Arc<dyn Instance>>,
    report: Report,
    /// When the child last became idle; `None` while it reports work.
    idle_since: Option<Instant>,
    booting: bool,
}

struct Shared {
    options: HostManagerOptions,
    state: Mutex<State>,
}

impl Shared {
    fn liveness(&self) -> Liveness {
        let state = lock(&self.state);
        Liveness {
            booting: state.booting,
            active_calls: state.report.active_calls,
            viewers: state.report.viewers,
            durable_tasks: state.report.durable_tasks,
            idle_for: state.idle_since.map_or(Duration::ZERO, |since| since.elapsed()),
        }
    }

    fn record(&self, status: &StatusReport) {
        let mut state = lock(&self.state);
        state.report = Report {
            active_calls: status.active_calls.unwrap_or(0),
            viewers: status.viewers.unwrap_or(0),
            durable_tasks: status.durable_tasks.unwrap_or(0),
        };
        // The idle clock starts at the busy to idle transition, not at the last busy report.
        if state.report.busy() {
            state.idle_since = None;
        } else {
            state.idle_since.get_or_insert_with(Instant::now);
        }
    }

    /// Forgets `current` if it is still the instance.
    fn forget(&self, current: &Arc<dyn Instance>) {
        let mut state = lock(&self.state);
        if state.instance.as_ref().is_some_and(|instance| Arc::ptr_eq(instance, current)) {
            state.instance = None;
            state.status = Status::Stopped;
        }
    }
}

type Reply<T> = oneshot::Sender<ChordResult<T>>;

enum Job {
    Start(Reply<Arc<dyn Instance>>),
    Stop(oneshot::Sender<()>),
    Replace(Reply<Arc<dyn Instance>>),
}

/// Starts, stops and replaces one child; the steps run one at a time, in call order, each to its end even when
/// its caller stops waiting (like upstream's promise chain).
#[derive(Clone)]
pub struct HostManager {
    shared: Arc<Shared>,
    jobs: mpsc::UnboundedSender<Job>,
}

impl HostManager {
    /// Must be called within a Tokio runtime.
    pub fn new(options: HostManagerOptions) -> Self {
        let state = State {
            status: Status::Stopped,
            instance: None,
            report: Report::default(),
            idle_since: None,
            booting: false,
        };
        let shared = Arc::new(Shared { options, state: Mutex::new(state) });
        let (jobs, queue) = mpsc::unbounded_channel();
        tokio::spawn(run(shared.clone(), queue));
        Self { shared, jobs }
    }

    pub fn status(&self) -> Status {
        lock(&self.shared.state).status
    }

    pub fn instance(&self) -> Option<Arc<dyn Instance>> {
        lock(&self.shared.state).instance.clone()
    }

    pub fn liveness(&self) -> Liveness {
        self.shared.liveness()
    }

    /// Starts the child (or returns the running one) once it reported ready.
    pub async fn start(&self) -> ChordResult<Arc<dyn Instance>> {
        let (reply, result) = oneshot::channel();
        let _ = self.jobs.send(Job::Start(reply));
        result.await.unwrap_or_else(|_| Err(gone()))
    }

    /// Asks the child to shut down, then kills its process tree after the grace period.
    pub async fn stop(&self) {
        let (reply, done) = oneshot::channel();
        let _ = self.jobs.send(Job::Stop(reply));
        let _ = done.await;
    }

    /// Stop, wait for the exit, start.
    pub async fn replace(&self) -> ChordResult<Arc<dyn Instance>> {
        let (reply, result) = oneshot::channel();
        let _ = self.jobs.send(Job::Replace(reply));
        result.await.unwrap_or_else(|_| Err(gone()))
    }
}

fn gone() -> ChordError {
    ChordError::code_only(ChordErrorCode::Unavailable, "host manager stopped working")
}

fn unavailable(message: impl AsRef<str>, reason: Option<String>) -> ChordError {
    ChordError::coded(ChordErrorCode::Unavailable, message, ErrorOptions { reason, ..ErrorOptions::default() })
}

/// Runs the jobs in order and, between them, the keep-alive check.
async fn run(shared: Arc<Shared>, mut queue: mpsc::UnboundedReceiver<Job>) {
    let period = shared.options.check_interval.max(Duration::from_millis(1));
    let mut ticker = tokio::time::interval_at(Instant::now() + period, period);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let checking = shared.options.keep_alive.is_some();
    loop {
        tokio::select! {
            biased;
            job = queue.recv() => {
                let Some(job) = job else { return };
                if run_job(&shared, job).await {
                    ticker.reset();
                }
            }
            _ = ticker.tick(), if checking => check(&shared).await,
        }
    }
}

/// Whether a new child started.
async fn run_job(shared: &Arc<Shared>, job: Job) -> bool {
    match job {
        Job::Start(reply) => {
            let result = start(shared).await;
            let started = matches!(result, Ok((_, true)));
            let _ = reply.send(result.map(|(instance, _)| instance));
            started
        }
        Job::Stop(reply) => {
            stop(shared).await;
            let _ = reply.send(());
            false
        }
        Job::Replace(reply) => {
            stop(shared).await;
            let result = start(shared).await;
            let started = result.is_ok();
            let _ = reply.send(result.map(|(instance, _)| instance));
            started
        }
    }
}

async fn check(shared: &Arc<Shared>) {
    let Some(keep_alive) = &shared.options.keep_alive else { return };
    if lock(&shared.state).status != Status::Running {
        return;
    }
    if !keep_alive(&shared.liveness()) {
        stop(shared).await;
    }
}

async fn stop(shared: &Arc<Shared>) {
    let current = {
        let mut state = lock(&shared.state);
        let Some(current) = state.instance.clone() else {
            state.status = Status::Stopped;
            state.booting = false;
            return;
        };
        state.status = Status::Stopping;
        current
    };
    current.send(&ControlMessage::Shutdown);
    let graceful = tokio::time::timeout(shared.options.grace, current.exited()).await.is_ok();
    if !graceful {
        current.kill_tree();
    }
    current.exited().await;
    shared.forget(&current);
    lock(&shared.state).status = Status::Stopped;
}

/// The instance, and whether it is a new one.
async fn start(shared: &Arc<Shared>) -> ChordResult<(Arc<dyn Instance>, bool)> {
    {
        let mut state = lock(&shared.state);
        if let (Some(instance), Status::Running) = (&state.instance, state.status) {
            return Ok((instance.clone(), false));
        }
        state.status = Status::Starting;
        state.booting = true;
        state.report = Report::default();
    }
    let spawned = shared.options.driver.spawn();
    let current = match spawned {
        Ok(current) => current,
        Err(error) => {
            let mut state = lock(&shared.state);
            state.status = Status::Stopped;
            state.booting = false;
            return Err(error);
        }
    };
    lock(&shared.state).instance = Some(current.clone());
    watch_exit(shared, &current);
    if let Err(error) = ready(shared, &current).await {
        current.kill_tree();
        current.exited().await;
        shared.forget(&current);
        lock(&shared.state).booting = false;
        return Err(error);
    }
    let mut state = lock(&shared.state);
    state.booting = false;
    if !state.report.busy() {
        state.idle_since.get_or_insert_with(Instant::now);
    }
    state.status = Status::Running;
    Ok((current, true))
}

/// When the child exits, whatever the cause, the manager forgets it and tells `on_exit`.
fn watch_exit(shared: &Arc<Shared>, current: &Arc<dyn Instance>) {
    let weak: Weak<Shared> = Arc::downgrade(shared);
    let on_exit = shared.options.on_exit.clone();
    let instance = current.clone();
    tokio::spawn(async move {
        let exit = instance.exited().await;
        if let Some(shared) = weak.upgrade() {
            shared.forget(&instance);
        }
        if let Some(on_exit) = on_exit {
            on_exit(&exit);
        }
    });
}

/// Waits for `ready`; a `fatal`, an exit or the timeout fails the start. Status reports update the liveness.
async fn ready(shared: &Arc<Shared>, current: &Arc<dyn Instance>) -> ChordResult<()> {
    let (settle, settled) = oneshot::channel::<ChordResult<()>>();
    let settle = Mutex::new(Some(settle));
    let weak = Arc::downgrade(shared);
    current.on_control(Arc::new(move |message| {
        let outcome = match message {
            ControlMessage::Ready => Ok(()),
            ControlMessage::Fatal { message, reason } => {
                Err(unavailable(format!("child failed: {message}"), reason.clone()))
            }
            ControlMessage::Status(status) => {
                if let Some(shared) = weak.upgrade() {
                    shared.record(status);
                }
                return;
            }
            ControlMessage::Init { .. } | ControlMessage::Shutdown => return,
        };
        if let Some(settle) = lock(&settle).take() {
            let _ = settle.send(outcome);
        }
    }));
    let settled = async move {
        match settled.await {
            Ok(outcome) => outcome,
            Err(_) => std::future::pending().await,
        }
    };
    let exited = current.exited();
    tokio::select! {
        biased;
        outcome = settled => outcome,
        exit = exited => Err(unavailable(format!("child exited before ready ({})", exit.describe()), None)),
        () = tokio::time::sleep(shared.options.ready_timeout) => Err(unavailable("child not ready in time", None)),
    }
}
