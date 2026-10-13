//! Conversation views (spec section 9.3): one mount per conversation with its active transcript and built-in
//! documents, kept in step with commit publications.

use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use pi_chord::delta::{Op, Path, Seg, apply_immutable};
use pi_chord::state::{ReplicatedStateSourceOptions, attach_replicated_state_source};
use pi_chord::{Context, Json, JsonMap, without_abort_signal};

use super::agent::AGENT_DOC;
use super::context::{active_entries, capture_context_bounds};
use super::inbox::INBOX_DOC;
use super::live::LIVE_DOC;
use super::usage::{PROVIDER_DOC, USAGE_DOC};
use super::util::{cancellation, closed_error, lock};
use crate::documents::encode;
use crate::errors::{DurableError, DurableResult};
use crate::ids::{ConversationId, DocumentId};
use crate::session::{CommittedObserver, CommittedStateSource, CommittedWatch, DocumentState, Release, Session};
use crate::types::{CommitChange, CommitPublication, DocumentCommitChange};

/// The view `{ conversation, entries, docs }` as JSON: the conversation record, the raw active entries (as
/// `ContextView::entries`), and the built-in conversation documents keyed by kind.
pub type ConversationView = Json;

/// Receives each next revision of a mount, and the Session's close.
pub(crate) trait ViewObserver: Send + Sync {
    fn advance(&self, _value: &Json, _ops: &[Op], _context: &Context) {}
    /// Every publication, after the mount took it; `ops` are the mount's, possibly none.
    fn publication(
        &self,
        _before: &Json,
        _after: &Json,
        _ops: &[Op],
        _publication: &CommitPublication,
        _context: &Context,
    ) {
    }
    fn close_session(&self, failure: Option<DurableError>);
}

struct Committed(Arc<dyn CommittedObserver>);

impl ViewObserver for Committed {
    fn advance(&self, value: &Json, ops: &[Op], context: &Context) {
        self.0.advance(value.clone(), ops.to_vec(), context.clone());
    }

    fn close_session(&self, failure: Option<DurableError>) {
        self.0.close_session(failure);
    }
}

const MOUNTED_KINDS: [&str; 5] = ["pi.agent", "pi.live", "pi.inbox", "pi.provider", "pi.usage"];

