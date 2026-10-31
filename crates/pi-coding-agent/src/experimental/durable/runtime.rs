//! Port of upstream `durable/runtime.ts` (and `vacation/runtime.ts`): the Harness, its registry and settings,
//! and the plain `DurableView` and `DurableController` the TUI uses. No Harness objects cross this boundary.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use pi_ai::Message;
use pi_chord::state::StateListener;
use pi_chord::{BACKGROUND_CONTEXT, Context, Unsubscribe};
use pi_durable::ROOT_CONVERSATION_ID;
use pi_durable::harness::types::{AgentState, ModelRef};
use pi_durable::harness::{Conversation, EntryRange, Harness};
use pi_durable::session::{CommitObserver, DocumentState, WatchEnd};
use pi_durable::{
    CommitChange, CommitPublication, ConversationId, ConversationQuery, Cursor, DurableResult, EntryRecord,
};
use serde_json::Value;
use tokio::sync::watch;

use super::controller::{ControllerState, DurableController};
use crate::agent_session::ModelRuntime;

/// Adds each conversation a commit stores, and fills in a conversation's title from its first input.
struct ConversationList {
    store: Arc<ViewStore>,
    root_id: ConversationId,
}

impl CommitObserver for ConversationList {
    fn advance(&self, publication: &CommitPublication, _context: &Context) -> DurableResult<()> {
        let mut changed = false;
        let mut conversations = self.store.current().conversations;
        for change in &publication.changes {
            match change {
                CommitChange::Conversation(record) => {
                    let label = label(self.root_id, record.id);
                    conversations.push(ConversationSummary { id: record.id, label, title: None });
                    changed = true;
                }
                CommitChange::Entry(entry) if entry.kind == "pi.user" => {
                    let id = entry.conversation_id;
                    if let Some(summary) = conversations.iter_mut().find(|s| s.id == id && s.title.is_none()) {
                        summary.title = title_of(Some(entry));
                        changed = true;
                    }
                }
                _ => {}
            }
        }
        if changed {
            self.store.update(|view| view.conversations = conversations);
        }
        Ok(())
    }

    fn end(&self, _end: WatchEnd) {}
}

