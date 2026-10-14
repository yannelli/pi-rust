//! The `DurableController` of upstream `durable/runtime.ts`: what the TUI may ask for. Commands run one
//! at a time in call order, so toggles, switches, and key presses apply in order.

use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use pi_ai::ThinkingLevel;
use pi_ai::simple_options::{clamp_thinking_level, supported_thinking_levels};
use pi_chord::state::StateListener;
use pi_chord::{BACKGROUND_CONTEXT, Unsubscribe};
use pi_durable::harness::types::{AgentChange, Change, ModelRef, SubmissionDraft, WhenBusy};
use pi_durable::harness::{Conversation, Harness};
use pi_durable::session::DocumentState;
use pi_durable::{ConversationId, DurableError, DurableResult, SubmissionStatus};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

use super::runtime::{NoticeLevel, ViewStore, agent_of, follow_conversation};
use crate::agent_session::ModelRuntime;

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The Harness side of the controller.
pub struct ControllerState {
    harness: Harness,
    models: Arc<ModelRuntime>,
    store: Arc<ViewStore>,
    current: Mutex<Conversation>,
    view_state: Mutex<DocumentState>,
    unsubscribe: Mutex<Option<Unsubscribe>>,
    unsubscribe_commits: Mutex<Option<Unsubscribe>>,
    tasks: Mutex<Option<(DocumentState, Unsubscribe)>>,
}

impl ControllerState {
    pub(crate) fn new(
        harness: Harness,
        models: Arc<ModelRuntime>,
        store: Arc<ViewStore>,
        current: Conversation,
        view_state: DocumentState,
    ) -> Arc<Self> {
        Arc::new(Self {
            harness,
            models,
            store,
            current: Mutex::new(current),
            view_state: Mutex::new(view_state),
            unsubscribe: Mutex::default(),
            unsubscribe_commits: Mutex::default(),
            tasks: Mutex::default(),
        })
    }

    pub(crate) fn set_subscriptions(&self, view: Unsubscribe, commits: Unsubscribe) {
        *lock(&self.unsubscribe) = Some(view);
        *lock(&self.unsubscribe_commits) = Some(commits);
    }

    fn current(&self) -> Conversation {
        lock(&self.current).clone()
    }

    fn close_tasks(&self) {
        if let Some((graph, unsubscribe)) = lock(&self.tasks).take() {
            unsubscribe.unsubscribe();
            let _ = graph.dispose();
        }
    }

    /// Stop every subscription. Closing the Harness is the caller's.
    pub(crate) fn detach(&self) {
        for slot in [&self.unsubscribe, &self.unsubscribe_commits] {
            if let Some(unsubscribe) = lock(slot).take() {
                unsubscribe.unsubscribe();
            }
        }
        let _ = lock(&self.view_state).dispose();
        self.close_tasks();
    }

    fn agent_model(&self) -> DurableResult<pi_ai::Model> {
        let reference = agent_of(&self.store.current().conversation).model;
        let Some(reference) = reference else { return Err(DurableError::Error("No model selected".into())) };
        let model = self.models.get_model(&reference.provider, &reference.model_id);
        model.ok_or_else(|| DurableError::Error("Current model is unavailable".into()))
    }

    fn watch_answer(self: &Arc<Self>, submission: pi_durable::harness::submissions::Submission) {
        let store = self.store.clone();
        tokio::spawn(async move {
            match submission.wait(&BACKGROUND_CONTEXT).await {
                Ok(settled) if settled.status == SubmissionStatus::Unanswered => {
                    let reason = settled.reason.unwrap_or_default();
                    if reason != "aborted" {
                        let detail = settled.detail.map(|detail| format!(" {}", detail.to_serde())).unwrap_or_default();
                        store.notice(NoticeLevel::Error, format!("No answer: {reason}{detail}"));
                    }
                }
                Ok(_) => {}
                Err(error) => store.notice(NoticeLevel::Error, error.to_string()),
            }
        });
    }

