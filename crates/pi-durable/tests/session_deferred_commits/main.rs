//! Port of upstream `test/session-deferred-commits.test.ts`: what a deferred commit holds in memory, what stores it, and
//! what Storage and each kind of listener then sees.

#[path = "../support/mod.rs"]
mod support;

mod holding;
mod publications;
mod storing;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

use pi_chord::Json;
use pi_durable::documents::*;
use pi_durable::session::{ObservedCommit, Session, SessionOptions, create_session};
use pi_durable::timers::{SetTimer, TimerCallback};
use pi_durable::*;
use support::*;

fn text_initial() -> Json {
    json(r#"{"text":""}"#)
}

/// A latest conversation document whose forks start empty, the kind a deferred commit may hold.
pub static TEXT_DOC: LazyLock<DocToken<Json, LatestConversation>> = LazyLock::new(|| {
    let checkpoint = |value: &Json, _: &[pi_chord::delta::Op], _| {
        if value.get("explode") == Some(&Json::Bool(true)) {
            return Err(DurableError::error("checkpoint failed"));
        }
        Ok(value.get("base") == Some(&Json::Bool(true)))
    };
    let scope = LatestConversation(LatestFork::Initial);
    define_doc(DocSpec::new("test.deferred.text", 1, scope, text_initial, checkpoint)).unwrap()
});

/// `TEXT_DOC` at version 2, which a reload migrates to.
pub static TEXT_DOC_V2: LazyLock<DocToken<Json, LatestConversation>> = LazyLock::new(|| {
    let initial = || json(r#"{"text":"","migrated":true}"#);
    let scope = LatestConversation(LatestFork::Initial);
    let spec = DocSpec::new("test.deferred.text", 2, scope, initial, |_, _, _| Ok(false)).migrate(|value, _| {
        let mut value = value.clone();
        value.set("migrated", true)?;
        Ok(value)
    });
    define_doc(spec).unwrap()
});

pub static REWINDABLE_DOC: LazyLock<DocToken<Json, RewindableConversation>> = LazyLock::new(|| {
    let scope = RewindableConversation(DocumentFork::AsOf);
    define_doc(DocSpec::new("test.deferred.rewindable", 1, scope, text_initial, |_, _, _| Ok(false))).unwrap()
});

pub static CURRENT_FORK_DOC: LazyLock<DocToken<Json, LatestConversation>> = LazyLock::new(|| {
    let scope = LatestConversation(LatestFork::Current);
    define_doc(DocSpec::new("test.deferred.current-fork", 1, scope, text_initial, |_, _, _| Ok(false))).unwrap()
});

pub static SESSION_TEXT_DOC: LazyLock<DocToken<Json, SessionScope>> = LazyLock::new(|| {
    define_doc(DocSpec::new("test.deferred.session", 1, SessionScope, text_initial, |_, _, _| Ok(false))).unwrap()
});

pub const IMMEDIATE: CommitOptions = CommitOptions { persistence: CommitPersistence::Immediate };

/// One foreground timer the Session armed.
struct Armed {
    callback: Option<TimerCallback>,
    delay_ms: f64,
    disarmed: Arc<AtomicBool>,
}

/// A Session over controlled storage with a manual clock and manual timers, recording what `observe_commits()`
/// and the Session's own observers get.
pub struct Opened {
    pub clock: Arc<Mutex<f64>>,
    timers: Arc<Mutex<Vec<Armed>>>,
    pub storage: Arc<ControlledStorage>,
    pub session: Session,
    pub stored: Arc<Mutex<Vec<CommitPublication>>>,
    pub observed: Arc<Mutex<Vec<ObservedCommit>>>,
    pub conversation_id: ConversationId,
    pub record_id: DocumentId,
}

pub async fn open(retention_ms: Option<f64>, persist_within_ms: Option<f64>) -> Opened {
    let clock = Arc::new(Mutex::new(1_000.0));
    let timers: Arc<Mutex<Vec<Armed>>> = Arc::default();
    let armed = timers.clone();
    let set_timer: SetTimer = Arc::new(move |callback, delay_ms, options| {
        if options.background {
            return None;
        }
        let disarmed = Arc::new(AtomicBool::new(false));
        armed.lock().unwrap().push(Armed { callback: Some(callback), delay_ms, disarmed: disarmed.clone() });
        Some(Box::new(move || disarmed.store(true, Ordering::SeqCst)))
    });
    let storage = Arc::new(ControlledStorage::default());
    let reader = clock.clone();
    let options = SessionOptions {
        now: Some(Arc::new(move || *reader.lock().unwrap())),
        document_retention_ms: Some(retention_ms.unwrap_or(600_000.0)),
        persist_within_ms,
        set_timer: Some(set_timer),
        ..SessionOptions::default()
    };
    let session = create_session(storage.clone(), options);
    let stored: Arc<Mutex<Vec<CommitPublication>>> = Arc::default();
    let sink = stored.clone();
    std::mem::forget(session.observe_commits(commit_observer(move |publication, _| {
        sink.lock().unwrap().push(publication.clone());
        Ok(())
    })));
    let observed: Arc<Mutex<Vec<ObservedCommit>>> = Arc::default();
    let sink = observed.clone();
    std::mem::forget(session.follow_commits(move |publication, _| {
        sink.lock().unwrap().push(publication.clone());
        Ok(())
    }));
    let conversation_id = create_conversation(&session).await;
    let create = move |tx: session::Tx| async move { tx.doc(&TEXT_DOC, conversation_id).await.map(drop) };
    session.commit(create, &context()).await.unwrap();
    let created = stored.lock().unwrap().last().cloned().unwrap();
    let record_id = change_record(&document_changes(&created)[0]).id;
    Opened { clock, timers, storage, session, stored, observed, conversation_id, record_id }
}

impl Opened {
    /// Delays of the timers armed and neither disarmed nor fired.
    pub fn armed(&self) -> Vec<f64> {
        let timers = self.timers.lock().unwrap();
        let live = timers.iter().filter(|timer| timer.callback.is_some() && !timer.disarmed.load(Ordering::SeqCst));
        live.map(|timer| timer.delay_ms).collect()
    }

    /// Fire every armed timer and let the line run what they queued.
    pub async fn fire(&self) {
        let callbacks: Vec<TimerCallback> = {
            let mut timers = self.timers.lock().unwrap();
            let live = timers.iter_mut().filter(|timer| !timer.disarmed.load(Ordering::SeqCst));
            live.filter_map(|timer| timer.callback.take()).collect()
        };
        for callback in callbacks {
            callback();
        }
        self.session.read_on_line(async { Ok(()) }).await.unwrap();
    }

    /// Batches admitted to Storage so far; `batches_since()` gives the ones after.
    pub fn mark(&self) -> usize {
        self.storage.commit_count()
    }

    pub fn batches_since(&self, mark: usize) -> Vec<Vec<StorageWrite>> {
        self.storage.commits()[mark..].to_vec()
    }

    pub async fn stored_text(&self) -> Option<Json> {
        stored_value(&self.storage, self.record_id).await
    }

    pub fn held(&self) -> usize {
        self.session.document_cache_sizes().held
    }

    pub async fn append(&self, text: &str, options: CommitOptions) -> DurableResult<()> {
        append(&self.session, self.conversation_id, text, options).await
    }

    pub async fn snapshot(&self) -> Option<Json> {
        self.session.snapshot(&TEXT_DOC, self.conversation_id, &context()).await.unwrap()
    }
}

pub async fn stored_value(storage: &ControlledStorage, id: DocumentId) -> Option<Json> {
    storage.document(id, DocumentPoint::Current, &context()).await.unwrap().map(|stored| stored.value)
}

pub async fn append(
    session: &Session,
    conversation_id: ConversationId,
    text: &str,
    options: CommitOptions,
) -> DurableResult<()> {
    let text = text.to_owned();
    let change = move |tx: session::Tx| async move {
        let draft = tx.doc(&TEXT_DOC, conversation_id).await?;
        draft.update(|state| {
            let next = format!("{}{text}", state["text"].as_str().unwrap_or_default());
            state.set("text", next)
        })??;
        Ok(())
    };
    session.commit_with_options(change, &context(), options).await
}

/// The document content writes of `batches`.
pub fn changes_of(batches: &[Vec<StorageWrite>]) -> Vec<(DocumentId, DocumentContent)> {
    let writes = batches.iter().flatten();
    let changes = writes.filter_map(|write| match write {
        StorageWrite::DocumentChange { id, content } => Some((*id, content.clone())),
        _ => None,
    });
    changes.collect()
}

pub fn content_json(content: &DocumentContent) -> serde_json::Value {
    serde_json::to_value(content).unwrap()
}
