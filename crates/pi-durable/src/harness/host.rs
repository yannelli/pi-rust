//! The Harness: a Session with conversation handles, a registry, and the task scheduler (upstream
//! `harness/harness.ts`).

use std::ops::Deref;
use std::sync::{Arc, OnceLock, Weak};

use async_trait::async_trait;
use futures::future::BoxFuture;
use pi_chord::{Context, without_abort_signal};

use super::agent::{configure, create_agent};
use super::conversation::{Conversation, CreateOptions, CreateTarget};
use super::core::{HarnessCore, HarnessOptions};
use super::inbox::INBOX_DOC;
use super::live::LIVE_DOC;
use super::registry::builtin_tasks;
use super::scheduler::{IndexSizes, Scheduler};
use super::submissions::{Submission, Submissions};
use super::task_graph::{TaskGraphView, TaskGraphWatch};
use super::telemetry::DurableTelemetry;
use super::types::{AbortSubmissionResult, AbortTaskResult, HarnessInspection, Wake};
use super::usage::{PROVIDER_DOC, USAGE_DOC, UsageState, add_usage_state};
use super::util::{cancellation, scan_all};
use super::view::ConversationViews;
use crate::errors::{DurableError, DurableResult};
use crate::ids::{ConversationId, ROOT_CONVERSATION_ID, SubmissionId, TaskId};
use crate::session::{
    Observation, Observer, Session, SessionHooks, SessionOptions, TransactionScope, Tx, create_session,
};
use crate::timers::Timers;
use crate::types::{
    ConversationOwnership, ConversationQuery, ConversationRecord, HostCallback, ReportInfo, Storage, SubmissionQuery,
    SubmissionStatus, TaskRecord,
};

const SCAN_PAGE_SIZE: usize = 256;

/// Runs in every commit that creates or forks a conversation.
pub type ConversationInit = Box<dyn FnOnce(Tx, ConversationId) -> BoxFuture<'static, DurableResult<()>> + Send>;

/// The Session extension points the Harness fills in.
struct HarnessHooks {
    options: HarnessOptions,
    telemetry: Arc<DurableTelemetry>,
    scheduler: OnceLock<Weak<Scheduler>>,
}

#[async_trait]
impl SessionHooks for HarnessHooks {
    /// Empty `pi.live`, `pi.inbox`, and `pi.usage`, a fresh `pi.provider`, the conversation's `pi.agent`, then
    /// `HarnessOptions::conversation_created`.
    async fn conversation_created(&self, tx: &Tx, record: &ConversationRecord) -> DurableResult<()> {
        tx.doc(&LIVE_DOC, record.id).await?;
        tx.doc(&INBOX_DOC, record.id).await?;
        tx.doc(&USAGE_DOC, record.id).await?;
        tx.doc(&PROVIDER_DOC, record.id).await?;
        create_agent(tx, record).await?;
        if let Some(hook) = &self.options.conversation_created {
            hook(tx.clone(), *record).await?;
        }
        Ok(())
    }

    /// Signal and join task invocations after the close listeners and before Storage closes.
    async fn before_close(&self) {
        let scheduler = self.scheduler.get().and_then(Weak::upgrade);
        if let Some(scheduler) = scheduler {
            scheduler.join().await;
        }
    }

    /// Read at each use, as all settings are, but only these fields: they are read on every commit.
    fn document_retention_ms(&self) -> Option<f64> {
        let source = self.options.settings.as_ref()?;
        source().ok()?.document_retention_ms
    }

    fn persist_within_ms(&self) -> Option<f64> {
        let source = self.options.settings.as_ref()?;
        source().ok()?.progress?.persist_within_ms
    }

    fn report(&self, error: &DurableError, info: &ReportInfo, context: Option<&Context>) {
        if let Some(report) = &self.options.on_report {
            report(error, info);
        }
        self.telemetry.report(info, context);
    }
}

