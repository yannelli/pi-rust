//! Shared helpers of the compaction cases: a scripted faux model with separate agent and summary queues whose steps
//! may wait, and a chat opened with small compaction thresholds.
#![allow(dead_code, unused_imports)]

use std::collections::VecDeque;
pub use std::sync::{Arc, Mutex};

pub use pi_ai::{AssistantMessage, CacheRetention, Message, StopReason};
use pi_chord::BoxFuture;
pub use pi_chord::Json;
pub use pi_durable::harness::*;
pub use pi_durable::models::SimpleStreamOptions;
pub use pi_durable::storage::MemoryStorage;
pub use pi_durable::testing::faux::{
    FauxMessageOptions, FauxModelDefinition, FauxResponseStep, RegisterFauxProviderOptions, faux_assistant_message,
    faux_tool_call_with_id,
};
pub use pi_durable::{EntryRecord, Storage, SubmissionId, SubmissionStatus, TaskId, TaskOutcome};
pub use serde_json::{Value, json};

pub use super::support::Deferred;
pub use super::support::harness_support::*;

/// Text of about `tokens` estimated tokens, starting with `label`.
pub fn text(label: &str, tokens: usize) -> String {
    format!("{label} {}", "x".repeat((tokens * 4).saturating_sub(label.len() + 1)))
}

#[derive(Clone)]
pub struct Request {
    pub messages: Vec<Message>,
    pub options: SimpleStreamOptions,
    pub model: String,
}

pub type StepFn = Arc<dyn Fn(Request) -> BoxFuture<Result<AssistantMessage, String>> + Send + Sync>;

pub enum Step {
    Message(Box<AssistantMessage>),
    Run(StepFn),
}

impl From<AssistantMessage> for Step {
    fn from(message: AssistantMessage) -> Self {
        Step::Message(Box::new(message))
    }
}

/// Scripted faux model that answers agent requests and summarization requests from separate queues.
#[derive(Clone, Default)]
pub struct Script {
    agent: Arc<Mutex<VecDeque<Step>>>,
    summaries: Arc<Mutex<VecDeque<Step>>>,
    agent_requests: Arc<Mutex<Vec<Request>>>,
    summary_requests: Arc<Mutex<Vec<Request>>>,
}

impl Script {
    pub fn agent(&self, step: impl Into<Step>) {
        self.agent.lock().unwrap().push_back(step.into());
    }

    pub fn summary(&self, step: impl Into<Step>) {
        self.summaries.lock().unwrap().push_back(step.into());
    }

    pub fn agent_requests(&self) -> Vec<Request> {
        self.agent_requests.lock().unwrap().clone()
    }

    pub fn summary_requests(&self) -> Vec<Request> {
        self.summary_requests.lock().unwrap().clone()
    }

    pub fn last_agent_request(&self) -> Vec<Message> {
        self.agent_requests.lock().unwrap().last().expect("an agent request").messages.clone()
    }
}

pub fn is_summary_request(messages: &[Message]) -> bool {
    let Some(Message::System(system)) = messages.first() else { return false };
    matches!(&system.content, pi_ai::UserContent::Text(text) if text.starts_with("You are a context summarization assistant"))
}

pub fn script(setup: &ChatSetup) -> Script {
    let script = Script::default();
    let steps = (0..500).map(|_| {
        let script = script.clone();
        FauxResponseStep::async_factory(move |transcript, options, _, model| {
            let request =
                Request { messages: transcript.messages.clone(), options: options.clone(), model: model.id.clone() };
            let summary = is_summary_request(&request.messages);
            let (requests, queue) = if summary {
                (&script.summary_requests, &script.summaries)
            } else {
                (&script.agent_requests, &script.agent)
            };
            requests.lock().unwrap().push(request.clone());
            let step = queue.lock().unwrap().pop_front();
            Box::pin(async move {
                match step {
                    None => Err(format!("No scripted {} response", if summary { "summary" } else { "agent" })),
                    Some(Step::Message(message)) => Ok(*message),
                    Some(Step::Run(run)) => run(request).await,
                }
            })
        })
    });
    setup.faux.set_responses(steps.collect::<Vec<_>>());
    script
}