struct Mount {
    value: Json,
    /// Mounted incarnation and definition version per kind; another incarnation or version is set whole.
    docs: HashMap<String, (DocumentId, u32)>,
    observers: Vec<(u64, Arc<dyn ViewObserver>)>,
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
        session.observe_commits(move |publication, context| {
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
            let failure = views.session.failure();
            for (_, observer) in mounts.into_iter().flat_map(|mount| mount.observers) {
                observer.close_session(failure.clone());
            }
        })?;
        Ok(views)
    }

    /// A disposable read-only Chord state of the view.
    pub async fn state(self: &Arc<Self>, id: ConversationId, context: &Context) -> DurableResult<DocumentState> {
        let (source, release) = self
            .attach(id, context, |value, release| {
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

    /// A serialized exact-frame watch of the view; cancelling `context` stops it.
    pub async fn watch(self: &Arc<Self>, id: ConversationId, context: &Context) -> DurableResult<CommittedWatch> {
        let session = self.session.clone();
        let (watch, _) = self
            .attach(id, context, move |value, release| {
                let watch = CommittedWatch::new(value.clone(), release, move |error| session.report(error), None);
                (Arc::new(Committed(Arc::new(watch.clone()))) as Arc<dyn ViewObserver>, watch)
            })
            .await?;
        if context.is_cancelled() {
            watch.cancel();
            return Err(cancellation(context));
        }
        if let Some(signal) = context.abort_signal() {
            watch.observe_cancellation(&signal)?;
        }
        Ok(watch)
    }

    /// Register an observer made from the current revision, atomically on the Session line: it sees every later
    /// publication and nothing earlier. Running the release drops it, and the mount with its last observer.
    pub fn attach<T, F>(
        self: &Arc<Self>,
        id: ConversationId,
        context: &Context,
        create: F,
    ) -> impl Future<Output = DurableResult<(T, Arc<Release>)>> + Send + use<T, F>
    where
        T: Send + 'static,
        F: FnOnce(&Json, Arc<Release>) -> (Arc<dyn ViewObserver>, T) + Send + 'static,
    {
        let views = self.clone();
        let context = context.clone();
        self.session.read_on_line(async move {
            let mounted = lock(&views.mounts).get(&id).map(|mount| mount.value.clone());
            let (value, built) = match mounted {
                Some(value) => (value, None),
                None => {
                    let mount = views.build(id, &context).await?;
                    (mount.value.clone(), Some(mount))
                }
            };
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
                mount.observers.push((key, observer));
            }
            Ok((result, release))
        })
    }

    async fn build(&self, id: ConversationId, context: &Context) -> DurableResult<Mount> {
        let storage = self.session.storage();
        let Some(conversation) = storage.conversation(id, context).await? else {
            return Err(DurableError::error(format!("Conversation {id} does not exist")));
        };
        let bounds = capture_context_bounds(&*storage, id, context, None).await?;
        let entries = active_entries(&*storage, id, bounds.as_ref(), context).await?;
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
        let mut value = JsonMap::new();
        value.insert("conversation", encode(&conversation)?);
        value.insert("entries", encode(&entries)?);
        value.insert("docs", Json::object(docs));
        Ok(Mount { value: Json::object(value), docs: incarnations, observers: Vec::new() })
    }

    fn advance_all(&self, publication: &CommitPublication, context: &Context) {
        let mut deliveries = Vec::new();
        {
            let mut mounts = lock(&self.mounts);
            for (id, mount) in mounts.iter_mut() {
                match advance(*id, mount, publication) {
                    Ok(delivery) => deliveries.push(delivery),
                    Err(error) => self.session.report(&error),
                }
            }
        }
        let frame_context = without_abort_signal(context);
        for (before, after, ops, observers) in deliveries {
            if !ops.is_empty() {
                for observer in &observers {
                    observer.advance(&after, &ops, &frame_context);
                }
            }
            for observer in &observers {
                observer.publication(&before, &after, &ops, publication, &frame_context);
            }
        }
    }
}

type Delivery = (Json, Json, Vec<Op>, Vec<Arc<dyn ViewObserver>>);

fn key_path(keys: &[&str]) -> Path {
    keys.iter().map(|key| Seg::Key(Arc::from(*key))).collect()
}

/// Derive the mount's operations from one publication and apply them. Entry writes are published in ID order.
fn advance(id: ConversationId, mount: &mut Mount, publication: &CommitPublication) -> DurableResult<Delivery> {
    let mut doc_ops = Vec::new();
    let mut entry_ops = Vec::new();
    let mut entries: Vec<Json> = mount.value.get("entries").and_then(Json::as_array).cloned().unwrap_or_default();
    let mut entries_changed = false;
    for change in &publication.changes {
        match change {
            CommitChange::Entry(entry) if entry.conversation_id == id => {
                let value = encode(entry)?;
                entries_changed = true;
                let Some(target) = entry.head else {
                    entry_ops.push(Op::Splice(key_path(&["entries"]), entries.len() as u64, 0, vec![value.clone()]));
                    entries.push(value);
                    continue;
                };
                // A head marker keeps the non-head entries from its head, which are always a suffix, and goes first.
                let kept = entries
                    .iter()
                    .position(|candidate| {
                        candidate.get("head").is_none()
                            && candidate.get("id").and_then(Json::as_u64).is_some_and(|other| other >= target.0)
                    })
                    .unwrap_or(entries.len());
                entry_ops.push(Op::Splice(key_path(&["entries"]), 0, kept as u64, vec![value.clone()]));
                entries.splice(0..kept, [value]);
            }
            CommitChange::Document(DocumentCommitChange::Document { record, conversation_id, version, value, ops })
                if *conversation_id == Some(id) =>
            {
                let kind = record.kind.as_str();
                if !MOUNTED_KINDS.contains(&kind) || record.key.is_some() {
                    continue;
                }
                let path = key_path(&["docs", kind]);
                let mounted = mount.docs.get(kind).copied();
                match value {
                    None => {
                        if mounted.map(|(doc, _)| doc) != Some(record.id) {
                            continue;
                        }
                        mount.docs.remove(kind);
                        doc_ops.push(Op::Delete(path));
                    }
                    Some(value) => {
                        if mounted.is_some_and(|(doc, at)| doc == record.id && Some(at) == *version) {
                            doc_ops.extend(ops.iter().map(|op| prefixed(op, &path)));
                        } else {
                            mount.docs.insert(kind.to_owned(), (record.id, version.unwrap_or(1)));
                            doc_ops.push(Op::Set(path, value.clone()));
                        }
                    }
                }
            }
            _ => {}
        }
    }
    let before = mount.value.clone();
    let mut ops = doc_ops.clone();
    ops.extend(entry_ops);
    if !ops.is_empty() {
        let mut value =
            if doc_ops.is_empty() { mount.value.clone() } else { apply_immutable(Some(&mount.value), &doc_ops)? };
        if entries_changed && let Some(map) = value.as_object_mut() {
            map.insert("entries", Json::array(entries));
        }
        mount.value = value;
    }
    let observers = mount.observers.iter().map(|(_, observer)| observer.clone()).collect();
    Ok((before, mount.value.clone(), ops, observers))
}

/// `op` moved under `prefix`; a root replacement becomes a set of the prefix.
fn prefixed(op: &Op, prefix: &Path) -> Op {
    let at = |path: &Path| -> Path { prefix.iter().chain(path.iter()).cloned().collect() };
    match op {
        Op::Replace(value) => Op::Set(prefix.clone(), value.clone()),
        Op::Set(path, value) => Op::Set(at(path), value.clone()),
        Op::Delete(path) => Op::Delete(at(path)),
        Op::Append(path, text) => Op::Append(at(path), text.clone()),
        Op::Truncate(path, count) => Op::Truncate(at(path), *count),
        Op::Splice(path, index, remove, items) => Op::Splice(at(path), *index, *remove, items.clone()),
        Op::Move(path, permutation) => Op::Move(at(path), permutation.clone()),
    }
}