#[derive(Clone, Debug, PartialEq)]
pub struct ModelSummary {
    pub provider: String,
    pub model_id: String,
    pub name: String,
    pub context_window: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoticeLevel {
    Info,
    Warning,
    Error,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Notice {
    pub id: u64,
    pub level: NoticeLevel,
    pub message: String,
}

/// A conversation the user can switch to: the main one, or a subagent's.
#[derive(Clone, Debug, PartialEq)]
pub struct ConversationSummary {
    pub id: ConversationId,
    pub label: String,
    /// The first user message, for a subagent its task.
    pub title: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SessionSummary {
    pub id: String,
    pub directory: PathBuf,
    pub cwd: String,
}

/// Everything the TUI renders, as plain values.
#[derive(Clone, Debug)]
pub struct DurableView {
    pub session: SessionSummary,
    /// The `ConversationView` JSON of the conversation shown and talked to.
    pub conversation: Value,
    pub conversations: Vec<ConversationSummary>,
    pub models: Vec<ModelSummary>,
    pub notices: Vec<Notice>,
    /// The live task graph JSON while the task panel is open.
    pub tasks: Option<Value>,
}

/// The agent document of a view; empty while the conversation has none.
pub fn agent_of(view: &Value) -> AgentState {
    serde_json::from_value(view["docs"]["pi.agent"].clone()).unwrap_or_default()
}

/// The view's conversation ID.
pub fn conversation_id_of(view: &Value) -> Option<ConversationId> {
    serde_json::from_value(view["conversation"]["id"].clone()).ok()
}

/// The text of a user entry, as a one-line title.
pub(crate) fn title_of(entry: Option<&EntryRecord>) -> Option<String> {
    let Some(Message::User(message)) = entry.and_then(|entry| entry.model.as_ref()?.first()) else { return None };
    let text = match &message.content {
        pi_ai::UserContent::Text(text) => text.clone(),
        pi_ai::UserContent::Blocks(blocks) => blocks
            .iter()
            .filter_map(|block| match block {
                pi_ai::ContentBlock::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(" "),
    };
    Some(text.split_whitespace().collect::<Vec<_>>().join(" "))
}

/// The view state and its change signal. Listeners coalesce bursts: a watch keeps only the newest version.
pub struct ViewStore {
    state: Mutex<DurableView>,
    version: watch::Sender<u64>,
    next_notice: Mutex<u64>,
}

impl ViewStore {
    pub(crate) fn new(view: DurableView) -> Self {
        Self { state: Mutex::new(view), version: watch::channel(0).0, next_notice: Mutex::new(1) }
    }

    pub fn current(&self) -> DurableView {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone()
    }

    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.version.subscribe()
    }

    pub(crate) fn update(&self, patch: impl FnOnce(&mut DurableView)) {
        patch(&mut self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner()));
        self.version.send_modify(|version| *version += 1);
    }

    pub(crate) fn notice(&self, level: NoticeLevel, message: impl Into<String>) {
        let id = {
            let mut next = self.next_notice.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            *next += 1;
            *next - 1
        };
        let message = message.into();
        self.update(|view| {
            view.notices.push(Notice { id, level, message });
            let excess = view.notices.len().saturating_sub(20);
            view.notices.drain(..excess);
        });
    }
}

/// Subscribe the store to a conversation's view state.
pub(crate) fn follow_conversation(store: &Arc<ViewStore>, state: &DocumentState) -> Unsubscribe {
    let store = store.clone();
    let initial = state.value().to_serde();
    store.update(|view| view.conversation = initial);
    state.subscribe(StateListener::sync(move |value, _, _| {
        let value = value.to_serde();
        store.update(|view| view.conversation = value);
        Ok(())
    }))
}

/// A subagent's task: the oldest user message of its conversation. The main conversation needs no title.
async fn first_input(harness: &Harness, id: ConversationId) -> DurableResult<Option<String>> {
    if id == ROOT_CONVERSATION_ID {
        return Ok(None);
    }
    let Some(conversation) = harness.conversation(id, &BACKGROUND_CONTEXT).await? else { return Ok(None) };
    let (mut first, mut cursor): (Option<EntryRecord>, Option<Cursor>) = (None, None);
    loop {
        let page = conversation.entries(EntryRange::default(), 256, cursor, &BACKGROUND_CONTEXT).await?;
        first = page.items.into_iter().rev().find(|entry| entry.kind == "pi.user").or(first);
        cursor = page.next;
        if cursor.is_none() {
            return Ok(title_of(first.as_ref()));
        }
    }
}

pub(crate) fn model_summaries(runtime: &ModelRuntime) -> Vec<ModelSummary> {
    let models = runtime.available_models().into_iter();
    models
        .map(|model| ModelSummary {
            provider: model.provider,
            model_id: model.id,
            name: model.name,
            context_window: model.context_window,
        })
        .collect()
}

pub(crate) fn label(root: ConversationId, id: ConversationId) -> String {
    if id == root { "main".into() } else { format!("subagent {id}") }
}

/// Open the view over an opened Harness and its root conversation, and start recovered work.
pub(crate) async fn open_view(
    harness: Harness,
    root: Conversation,
    model_runtime: Arc<ModelRuntime>,
    session: SessionSummary,
) -> DurableResult<(Arc<ViewStore>, DurableController)> {
    let root_id = root.id;
    let mut summaries = Vec::new();
    let mut cursor: Option<Cursor> = None;
    loop {
        let after = cursor.take();
        let page = harness
            .session()
            .commit(
                move |tx| async move { tx.scan_conversations(ConversationQuery::default(), 256, after).await },
                &BACKGROUND_CONTEXT,
            )
            .await?;
        for conversation in page.items {
            let title = first_input(&harness, conversation.id).await?;
            summaries.push(ConversationSummary { id: conversation.id, label: label(root_id, conversation.id), title });
        }
        cursor = page.next;
        if cursor.is_none() {
            break;
        }
    }
    let view_state = root.view_state(&BACKGROUND_CONTEXT).await?;
    let store = Arc::new(ViewStore::new(DurableView {
        session,
        conversation: view_state.value().to_serde(),
        conversations: summaries,
        models: model_summaries(&model_runtime),
        notices: Vec::new(),
        tasks: None,
    }));
    let unsubscribe = follow_conversation(&store, &view_state);
    // Subagents appear as their conversations are created. A commit observer only records.
    let observer = Arc::new(ConversationList { store: store.clone(), root_id });
    let commit_observation = harness.session().observe_commits(observer)?;

    let state = ControllerState::new(harness.clone(), model_runtime.clone(), store.clone(), root, view_state);
    state.set_subscriptions(unsubscribe, commit_observation);
    let controller = DurableController::start(state);
    let saved = agent_of(&store.current().conversation).model;
    match saved {
        None => store.notice(NoticeLevel::Warning, "No model configured; select one with /model."),
        Some(ModelRef { provider, model_id }) if model_runtime.get_model(&provider, &model_id).is_none() => {
            store.notice(NoticeLevel::Warning, format!("Saved model is unavailable: {provider}/{model_id}"));
        }
        Some(_) => {}
    }
    // The task panel starts open; /tasks hides it.
    let _ = controller.toggle_tasks().await;
    // Recovered work from an interrupted turn continues now.
    harness.resume()?;
    Ok((store, controller))
}