/// A step that waits for `gate`, or fails when the request is cancelled; `reached` resolves when it starts.
pub fn gated(gate: &Deferred, message: AssistantMessage, reached: Option<&Deferred>) -> Step {
    let (gate, reached) = (gate.clone(), reached.cloned());
    Step::Run(Arc::new(move |request: Request| {
        let (gate, reached, message, signal) = (gate.clone(), reached.clone(), message.clone(), request.options.signal);
        Box::pin(async move {
            if let Some(reached) = reached {
                reached.resolve();
            }
            tokio::select! {
                () = gate.wait() => Ok(message),
                reason = async { signal.unwrap().cancelled().await } => Err(reason.to_string()),
            }
        })
    }))
}

/// A step computed from its request.
pub fn step(build: impl Fn(&Request) -> AssistantMessage + Send + Sync + 'static) -> Step {
    Step::Run(Arc::new(move |request: Request| {
        let message = build(&request);
        Box::pin(async move { Ok(message) })
    }))
}

pub fn answer(content: &str) -> AssistantMessage {
    faux_assistant_message(content, FauxMessageOptions::default())
}

pub fn failure(error_message: &str) -> AssistantMessage {
    let options = FauxMessageOptions {
        stop_reason: Some(StopReason::Error),
        error_message: Some(error_message.into()),
        ..FauxMessageOptions::default()
    };
    faux_assistant_message("", options)
}

pub fn summary() -> AssistantMessage {
    answer("SUMMARY")
}

pub const OVERFLOW: &str = "prompt is too long: 250000 tokens > 200000 maximum";

/// Small thresholds: no automatic compaction unless a test enables it.
pub fn manual() -> CompactionOverrides {
    policy(false, 1000, 150, 0)
}

pub fn policy(enabled: bool, reserve: u64, keep: u64, background: u64) -> CompactionOverrides {
    CompactionOverrides {
        enabled: Some(enabled),
        reserve_tokens: Some(reserve),
        keep_recent_tokens: Some(keep),
        background_tokens: Some(background),
        model_overrides: None,
    }
}

#[derive(Clone)]
pub struct Chat {
    pub harness: Harness,
    pub root: Conversation,
    pub setup: Arc<ChatSetup>,
    pub faux: Script,
}

#[derive(Default)]
pub struct OpenOptions {
    pub policy: Option<CompactionOverrides>,
    pub context_window: Option<u64>,
    pub storage: Option<Arc<dyn Storage>>,
}

pub fn compaction_setup(context_window: Option<u64>) -> ChatSetup {
    let model = FauxModelDefinition {
        context_window: Some(context_window.unwrap_or(100_000)),
        max_tokens: Some(900),
        ..FauxModelDefinition::new("faux-1")
    };
    chat_setup(RegisterFauxProviderOptions { models: vec![model], ..Default::default() })
}

pub async fn open(options: OpenOptions) -> Chat {
    let setup = compaction_setup(options.context_window);
    let faux = script(&setup);
    open_with(options, setup, faux).await
}

pub async fn open_with(options: OpenOptions, setup: ChatSetup, faux: Script) -> Chat {
    add_section(&setup.registry, "preamble", "You are helpful.", Some(false));
    let storage = options.storage.unwrap_or_else(|| Arc::new(MemoryStorage::new()));
    let (harness, root) = open_chat(storage, &setup).await;
    {
        // These tests exercise compaction accounting and thresholds, not the faux provider's cache simulation.
        let mut settings = setup.settings.lock().unwrap();
        settings.stream =
            Some(ConversationStreamOptions { cache_retention: Some(CacheRetention::None), ..Default::default() });
        settings.compaction = Some(options.policy.unwrap_or_else(manual));
        settings.retry = Some(RetryOverrides {
            enabled: Some(true),
            max_retries: Some(2),
            base_delay_ms: Some(1),
            ..Default::default()
        });
    }
    harness.resume().unwrap();
    Chat { harness, root, setup: Arc::new(setup), faux }
}

impl Chat {
    /// Run one turn: `user` answered by `reply`.
    pub async fn turn(&self, user: &str, reply: &str) {
        self.faux.agent(answer(reply));
        let submission = self.root.submit(SubmissionDraft::input(user), &ctx()).await.unwrap();
        assert_eq!(submission.wait(&ctx()).await.unwrap().status, SubmissionStatus::Done);
    }

    /// Three turns of about 100-token messages.
    pub async fn history(&self) {
        self.turn(&text("u1", 100), &text("a1", 100)).await;
        self.turn(&text("u2", 100), &text("a2", 100)).await;
        self.turn(&text("u3", 100), &text("a3", 100)).await;
    }