    /// Report a compaction's outcome once it is known; the status line shows the compaction meanwhile.
    fn report_compaction(self: &Arc<Self>, id: pi_durable::TaskId) {
        let state = self.clone();
        tokio::spawn(async move {
            let result: DurableResult<()> = async {
                let receipt = state.harness.wait_for_task(id, &BACKGROUND_CONTEXT).await?;
                let receipt = serde_json::to_value(&receipt.state).unwrap_or_default();
                let outcome = &receipt["outcome"];
                match outcome["status"].as_str() {
                    Some("completed") => {
                        let (entry, submission) = (&outcome["result"]["entryId"], &outcome["result"]["submissionId"]);
                        // A summary written while busy is a submission: placed now, queued, or dropped as stale.
                        let status = match serde_json::from_value(submission.clone()) {
                            Ok(id) => match state.harness.submission(id, &BACKGROUND_CONTEXT).await? {
                                Some(found) => Some(found.status(&BACKGROUND_CONTEXT).await?.status),
                                None => None,
                            },
                            Err(_) => None,
                        };
                        let message = match status {
                            _ if !entry.is_null() => "Compacted.",
                            Some(SubmissionStatus::Done) => "Compacted.",
                            Some(SubmissionStatus::Queued) => {
                                "Compaction summary queued; it is placed at the next turn boundary."
                            }
                            Some(SubmissionStatus::Unanswered) => {
                                "Compaction summary dropped: the context changed under it."
                            }
                            _ => "Nothing to compact: the context fits in the recent window that is kept verbatim.",
                        };
                        state.store.notice(NoticeLevel::Info, message);
                    }
                    Some("aborted") => state.store.notice(NoticeLevel::Info, "Compaction aborted."),
                    status => {
                        let detail = outcome["error"]["message"].as_str().or(outcome["reason"].as_str());
                        let text = format!("Compaction {}: {}", status.unwrap_or("?"), detail.unwrap_or(""));
                        state.store.notice(NoticeLevel::Error, text);
                    }
                }
                Ok(())
            }
            .await;
            if let Err(error) = result {
                state.store.notice(NoticeLevel::Error, error.to_string());
            }
        });
    }
}

