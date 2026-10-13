//! One ACP session over a pi session (upstream `PiAcpSession`): prompt turns with a client-side
//! queue, cancellation, titles, context usage, and the startup info block.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use agent_client_protocol_schema::{
    ContentBlock, ContentChunk, Error, Meta, SessionInfoUpdate, SessionNotification, SessionUpdate, TextContent,
    UsageUpdate,
};
use serde_json::{Value, json};
use tokio::sync::oneshot;

use super::backend::{AcpClient, PiBackend};
use super::errors::maybe_auth_required;
use crate::agent_session::lock;
use crate::session::ids::iso_timestamp;

/// How long a context usage query may take before the turn completes without it.
pub const SESSION_STATS_TIMEOUT: Duration = Duration::from_secs(1);

/// How a turn ended; `Error` maps to `end_turn` or `cancelled` in the prompt response.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TurnEnd {
    EndTurn,
    Cancelled,
    Error,
}

pub type TurnResult = Result<TurnEnd, Error>;

struct PendingTurn {
    generation: u64,
    sender: oneshot::Sender<TurnResult>,
}

pub(super) struct QueuedTurn {
    message: String,
    images: Vec<pi_ai::ContentBlock>,
    title: Option<String>,
    sender: oneshot::Sender<TurnResult>,
}

/// A tool call's status; it only moves forward.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ToolPhase {
    Pending,
    InProgress,
}

/// File contents before an edit or write, for the structured diff.
pub(super) struct Snapshot {
    pub path: String,
    pub old_text: Option<String>,
}

#[derive(Default)]
pub(super) struct Inner {
    startup_info: Option<String>,
    startup_info_sent: bool,
    initial_title_pending: bool,
    /// `None` until a title was published; then the last one (`Some(None)` for cleared).
    last_title: Option<Option<String>>,
    cancel_requested: bool,
    pending: Option<PendingTurn>,
    queue: VecDeque<QueuedTurn>,
    generation: u64,
    pub tool_calls: HashMap<String, ToolPhase>,
    pub in_agent_loop: bool,
    pub snapshots: HashMap<String, Snapshot>,
    pub mutations: HashSet<String>,
    pub bash_calls: HashSet<String>,
    pub bash_output: HashMap<String, String>,
}

/// Construction parameters of [`AcpSession`].
pub struct AcpSessionParams {
    pub session_id: String,
    pub cwd: PathBuf,
    pub backend: Arc<dyn PiBackend>,
    pub client: Arc<dyn AcpClient>,
    /// Name the session after its first prompt (new sessions, not loaded ones).
    pub auto_title: bool,
}

pub struct AcpSession {
    pub session_id: String,
    pub cwd: PathBuf,
    pub backend: Arc<dyn PiBackend>,
    client: Arc<dyn AcpClient>,
    pub(super) inner: Mutex<Inner>,
    titles: tokio::sync::Mutex<()>,
    weak: Weak<AcpSession>,
}

pub(super) fn text_chunk(text: impl Into<String>) -> SessionUpdate {
    SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(TextContent::new(text))))
}

pub(super) fn meta_of(value: Value) -> Meta {
    match value {
        Value::Object(map) => map,
        _ => Meta::new(),
    }
}

fn queue_info(depth: usize, running: bool) -> SessionUpdate {
    let meta = meta_of(json!({"piAcp": {"queueDepth": depth, "running": running}}));
    SessionUpdate::SessionInfoUpdate(SessionInfoUpdate::new().meta(meta))
}

fn safe_integer(value: &Value) -> Option<u64> {
    let number = value.as_f64()?;
    (number.fract() == 0.0 && (0.0..=9_007_199_254_740_991.0).contains(&number)).then_some(number as u64)
}

/// pi's `contextUsage` as `usage_update`; `None` when the token count is unknown or invalid.
pub fn to_usage_update(stats: &Value) -> Option<UsageUpdate> {
    let usage = stats.get("contextUsage")?;
    let used = safe_integer(usage.get("tokens")?)?;
    let size = safe_integer(usage.get("contextWindow")?).filter(|size| *size > 0)?;
    Some(UsageUpdate::new(used, size))
}