    pub async fn compact(&self, instructions: Option<&str>) -> TaskId {
        self.root.compact(instructions.map(str::to_owned), &ctx()).await.unwrap()
    }

    pub async fn result(&self, id: TaskId) -> TaskOutcome {
        let record = self.harness.wait_for_task(id, &ctx()).await.unwrap();
        record.state.outcome().cloned().expect("a terminal task")
    }

    /// The outcome as upstream JSON.
    pub async fn outcome(&self, id: TaskId) -> Value {
        serde_json::to_value(self.result(id).await).unwrap()
    }

    /// The submission a completed compaction queued or placed.
    pub async fn placed(&self, id: TaskId) -> Submission {
        let outcome = self.outcome(id).await;
        let submission: SubmissionId = serde_json::from_value(outcome["result"]["submissionId"].clone()).unwrap();
        self.harness.submission(submission, &ctx()).await.unwrap().unwrap()
    }

    pub async fn kinds(&self) -> Vec<String> {
        kinds(&self.root).await
    }

    pub async fn live(&self) -> Value {
        let live = self.harness.snapshot(&pi_durable::harness::live::LIVE_DOC, self.root.id, &ctx()).await.unwrap();
        live.map_or_else(|| json!({}), |live| live.to_serde())
    }

    pub async fn submit(&self, text: &str) -> Submission {
        self.root.submit(SubmissionDraft::input(text), &ctx()).await.unwrap()
    }

    pub async fn close(&self) {
        self.harness.close(&ctx()).await.unwrap();
    }
}

pub async fn kinds(conversation: &Conversation) -> Vec<String> {
    all_entries(conversation).await.into_iter().map(|entry| entry.kind).collect()
}

pub fn tail<T: Clone>(items: &[T], count: usize) -> Vec<T> {
    items[items.len().saturating_sub(count)..].to_vec()
}

pub fn user_text(message: Option<&Message>) -> String {
    text_of(message).unwrap_or_default()
}

/// Text of the first user message; a leading baseline system message comes before it.
pub fn first_user_text(messages: &[Message]) -> String {
    user_text(messages.iter().find(|message| matches!(message, Message::User(_))))
}

pub fn roles(messages: &[Message]) -> Vec<&'static str> {
    messages
        .iter()
        .map(|message| match message {
            Message::User(_) => "user",
            Message::Assistant(_) => "assistant",
            Message::System(_) => "system",
            Message::ToolResult(_) => "toolResult",
            _ => "other",
        })
        .collect()
}

/// Background threshold at 500 and blocking threshold at 1500 tokens of a 2000-token window.
pub fn background() -> CompactionOverrides {
    policy(true, 500, 150, 1000)
}

/// Blocking threshold at 700 tokens of a 1000-token window, no background compaction.
pub fn blocking() -> CompactionOverrides {
    policy(true, 300, 150, 0)
}

pub fn window(context_window: u64) -> OpenOptions {
    OpenOptions { context_window: Some(context_window), ..OpenOptions::default() }
}

impl Chat {
    pub fn set_policy(&self, policy: CompactionOverrides) {
        self.setup.settings.lock().unwrap().compaction = Some(policy);
    }

    pub async fn compaction_tasks(&self) -> Vec<pi_durable::TaskRecord> {
        let inspection = self.harness.inspect(&ctx()).await.unwrap();
        inspection.tasks.into_iter().map(|task| task.record).filter(|record| record.kind == "pi.compaction").collect()
    }

    pub async fn first_text(&self) -> String {
        first_user_text(&self.root.context(&ctx(), None).await.unwrap().messages)
    }

    pub async fn run_task(&self) -> TaskId {
        serde_json::from_value(self.live().await["run"]["taskId"].clone()).unwrap()
    }
}

/// Every view of the root conversation from now on, unfolded.
pub async fn views(chat: &Chat) -> super::support::live_support::History {
    super::support::live_support::view_history(&chat.harness, chat.root.id).await
}

/// The `pi.live` value of the root conversation's late watch.
pub async fn watched_live(chat: &Chat) -> Value {
    let watch = chat.root.watch(&ctx(), None).await.unwrap();
    let live = watch.value().to_serde()["docs"]["pi.live"].clone();
    watch.stop().await;
    live
}
