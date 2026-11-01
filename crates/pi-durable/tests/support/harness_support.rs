//! Port of upstream `test/chat-support.ts` and `test/harness-support.ts`.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pi_ai::{ContentBlock, Message, UserContent, UserMessage};
use pi_chord::Context;
use pi_durable::harness::*;
use pi_durable::models::Models;
use pi_durable::testing::faux::{FauxProviderHandle, FauxResponseStep, RegisterFauxProviderOptions, faux_provider};
use pi_durable::{DurableError, EntryDraft, EntryRecord, ReportInfo, Storage, TaskQuery, TaskRecord};
use tokio::sync::Notify;

use super::context;

/// Models and registry that survive a close and reopen, like a host process's own objects.
#[derive(Clone)]
pub struct ChatSetup {
    pub faux: FauxProviderHandle,
    pub models: Arc<dyn Models>,
    pub registry: Arc<Registry>,
    pub reports: Arc<Mutex<Vec<DurableError>>>,
    /// What each report was about, in the order of `reports`.
    pub infos: Arc<Mutex<Vec<ReportInfo>>>,
    /// Live Harness settings; tests change them between decisions.
    pub settings: Arc<Mutex<HarnessSettings>>,
    /// Fixed clock in milliseconds; `None` reads the system clock.
    pub now: Arc<Mutex<Option<f64>>>,
    /// Added to the system clock when `now` is `None`, for a clock that moves but runs ahead.
    pub now_offset_ms: Arc<Mutex<f64>>,
}

pub fn chat_setup(options: RegisterFauxProviderOptions) -> ChatSetup {
    let faux = faux_provider(options);
    ChatSetup {
        models: faux.provider(),
        faux,
        registry: Arc::new(create_registry()),
        reports: Arc::default(),
        infos: Arc::default(),
        settings: Arc::default(),
        now: Arc::default(),
        now_offset_ms: Arc::default(),
    }
}

impl ChatSetup {
    pub fn options(&self) -> HarnessOptions {
        let settings = self.settings.clone();
        let (reports, infos) = (self.reports.clone(), self.infos.clone());
        let mut options = HarnessOptions::new(self.models.clone(), self.registry.clone() as Arc<dyn RegistryReader>)
            .with_on_report(move |report| {
                reports.lock().unwrap().push(report.error.clone());
                infos.lock().unwrap().push(report.info.clone());
            });
        let (now, offset) = (self.now.clone(), self.now_offset_ms.clone());
        options =
            options.with_now(move || now.lock().unwrap().unwrap_or_else(|| system_now() + *offset.lock().unwrap()));
        options.settings = Some(Arc::new(move || Ok(settings.lock().unwrap().clone())));
        options
    }

    pub fn reports(&self) -> Vec<String> {
        self.reports.lock().unwrap().iter().map(ToString::to_string).collect()
    }
}

pub fn faux_model() -> ModelRef {
    ModelRef { provider: "faux".into(), model_id: "faux-1".into() }
}

/// Open a Harness over `storage` and return its root, configured with the faux model on first creation.
pub async fn open_chat(storage: Arc<dyn Storage>, setup: &ChatSetup) -> (Harness, Conversation) {
    open_chat_with(storage, setup, setup.options()).await
}

pub async fn open_chat_with(
    storage: Arc<dyn Storage>,
    setup: &ChatSetup,
    options: HarnessOptions,
) -> (Harness, Conversation) {
    let _ = setup;
    let harness = Harness::open(storage, options, &context()).await.unwrap();
    let options = CreateOptions::default().with_agent(AgentChange::model(faux_model()));
    let root = harness.root(&context(), options).await.unwrap();
    (harness, root)
}

/// Raw entries of a conversation, oldest first.
pub async fn all_entries(conversation: &Conversation) -> Vec<EntryRecord> {
    let page = conversation.entries(EntryRange::default(), 1000, None, &context()).await.unwrap();
    page.items.into_iter().rev().collect()
}

pub fn kinds(entries: &[EntryRecord]) -> Vec<String> {
    entries.iter().map(|entry| entry.kind.clone()).collect()
}

/// A write that affects model context, so it queues while busy; a data-only write is appended at once.
pub fn queued_note(text: &str) -> EntryDraft {
    let message = Message::User(UserMessage { content: UserContent::Text(text.into()), timestamp: 0 });
    EntryDraft { model: Some(vec![message]), ..EntryDraft::new("note") }
}

/// Text of the first text content of a message.
pub fn text_of(message: Option<&Message>) -> Option<String> {
    let blocks = match message? {
        Message::User(user) => match &user.content {
            UserContent::Text(text) => return Some(text.clone()),
            UserContent::Blocks(blocks) => blocks.clone(),
        },
        Message::Assistant(assistant) => assistant.content.clone(),
        Message::ToolResult(result) => result.content.clone(),
        _ => return None,
    };
    blocks.iter().find_map(|block| match block {
        ContentBlock::Text { text, .. } => Some(text.clone()),
        _ => None,
    })
}

/// Poll `check` in real time until it holds; for waits that span throttle windows and timers.
pub async fn wait_for<F, Fut>(mut check: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    let deadline = Instant::now() + Duration::from_secs(5);
    while !check().await {
        assert!(Instant::now() < deadline, "Condition was not reached");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Faux response that never answers; the run stays busy until its generation is cancelled. `reached` fires once
/// the request was sent.
pub fn unanswered() -> (FauxResponseStep, Arc<Notify>) {
    let reached = Arc::new(Notify::new());
    let notify = reached.clone();
    let step = FauxResponseStep::async_factory(move |_, options, _, _| {
        let (notify, signal) = (notify.clone(), options.signal.clone());
        Box::pin(async move {
            notify.notify_one();
            match signal {
                Some(signal) => Err(signal.cancelled().await.to_string()),
                None => std::future::pending().await,
            }
        })
    });
    (step, reached)
}

/// Tasks of one conversation, oldest first.
pub async fn conversation_tasks(harness: &Harness, conversation: &Conversation) -> Vec<TaskRecord> {
    let query = TaskQuery { conversation_id: Some(conversation.id), ..TaskQuery::default() };
    let commit = harness.commit(move |tx| async move { tx.scan_tasks(query, 100, None).await }, &context());
    commit.await.unwrap().items
}

/// Install a one-section extension named after the section.
pub fn add_section(registry: &Registry, key: &str, text: &str, tag: Option<bool>) {
    let text = text.to_owned();
    let section = section(key, move |_, _| std::future::ready(Ok(Some(text.clone()))), tag);
    registry.install(Extension::new(format!("section:{key}")).section(section)).unwrap();
}

pub fn ctx() -> Context {
    context()
}

/// The task of the conversation's current run, once one exists.
pub async fn run_task(harness: &Harness, conversation: &Conversation) -> pi_durable::TaskId {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let live = harness.snapshot(&pi_durable::harness::live::LIVE_DOC, conversation.id, &context()).await.unwrap();
        if let Some(id) = live.and_then(|value| value.get("run")?.get("taskId")?.deserialize_into().ok()) {
            return id;
        }
        assert!(Instant::now() < deadline, "No run started");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

fn system_now() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() as f64
}