impl AcpSession {
    pub fn new(params: AcpSessionParams) -> Arc<Self> {
        let inner = Inner { initial_title_pending: params.auto_title, ..Inner::default() };
        Arc::new_cyclic(|weak| Self {
            session_id: params.session_id,
            cwd: params.cwd,
            backend: params.backend,
            client: params.client,
            inner: Mutex::new(inner),
            titles: tokio::sync::Mutex::new(()),
            weak: weak.clone(),
        })
    }

    /// Feed pi's events to [`Self::handle_pi_event`] until the backend stops.
    pub fn start_event_pump(&self) {
        let Some(mut events) = self.backend.take_events() else { return };
        let weak = self.weak.clone();
        tokio::spawn(async move {
            while let Some(event) = events.recv().await {
                let Some(session) = weak.upgrade() else { return };
                session.handle_pi_event(event).await;
            }
        });
    }

    pub(super) fn this(&self) -> Arc<Self> {
        self.weak.upgrade().expect("a session outlives its own calls")
    }

    pub(super) fn emit(&self, update: SessionUpdate) {
        self.client.session_update(SessionNotification::new(self.session_id.clone(), update));
    }

    pub(super) fn client(&self) -> &Arc<dyn AcpClient> {
        &self.client
    }

    pub fn set_startup_info(&self, text: String) {
        let mut inner = lock(&self.inner);
        inner.startup_info = Some(text);
        inner.startup_info_sent = false;
    }

    /// Send the startup info once, outside a prompt turn.
    pub fn send_startup_info_if_pending(&self) {
        let text = {
            let mut inner = lock(&self.inner);
            if inner.startup_info_sent || inner.startup_info.is_none() {
                return;
            }
            inner.startup_info_sent = true;
            inner.startup_info.clone().unwrap_or_default()
        };
        self.emit(text_chunk(text));
    }

    /// Run a prompt, or queue it behind the running turn; resolves when its turn ends.
    pub async fn prompt(&self, message: String, images: Vec<pi_ai::ContentBlock>, title: Option<String>) -> TurnResult {
        let (sender, receiver) = oneshot::channel();
        let turn = QueuedTurn { message, images, title, sender };
        let queued_at = {
            let mut inner = lock(&self.inner);
            if inner.pending.is_some() {
                inner.queue.push_back(turn);
                Some(inner.queue.len())
            } else {
                self.start_turn(&mut inner, turn);
                None
            }
        };
        if let Some(position) = queued_at {
            self.emit(text_chunk(format!("Queued message (position {position}).")));
            self.emit(queue_info(position, true));
        }
        receiver.await.unwrap_or(Ok(TurnEnd::Cancelled))
    }

    fn start_turn(&self, inner: &mut Inner, turn: QueuedTurn) {
        inner.cancel_requested = false;
        inner.in_agent_loop = false;
        inner.generation += 1;
        let generation = inner.generation;
        inner.pending = Some(PendingTurn { generation, sender: turn.sender });
        let depth = inner.queue.len();
        let session = self.this();
        let title = turn.title;
        tokio::spawn(async move { session.maybe_auto_title(title).await });
        self.emit(queue_info(depth, true));
        let session = self.this();
        let (message, images) = (turn.message, turn.images);
        tokio::spawn(async move {
            if let Err(error) = session.backend.prompt(message, images).await {
                session.fail_turn(generation, &error);
            }
        });
    }

    /// pi rejected the prompt: report AUTH_REQUIRED for credential errors, else end the turn.
    fn fail_turn(&self, generation: u64, error: &anyhow::Error) {
        let depth = {
            let mut inner = lock(&self.inner);
            if inner.pending.as_ref().is_none_or(|pending| pending.generation != generation) {
                return;
            }
            let pending = inner.pending.take().expect("checked above");
            let result = match maybe_auth_required(error) {
                Some(auth) => Err(auth),
                None if inner.cancel_requested => Ok(TurnEnd::Cancelled),
                None => Ok(TurnEnd::Error),
            };
            let _ = pending.sender.send(result);
            inner.in_agent_loop = false;
            inner.queue.len()
        };
        self.emit(queue_info(depth, false));
    }