pub(crate) struct HarnessInner {
    pub session: Session,
    pub core: Arc<HarnessCore>,
    pub scheduler: Arc<Scheduler>,
    pub submissions: Arc<Submissions>,
    pub views: Arc<ConversationViews>,
    pub task_graph: Arc<TaskGraphView>,
}

/// Durable agent harness over one Session; cheap to clone. Derefs to the [`Session`].
#[derive(Clone)]
pub struct Harness(pub(crate) Arc<HarnessInner>);

impl Deref for Harness {
    type Target = Session;

    fn deref(&self) -> &Session {
        &self.0.session
    }
}

impl Harness {
    /// Open a Harness over storage. The registry may keep changing while the Harness runs.
    pub async fn open(storage: Arc<dyn Storage>, options: HarnessOptions, context: &Context) -> DurableResult<Harness> {
        if context.is_cancelled() {
            return Err(cancellation(context));
        }
        let snapshot = options.registry.snapshot();
        let missing: Vec<String> = builtin_tasks()
            .into_iter()
            .map(|task| task.name.clone())
            .filter(|name| snapshot.task(name).is_none())
            .collect();
        if !missing.is_empty() {
            let names = missing.join(", ");
            return Err(DurableError::error(format!(
                "Registry lacks built-in tasks {names}; create it with createRegistry()"
            )));
        }
        let telemetry = Arc::new(DurableTelemetry::new(options.telemetry.clone()));
        let hooks = Arc::new(HarnessHooks {
            options: options.clone(),
            telemetry: telemetry.clone(),
            scheduler: OnceLock::new(),
        });
        let session = create_session(
            storage,
            SessionOptions {
                now: options.now.clone(),
                hooks: Some(hooks.clone() as Arc<dyn SessionHooks>),
                document_retention_ms: None,
                read_only: options.read_only,
                persist_within_ms: None,
                set_timer: options.set_timer.clone(),
            },
        );
        let timer_hooks = hooks.clone();
        let timer_report = Arc::new(move |error: &DurableError| {
            let info = ReportInfo::HostError { callback: HostCallback::SetTimer, task: None };
            timer_hooks.report(error, &info, None);
        });
        let now = options.now.clone().unwrap_or_else(|| Arc::new(system_now));
        let core = Arc::new(HarnessCore {
            session: session.clone(),
            models: options.identified_models(),
            registry: options.registry.clone(),
            settings: options.settings.clone(),
            env: options.env.clone(),
            now,
            timers: Timers::new(options.set_timer.clone(), timer_report),
            on_wake: options.on_wake.clone(),
            on_report: options.on_report.clone(),
            telemetry,
            read_only: options.read_only,
        });
        let scheduler = Scheduler::new(core.clone(), without_abort_signal(context));
        let _ = hooks.scheduler.set(Arc::downgrade(&scheduler));
        let opened = async {
            let submissions = Submissions::new(session.clone(), scheduler.clone())?;
            let _ = scheduler.submissions.set(Arc::downgrade(&submissions));
            let views = ConversationViews::new(session.clone())?;
            let task_graph = TaskGraphView::new(session.clone(), &scheduler)?;
            scheduler.open(context).await?;
            Ok::<_, DurableError>(HarnessInner {
                session: session.clone(),
                core,
                scheduler,
                submissions,
                views,
                task_graph,
            })
        };
        match opened.await {
            Ok(inner) => Ok(Harness(Arc::new(inner))),
            Err(error) => {
                // The caller's context may be what failed open: close without it.
                let _ = session.close(&without_abort_signal(context)).await;
                Err(error)
            }
        }
    }

    pub fn session(&self) -> &Session {
        &self.0.session
    }

    /// A disposable read-only Chord state of every live task (spec section 9.4); its value is a [`super::TaskGraph`].
    pub async fn task_graph(&self, context: &Context) -> DurableResult<crate::session::DocumentState> {
        self.0.task_graph.state(context).await
    }

