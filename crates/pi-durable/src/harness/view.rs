//! Conversation views (spec section 9.3): one mount per conversation with its built-in documents, and one window over
//! the transcript per observer, kept in step with commit publications.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use pi_chord::state::{ReplicatedStateSourceOptions, attach_replicated_state_source};
use pi_chord::{Context, Json, JsonMap, Watch, without_abort_signal};

use super::agent::AGENT_DOC;
use super::inbox::INBOX_DOC;
use super::live::LIVE_DOC;
use super::usage::{PROVIDER_DOC, USAGE_DOC};
use super::util::{cancellation, closed_error, lock};
pub use super::view_window::ConversationWindow;
use super::view_window::{Mount, Windowed, advance, load_window, resolve_window, view};
use crate::documents::encode;
use crate::errors::{DurableError, DurableResult};
use crate::ids::ConversationId;
use crate::session::{
    CommittedObserver, CommittedStateSource, DocumentState, GuardedObserver, Observation, ObservedCommit, Observer,
    Release, Session, WatchCall, WatchEnd, session_end,
};

/// The view `{ conversation, entries, before?, docs }` as JSON: a window over the fork-aware transcript in entry ID
/// order, the entry just before it when there is one, and the built-in documents keyed by kind.
pub type ConversationView = Json;

/// The structural view as a serialized exact-frame watch with bounded pending frames.
pub type ConversationWatch = Watch<DurableError>;

/// Receives each next revision of one observer's window, and the Session's close.
pub(crate) trait ViewObserver: Send + Sync {
    fn advance(&self, call: &WatchCall, context: &Context);
    /// The Session closed (`Ended`), or failed (`Failed`).
    fn end(&self, end: WatchEnd);
}

struct Committed(Arc<dyn CommittedObserver>);

impl ViewObserver for Committed {
    fn advance(&self, call: &WatchCall, context: &Context) {
        self.0.advance(call.clone(), context.clone());
    }

    fn end(&self, end: WatchEnd) {
        self.0.end(end);
    }
}

/// The Harness's conversation view mounts: at most one per conversation, built on the Session line by its first
/// observer and dropped with its last.
pub(crate) struct ConversationViews {
    session: Session,
    mounts: Mutex<HashMap<ConversationId, Mount>>,
    closed: AtomicBool,
    next: AtomicU64,
}

impl ConversationViews {
    pub fn new(session: Session) -> DurableResult<Arc<Self>> {
        let views = Arc::new(Self {
            session: session.clone(),
            mounts: Mutex::default(),
            closed: AtomicBool::new(false),
            next: AtomicU64::new(0),
        });
        let weak = Arc::downgrade(&views);
        session.follow_commits(move |publication, context| {
            if let Some(views) = weak.upgrade() {
                views.advance_all(publication, context);
            }
            Ok(())
        })?;
        let weak = Arc::downgrade(&views);
        session.subscribe_close(move || {
            let Some(views) = weak.upgrade() else { return };
            views.closed.store(true, Ordering::SeqCst);
            let mounts: Vec<Mount> = lock(&views.mounts).drain().map(|(_, mount)| mount).collect();
            let end = session_end(views.session.failure());
            for (_, windowed) in mounts.into_iter().flat_map(|mount| mount.observers) {
                windowed.observer.end(end.clone());
            }
        })?;
        Ok(views)
    }

    /// A disposable read-only Chord state of the view, with the default window.
    pub async fn state(self: &Arc<Self>, id: ConversationId, context: &Context) -> DurableResult<DocumentState> {
        let (source, release) = self
            .attach(id, None, context, |value, release| {
                let source = CommittedStateSource::new(value.clone(), release);
                (Arc::new(Committed(source.clone())) as Arc<dyn ViewObserver>, source)
            })
            .await?;
        let session = self.session.clone();
        let on_error = Arc::new(move |error: pi_chord::ChordError| session.report(&error.into()));
        match attach_replicated_state_source(source.as_ref(), ReplicatedStateSourceOptions { on_error: Some(on_error) })
        {
            Ok(state) => Ok(state),
            Err(error) => {
                release.run();
                Err(error.into())
            }
        }
    }

    /// Attach a synchronous observer of the view (`Harness::observe_conversation()`), behind its guard; `context`
    /// governs only the attachment.
    pub async fn observe(
        self: &Arc<Self>,
        id: ConversationId,
        observer: Arc<dyn Observer>,
        context: &Context,
        window: Option<ConversationWindow>,
    ) -> DurableResult<Observation> {
        let session = self.session.clone();
        let ((value, guarded), _) = self
            .attach(id, window, context, move |value, release| {
                let guarded = GuardedObserver::new(observer, move || release.run(), move |error| session.report(error));
                (Arc::new(Committed(guarded.clone())) as Arc<dyn ViewObserver>, (value.clone(), guarded))
            })
            .await?;
        if context.is_cancelled() {
            guarded.detach();
            return Err(cancellation(context));
        }
        Ok(Observation::new(value, Arc::new(move || guarded.detach())))
    }