    /// `agent_settled`: publish usage, end the turn, and start the next queued prompt.
    pub(super) async fn settle_turn(&self) {
        self.publish_context_usage().await;
        let next = {
            let mut inner = lock(&self.inner);
            let reason = if inner.cancel_requested { TurnEnd::Cancelled } else { TurnEnd::EndTurn };
            if let Some(pending) = inner.pending.take() {
                let _ = pending.sender.send(Ok(reason));
            }
            inner.in_agent_loop = false;
            inner.queue.pop_front().map(|turn| (turn, inner.queue.len()))
        };
        match next {
            Some((turn, remaining)) => {
                self.emit(text_chunk(format!("Starting queued message. ({remaining} remaining)")));
                let mut inner = lock(&self.inner);
                self.start_turn(&mut inner, turn);
            }
            None => self.emit(queue_info(0, false)),
        }
    }

    /// Abort the running turn and drop queued prompts.
    pub async fn cancel(&self) {
        let cleared = {
            let mut inner = lock(&self.inner);
            inner.cancel_requested = true;
            let queued: Vec<QueuedTurn> = inner.queue.drain(..).collect();
            (!queued.is_empty()).then(|| (queued, inner.pending.is_some()))
        };
        if let Some((queued, running)) = cleared {
            for turn in queued {
                let _ = turn.sender.send(Ok(TurnEnd::Cancelled));
            }
            self.emit(text_chunk("Cleared queued prompts."));
            self.emit(queue_info(0, running));
        }
        if let Err(error) = self.backend.abort().await {
            eprintln!("Warning: could not abort the pi session: {error:#}");
        }
    }

    pub fn was_cancel_requested(&self) -> bool {
        lock(&self.inner).cancel_requested
    }

    /// `/name`: set pi's session name and publish it as the title.
    pub async fn set_session_name(&self, name: &str) -> anyhow::Result<()> {
        let _order = self.titles.lock().await;
        self.backend.set_session_name(name).await?;
        self.publish_title(Some(name.to_string()), Some(iso_timestamp()));
        Ok(())
    }

    /// Send `session_info_update` with the title, unless it is unchanged.
    pub fn publish_title(&self, title: Option<String>, updated_at: Option<String>) {
        {
            let mut inner = lock(&self.inner);
            if inner.last_title.as_ref() == Some(&title) {
                return;
            }
            inner.last_title = Some(title.clone());
        }
        let mut update = SessionInfoUpdate::new().title(title);
        if let Some(updated_at) = updated_at {
            update = update.updated_at(updated_at);
        }
        self.emit(SessionUpdate::SessionInfoUpdate(update));
    }

    /// Name a new session after its first prompt unless it already has a name.
    async fn maybe_auto_title(&self, title: Option<String>) {
        let Some(title) = title else { return };
        {
            let mut inner = lock(&self.inner);
            if !inner.initial_title_pending {
                return;
            }
            inner.initial_title_pending = false;
        }
        let _order = self.titles.lock().await;
        let Ok(state) = self.backend.get_state().await else { return };
        let current = state["sessionName"].as_str().map(str::trim).unwrap_or_default();
        if !current.is_empty() {
            self.publish_title(Some(current.to_string()), Some(iso_timestamp()));
            return;
        }
        let named = lock(&self.inner).last_title.clone().flatten().is_some_and(|title| !title.is_empty());
        if named {
            return;
        }
        if self.backend.set_session_name(&title).await.is_ok() {
            self.publish_title(Some(title), Some(iso_timestamp()));
        }
    }

    /// Send pi's context occupancy as `usage_update`; failures and timeouts are ignored.
    pub async fn publish_context_usage(&self) {
        let stats = tokio::time::timeout(SESSION_STATS_TIMEOUT, self.backend.get_session_stats()).await;
        if let Ok(Ok(stats)) = stats
            && let Some(update) = to_usage_update(&stats)
        {
            self.emit(SessionUpdate::UsageUpdate(update));
        }
    }

    /// Stop the pi session.
    pub async fn dispose(&self) {
        self.backend.dispose().await;
    }
}