    /// A serialized exact-frame watch of the [`super::TaskGraph`]; cancelling `context` stops it.
    pub async fn watch_task_graph(&self, context: &Context) -> DurableResult<TaskGraphWatch> {
        self.0.task_graph.watch(context).await
    }

    /// Attach a synchronous [`Observer`] to a conversation's [`super::ConversationView`], atomically on the Session
    /// line, for adapters: `advance` gets each next revision with its ops as memory adopts the commit, so it only
    /// records it. `window` as for [`super::Conversation::watch`]; each observer has its own. `context` governs only
    /// the attachment.
    pub async fn observe_conversation(
        &self,
        id: ConversationId,
        observer: Arc<dyn Observer>,
        context: &Context,
        window: Option<super::ConversationWindow>,
    ) -> DurableResult<Observation> {
        self.0.session.assert_usable()?;
        self.0.views.observe(id, observer, context, window).await
    }

    /// Attach a synchronous [`Observer`] to the [`super::TaskGraph`], as `observe_conversation()` does to a view. A
    /// `blocked` reason that changes without a commit advances it on the Session line with the background context.
    pub async fn observe_task_graph(
        &self,
        observer: Arc<dyn Observer>,
        context: &Context,
    ) -> DurableResult<Observation> {
        self.0.session.assert_usable()?;
        self.0.task_graph.observe(observer, context).await
    }

    /// Restart dispatch after a cancellation paused it. Fails after close, and with `ReadOnlySession` on a read-only
    /// Harness.
    pub fn resume(&self) -> DurableResult<()> {
        self.0.session.assert_usable()?;
        self.0.scheduler.resume()
    }

    /// When this Session next needs a running process, from the scheduler's memory: `active` while some task can run
    /// now or is running, `at` for the earliest sleep. Answers also while scheduling is paused. A host that wakes the
    /// process itself arms from it and calls `resume()` when woken; see `HarnessOptions::on_wake`.
    pub fn wake(&self) -> DurableResult<Wake> {
        self.0.session.assert_usable()?;
        Ok(self.0.scheduler.wake())
    }

    /// Internal, for tests: sizes of the scheduler's ownership indexes.
    #[doc(hidden)]
    pub fn scheduler_index_sizes(&self) -> IndexSizes {
        self.0.scheduler.index_sizes()
    }

    /// The root conversation, created on first use.
    pub async fn root(&self, context: &Context, options: CreateOptions) -> DurableResult<Conversation> {
        self.create(CreateTarget::Root, options, context).await
    }

    pub async fn conversation(&self, id: ConversationId, context: &Context) -> DurableResult<Option<Conversation>> {
        self.0.session.assert_usable()?;
        let (storage, read_context) = (self.0.session.storage(), context.clone());
        let record = self.0.session.read_on_line(async move { storage.conversation(id, &read_context).await }).await?;
        Ok(record.map(|record| Conversation::new(record.id, self.clone())))
    }

    pub async fn create_conversation(&self, options: CreateOptions, context: &Context) -> DurableResult<Conversation> {
        self.create(CreateTarget::Independent, options, context).await
    }

    pub async fn get_task(&self, id: TaskId, context: &Context) -> DurableResult<Option<TaskRecord>> {
        let (storage, read_context) = (self.0.session.storage(), context.clone());
        self.0.session.read_on_line(async move { storage.task(id, &read_context).await }).await
    }

    /// Scheduling state, every live task with its derived state, and queued and placed submissions.
    pub async fn inspect(&self, context: &Context) -> DurableResult<HarnessInspection> {
        let (scheduler, storage, context) = (self.0.scheduler.clone(), self.0.session.storage(), context.clone());
        let snapshot = self.0.core.registry.snapshot();
        self.0
            .session
            .read_on_line(async move {
                let (scheduling, tasks) = scheduler.inspect(&snapshot).await?;
                let mut submissions = Vec::new();
                for status in [SubmissionStatus::Queued, SubmissionStatus::Placed] {
                    let query = SubmissionQuery { status: Some(status), ..SubmissionQuery::default() };
                    let (storage, query, context) = (&storage, &query, &context);
                    submissions.extend(
                        scan_all(|cursor| async move {
                            storage.scan_submissions(query, SCAN_PAGE_SIZE, cursor.as_ref(), context).await
                        })
                        .await?,
                    );
                }
                submissions.sort_by_key(|record| record.id);
                Ok(HarnessInspection { scheduling, tasks, submissions })
            })
            .await
    }