type Command = (BoxFuture<'static, DurableResult<()>>, oneshot::Sender<()>);

/// What the TUI may ask for. Each method queues its command at once and returns a future of its completion;
/// a failed command shows an error notice.
#[derive(Clone)]
pub struct DurableController {
    state: Arc<ControllerState>,
    commands: mpsc::UnboundedSender<Command>,
}

impl DurableController {
    pub(crate) fn start(state: Arc<ControllerState>) -> Self {
        let (commands, mut queue) = mpsc::unbounded_channel::<Command>();
        let store = state.store.clone();
        tokio::spawn(async move {
            while let Some((operation, done)) = queue.recv().await {
                if let Err(error) = operation.await {
                    store.notice(NoticeLevel::Error, error.to_string());
                }
                let _ = done.send(());
            }
        });
        Self { state, commands }
    }

    pub(crate) fn state(&self) -> &Arc<ControllerState> {
        &self.state
    }

    fn command(&self, operation: BoxFuture<'static, DurableResult<()>>) -> impl Future<Output = ()> + use<> {
        let (done, finished) = oneshot::channel();
        let _ = self.commands.send((operation, done));
        async move {
            let _ = finished.await;
        }
    }

    /// Prompt when idle; otherwise steer or queue a follow-up.
    pub fn submit(&self, text: String, when_busy: WhenBusy) -> impl Future<Output = ()> + use<> {
        let state = self.state.clone();
        self.command(Box::pin(async move {
            let draft = SubmissionDraft::Input {
                request_id: None,
                content: pi_ai::UserContent::Text(text),
                when_busy: Some(when_busy),
            };
            let submission = state.current().submit(draft, &BACKGROUND_CONTEXT).await?;
            state.watch_answer(submission);
            Ok(())
        }))
    }

    pub fn compact(&self, instructions: Option<String>) -> impl Future<Output = ()> + use<> {
        let state = self.state.clone();
        self.command(Box::pin(async move {
            let id = state.current().compact(instructions, &BACKGROUND_CONTEXT).await?;
            state.report_compaction(id);
            Ok(())
        }))
    }

    /// Not queued: it waits until the conversation is idle.
    pub fn abort(&self) -> impl Future<Output = ()> + use<> {
        let state = self.state.clone();
        async move {
            if let Err(error) = state.current().abort(&BACKGROUND_CONTEXT, false).await {
                state.store.notice(NoticeLevel::Error, error.to_string());
            }
        }
    }

    pub fn cycle_thinking(&self) -> impl Future<Output = ()> + use<> {
        let state = self.state.clone();
        self.command(Box::pin(async move {
            let model = state.agent_model()?;
            if !model.reasoning {
                return Err(DurableError::Error("Current model does not support thinking".into()));
            }
            let levels = supported_thinking_levels(&model);
            let level = agent_of(&state.store.current().conversation).thinking_level.unwrap_or(ThinkingLevel::Off);
            let index = levels.iter().position(|each| *each == level).map_or(0, |index| (index + 1) % levels.len());
            let next = levels.get(index).copied().unwrap_or(ThinkingLevel::Off);
            let change = AgentChange { thinking_level: Change::Set(next), ..AgentChange::default() };
            state.current().configure(change, &BACKGROUND_CONTEXT).await
        }))
    }

    pub fn set_model(&self, reference: ModelRef) -> impl Future<Output = ()> + use<> {
        let state = self.state.clone();
        self.command(Box::pin(async move {
            let Some(model) = state.models.get_model(&reference.provider, &reference.model_id) else {
                let message = format!("Unknown model: {}/{}", reference.provider, reference.model_id);
                return Err(DurableError::Error(message));
            };
            let thinking = agent_of(&state.store.current().conversation).thinking_level.unwrap_or(ThinkingLevel::Off);
            let mut change = AgentChange::model(reference);
            change.thinking_level = Change::Set(clamp_thinking_level(&model, thinking));
            state.current().configure(change, &BACKGROUND_CONTEXT).await
        }))
    }

    pub fn toggle_tasks(&self) -> impl Future<Output = ()> + use<> {
        let state = self.state.clone();
        self.command(Box::pin(async move {
            if lock(&state.tasks).is_some() {
                state.close_tasks();
                state.store.update(|view| view.tasks = None);
                return Ok(());
            }
            let graph = state.harness.task_graph(&BACKGROUND_CONTEXT).await?;
            let store = state.store.clone();
            let value = graph.value().to_serde();
            store.update(|view| view.tasks = Some(value));
            let unsubscribe = graph.subscribe(StateListener::sync(move |value, _, _| {
                let value: Value = value.to_serde();
                store.update(|view| view.tasks = Some(value));
                Ok(())
            }));
            *lock(&state.tasks) = Some((graph, unsubscribe));
            Ok(())
        }))
    }

    /// Show and talk to another conversation.
    pub fn switch_conversation(&self, id: ConversationId) -> impl Future<Output = ()> + use<> {
        let state = self.state.clone();
        self.command(Box::pin(async move {
            let next = state.harness.conversation(id, &BACKGROUND_CONTEXT).await?;
            let next = next.ok_or_else(|| DurableError::Error(format!("Conversation {id} does not exist")))?;
            let next_state = next.view_state(&BACKGROUND_CONTEXT).await?;
            if let Some(unsubscribe) = lock(&state.unsubscribe).take() {
                unsubscribe.unsubscribe();
            }
            let _ = lock(&state.view_state).dispose();
            *lock(&state.current) = next;
            let unsubscribe = follow_conversation(&state.store, &next_state);
            *lock(&state.view_state) = next_state;
            *lock(&state.unsubscribe) = Some(unsubscribe);
            Ok(())
        }))
    }
}