    /// A serialized exact-frame watch of the view, an observer of its mount; aborting `context` ends it `Aborted`.
    pub async fn watch(
        self: &Arc<Self>,
        id: ConversationId,
        context: &Context,
        window: Option<ConversationWindow>,
    ) -> DurableResult<ConversationWatch> {
        let session = self.session.clone();
        let (watch, _) = self
            .attach(id, window, context, move |value, release| {
                let watch = Watch::new(value.clone(), move || release.run(), move |error| session.report(error));
                (Arc::new(Committed(Arc::new(watch.clone()))) as Arc<dyn ViewObserver>, watch)
            })
            .await?;
        if context.is_cancelled() {
            watch.abort();
            return Err(cancellation(context));
        }
        if let Some(signal) = context.abort_signal() {
            watch.observe_abort(&signal)?;
        }
        Ok(watch)
    }

    /// Register an observer made from its window's first revision, atomically on the Session line: it sees every later
    /// publication and nothing earlier. Running the release drops it, and the mount with its last observer.
    pub async fn attach<T, F>(
        self: &Arc<Self>,
        id: ConversationId,
        window: Option<ConversationWindow>,
        context: &Context,
        create: F,
    ) -> DurableResult<(T, Arc<Release>)>
    where
        T: Send + 'static,
        F: FnOnce(&Json, Arc<Release>) -> (Arc<dyn ViewObserver>, T) + Send + 'static,
    {
        let bounds = resolve_window(window)?;
        let views = self.clone();
        let context = context.clone();
        let attach = async move {
            let mounted = lock(&views.mounts).get(&id).map(|mount| (mount.conversation.clone(), mount.docs.clone()));
            let (conversation, docs, built) = match mounted {
                Some((conversation, docs)) => (conversation, docs, None),
                None => {
                    let mount = views.build(id, &context).await?;
                    (mount.conversation.clone(), mount.docs.clone(), Some(mount))
                }
            };
            let (entries, before) = load_window(&*views.session.storage(), id, bounds, &context).await?;
            let value = view(&conversation, Json::array(entries), before, &docs);
            let key = views.next.fetch_add(1, Ordering::SeqCst);
            let release = Release::new();
            let weak = Arc::downgrade(&views);
            release.push(move || {
                let Some(views) = weak.upgrade() else { return };
                let mut mounts = lock(&views.mounts);
                if let Some(mount) = mounts.get_mut(&id) {
                    mount.observers.retain(|(other, _)| *other != key);
                    if mount.observers.is_empty() {
                        mounts.remove(&id);
                    }
                }
            });
            let (observer, result) = create(&value, release.clone());
            // Close or cancellation may begin while the mount hydrates; register nothing then.
            if views.closed.load(Ordering::SeqCst) {
                return Err(closed_error(&views.session));
            }
            if context.is_cancelled() {
                return Err(cancellation(&context));
            }
            let mut mounts = lock(&views.mounts);
            if let Some(mount) = built {
                mounts.insert(id, mount);
            }
            if let Some(mount) = mounts.get_mut(&id) {
                mount.observers.push((key, Windowed { min: bounds.min, max: bounds.max, value, observer }));
            }
            Ok((result, release))
        };
        self.session.read_on_line(attach).await
    }

    async fn build(&self, id: ConversationId, context: &Context) -> DurableResult<Mount> {
        let storage = self.session.storage();
        let Some(conversation) = storage.conversation(id, context).await? else {
            return Err(DurableError::error(format!("Conversation {id} does not exist")));
        };
        let mut docs = JsonMap::new();
        let mut incarnations = HashMap::new();
        let loaded = [
            self.session.conversation_document_on_line(&AGENT_DOC, id, context).await?,
            self.session.conversation_document_on_line(&LIVE_DOC, id, context).await?,
            self.session.conversation_document_on_line(&INBOX_DOC, id, context).await?,
            self.session.conversation_document_on_line(&PROVIDER_DOC, id, context).await?,
            self.session.conversation_document_on_line(&USAGE_DOC, id, context).await?,
        ];
        for loaded in loaded.into_iter().flatten() {
            docs.insert(loaded.record.kind.as_str(), loaded.value.clone());
            incarnations.insert(loaded.record.kind.clone(), (loaded.record.id, loaded.version));
        }
        let conversation = encode(&conversation)?;
        Ok(Mount { conversation, docs: Json::object(docs), incarnations, observers: Vec::new() })
    }

    fn advance_all(&self, publication: &ObservedCommit, context: &Context) {
        let mut deliveries = Vec::new();
        {
            let mut mounts = lock(&self.mounts);
            for (id, mount) in mounts.iter_mut() {
                if let Err(error) = advance(*id, mount, publication, &mut deliveries) {
                    self.session.report(&error);
                }
            }
        }
        let frame_context = without_abort_signal(context);
        for (observer, call) in deliveries {
            observer.advance(&call, &frame_context);
        }
    }
}
