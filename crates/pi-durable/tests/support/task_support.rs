//! Port of upstream `test/task-support.ts`: open a Harness over tasks, next states, and polling.

use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pi_chord::Json;
use pi_durable::harness::*;
use pi_durable::*;
use serde_json::Value;

use super::context;
use super::harness_support::chat_setup;

pub struct OpenedTasks {
    pub harness: Harness,
    pub registry: Arc<Registry>,
    pub reports: Arc<Mutex<Vec<DurableError>>>,
    /// The info of each report, in order.
    pub infos: Arc<Mutex<Vec<ReportInfo>>>,
}

impl OpenedTasks {
    pub fn infos(&self) -> Vec<ReportInfo> {
        self.infos.lock().unwrap().clone()
    }
}

/// Open a Harness whose registry holds `tasks`; failures passed to `on_report` are collected, their infos in `infos`.
pub async fn open_tasks(storage: Arc<dyn Storage>, tasks: Vec<Task>) -> OpenedTasks {
    open_tasks_with(storage, tasks, Arc::new(create_registry()), |options| options).await
}

pub async fn open_tasks_with(
    storage: Arc<dyn Storage>,
    tasks: Vec<Task>,
    registry: Arc<Registry>,
    configure: impl FnOnce(HarnessOptions) -> HarnessOptions,
) -> OpenedTasks {
    if !tasks.is_empty() {
        registry.install(tasks.into_iter().fold(Extension::new("tasks"), Extension::task)).unwrap();
    }
    let reports = Arc::new(Mutex::new(Vec::new()));
    let infos = Arc::new(Mutex::new(Vec::new()));
    let (sink, info_sink) = (reports.clone(), infos.clone());
    let options =
        HarnessOptions::new(chat_setup(Default::default()).models, registry.clone() as Arc<dyn RegistryReader>)
            .with_on_report(move |report| {
                sink.lock().unwrap().push(report.error.clone());
                info_sink.lock().unwrap().push(report.info.clone());
            });
    let harness = Harness::open(storage, configure(options), &context()).await.unwrap();
    OpenedTasks { harness, registry, reports, infos }
}

/// Next state that completes a task with `result`.
pub fn completed(result: Value) -> Option<TaskState> {
    Some(TaskState::Terminal { outcome: TaskOutcome::Completed { result: Some(Json::from(result)) } })
}

/// Next state that aborts a task.
pub fn aborted_with(reason: &str) -> Option<TaskState> {
    Some(TaskState::Terminal { outcome: TaskOutcome::Aborted { reason: Some(reason.into()), result: None } })
}

pub fn running(checkpoint: Value) -> Option<TaskState> {
    Some(TaskState::Running { checkpoint: Json::from(checkpoint) })
}

/// Poll `check` until it holds, failing after about two seconds.
pub async fn eventually<F, Fut>(mut check: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    for _ in 0..400 {
        if check().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("Condition was not reached");
}

/// Wait until `signal` aborts.
pub async fn aborted(signal: pi_chord::AbortSignal) {
    let notify = Arc::new(tokio::sync::Notify::new());
    let waker = notify.clone();
    let notified = notify.notified();
    tokio::pin!(notified);
    notified.as_mut().enable();
    let id = signal.add_listener(move |_| waker.notify_one());
    if !signal.aborted() {
        notified.await;
    }
    signal.remove_listener(id);
}