    pub async fn submission(&self, id: SubmissionId, context: &Context) -> DurableResult<Option<Submission>> {
        self.0.submissions.get(id, context).await
    }

    /// Withdraw a queued submission; `conversation_id` limits it to one conversation.
    pub async fn abort_submission(
        &self,
        id: SubmissionId,
        context: &Context,
        conversation_id: Option<ConversationId>,
    ) -> DurableResult<AbortSubmissionResult> {
        self.0.submissions.abort(id, context, conversation_id).await
    }

    pub async fn abort_task(&self, id: TaskId, context: &Context) -> DurableResult<AbortTaskResult> {
        self.0.scheduler.abort(id, context, false).await
    }

    pub async fn wait_for_task(&self, id: TaskId, context: &Context) -> DurableResult<TaskRecord> {
        self.0.scheduler.resume()?;
        self.0.scheduler.wait_for_task(id, context).await
    }

    /// Wait until no conversation has live non-background work.
    pub async fn wait_for_idle(&self, context: &Context) -> DurableResult<()> {
        self.0.scheduler.resume()?;
        self.0.scheduler.wait_for_idle(None, context).await
    }

    /// Sum every conversation's committed `pi.usage`. Each document is read at its own point; totals only grow.
    pub async fn usage(&self, context: &Context) -> DurableResult<UsageState> {
        let (storage, read_context) = (self.0.session.storage(), context.clone());
        let conversations = self
            .0
            .session
            .read_on_line(async move {
                let (storage, query, context) = (&storage, &ConversationQuery::default(), &read_context);
                scan_all(|cursor| async move {
                    storage.scan_conversations(query, SCAN_PAGE_SIZE, cursor.as_ref(), context).await
                })
                .await
            })
            .await?;
        let mut total = UsageState::default();
        for record in conversations {
            if let Some(state) = self.0.session.snapshot(&USAGE_DOC, record.id, context).await? {
                add_usage_state(&mut total, &USAGE_DOC.decode(&state)?)?;
            }
        }
        Ok(total)
    }

    pub(crate) async fn create(
        &self,
        target: CreateTarget,
        options: CreateOptions,
        context: &Context,
    ) -> DurableResult<Conversation> {
        self.0.session.assert_usable()?;
        let CreateOptions { ownership, agent, init } = options;
        let change = move |tx: Tx| async move {
            if matches!(target, CreateTarget::Root) && tx.conversation(ROOT_CONVERSATION_ID).await?.is_some() {
                return Ok(ROOT_CONVERSATION_ID);
            }
            let record = match target {
                CreateTarget::Root => tx.create_root_conversation().await?,
                CreateTarget::Fork { parent, at } => tx.fork_conversation(parent, at, ownership).await?,
                CreateTarget::Independent => tx.create_conversation(ownership).await?,
            };
            if let Some(agent) = &agent {
                configure(&tx, record.id, agent).await?;
            }
            if let Some(init) = init {
                init(tx.clone(), record.id).await?;
            }
            Ok(record.id)
        };
        let id = self.0.session.commit_with(change, context, TransactionScope::default()).await?;
        Ok(Conversation::new(id, self.clone()))
    }
}

impl Default for CreateOptions {
    fn default() -> Self {
        Self { ownership: ConversationOwnership::Ownerless, agent: None, init: None }
    }
}

fn system_now() -> f64 {
    let elapsed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    elapsed.as_millis() as f64
}
