//! Port of upstream `test/session-document-retention.test.ts`: the Session's document cache is bounded by use.
//! No counterpart for a clock or setting that throws, or for timers that cannot be unreferenced: Rust has neither.

#[path = "../support/mod.rs"]
mod support;

mod edges;
mod harness;

use std::sync::{Arc, LazyLock, Mutex};

use pi_chord::Json;
use pi_durable::documents::*;
use pi_durable::session::{self, DocumentCacheSizes, Session, SessionOptions, create_session};
use pi_durable::*;
use serde::{Deserialize, Serialize};
use serde_json::json;
use support::task_support::eventually;
use support::{ControlledStorage, Deferred, context, create_conversation};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Counter {
    pub count: f64,
}

fn counter_spec<S: DocScope>(kind: &str, scope: S) -> DocSpec<Counter, S> {
    DocSpec::new(kind, 1, scope, || Counter { count: 0.0 }, |_, _, _| Ok(false))
}

pub static COUNTER: LazyLock<DocToken<Counter, LatestConversation>> = LazyLock::new(|| {
    define_doc(counter_spec("test.retention.counter", LatestConversation(LatestFork::Initial))).unwrap()
});
pub static REWINDABLE: LazyLock<DocToken<Counter, RewindableConversation>> = LazyLock::new(|| {
    define_doc(counter_spec("test.retention.rewindable", RewindableConversation(DocumentFork::AsOf))).unwrap()
});
pub static SESSION_COUNTER: LazyLock<DocToken<Counter, SessionScope>> =
    LazyLock::new(|| define_doc(counter_spec("test.retention.session-counter", SessionScope)).unwrap());

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub seq: u64,
}

/// A third-party conversation family: one member per event.
pub static EVENTS: LazyLock<DocFamilyToken<Event, u64, LatestConversation>> = LazyLock::new(|| {
    let spec = DocFamilySpec::new(
        "test.retention.event",
        1,
        LatestConversation(LatestFork::Initial),
        |seq| Event { seq },
        |_, _, _| Ok(false),
    );
    define_doc_family(spec).unwrap()
});

pub type Clock = Arc<Mutex<f64>>;

pub struct Opened {
    pub storage: Arc<ControlledStorage>,
    pub session: Session,
    pub clock: Clock,
}

impl Opened {
    pub fn sizes(&self) -> DocumentCacheSizes {
        self.session.document_cache_sizes()
    }
}

/// A Session with a controllable clock and retention.
pub fn open(retention_ms: f64) -> Opened {
    let clock: Clock = Arc::new(Mutex::new(1_000.0));
    let storage = Arc::new(ControlledStorage::default());
    let reader = clock.clone();
    let options = SessionOptions {
        now: Some(Arc::new(move || *reader.lock().unwrap())),
        document_retention_ms: Some(retention_ms),
        hooks: None,
        ..SessionOptions::default()
    };
    Opened { session: create_session(storage.clone(), options), storage, clock }
}

pub fn advance(clock: &Clock, ms: f64) {
    *clock.lock().unwrap() += ms;
}

/// Let the line run what is queued on it: an eviction job, then return.
pub async fn settle_line(session: &Session) {
    session.read_on_line(async { Ok(()) }).await.unwrap();
}

/// Wait until the cache holds no document.
pub async fn emptied(test: &Opened) {
    eventually(|| async {
        settle_line(&test.session).await;
        test.sizes().cached == 0
    })
    .await;
}

pub async fn bump(session: &Session, conversation_id: ConversationId, by: f64) {
    let edit = move |tx: session::Tx| async move {
        tx.doc(&COUNTER, conversation_id).await?.edit(|counter| counter.count += by)
    };
    session.commit(edit, &context()).await.unwrap();
}

pub async fn count_of(session: &Session, conversation_id: ConversationId) -> Json {
    session.snapshot(&COUNTER, conversation_id, &context()).await.unwrap().unwrap()
}

pub fn counter_json(count: f64) -> Json {
    Json::from(json!({ "count": count }))
}

pub async fn touch_session_document(session: &Session) {
    let touch = |tx: session::Tx| async move { tx.doc(&SESSION_COUNTER, ()).await.map(drop) };
    session.commit(touch, &context()).await.unwrap();
}

#[tokio::test]
async fn evicts_an_unattached_document_once_its_retention_passed_and_loads_it_again_with_the_same_value() {
    let test = open(60_000.0);
    let conversation_id = create_conversation(&test.session).await;
    bump(&test.session, conversation_id, 3.0).await;
    assert!(test.sizes().cached > 0);
    // Not yet: inside the retention period.
    advance(&test.clock, 59_000.0);
    bump(&test.session, conversation_id, 0.0).await;
    settle_line(&test.session).await;
    assert!(test.sizes().cached > 0);
    // Past it, with no use since: the next commit evicts it.
    advance(&test.clock, 61_000.0);
    test.session.commit(|_| async { Ok(()) }, &context()).await.unwrap();
    touch_session_document(&test.session).await;
    settle_line(&test.session).await;
    let reads = test.storage.document_reads();
    assert_eq!(count_of(&test.session, conversation_id).await, counter_json(3.0));
    assert!(test.storage.document_reads() > reads);
    test.session.close(&context()).await.unwrap();
}

#[tokio::test]
async fn evicts_with_retention_0_right_after_use_on_the_line() {
    let test = open(0.0);
    let conversation_id = create_conversation(&test.session).await;
    bump(&test.session, conversation_id, 1.0).await;
    emptied(&test).await;
    assert_eq!(count_of(&test.session, conversation_id).await, counter_json(1.0));
    test.session.close(&context()).await.unwrap();
}

#[tokio::test]
async fn never_evicts_a_document_a_watch_or_a_document_state_observes_and_evicts_it_once_the_last_detaches() {
    let test = open(1_000.0);
    let conversation_id = create_conversation(&test.session).await;
    bump(&test.session, conversation_id, 1.0).await;
    let ctx = context();
    let watch = test.session.watch_doc(&COUNTER, conversation_id, &ctx).await.unwrap().unwrap();
    let state = test.session.document_state(&COUNTER, conversation_id, &ctx).await.unwrap().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = seen.clone();
    watch
        .start(move |value, _, _| {
            sink.lock().unwrap().push(value["count"].as_f64().unwrap_or(-1.0));
            async { Ok(()) }
        })
        .unwrap();
    advance(&test.clock, 1_000_000.0);
    bump(&test.session, conversation_id, 1.0).await;
    settle_line(&test.session).await;
    // Still cached: a read long after its retention passed loads nothing from Storage.
    assert_eq!(state.value()["count"], Json::from(2.0));
    eventually(|| async { seen.lock().unwrap().contains(&2.0) }).await;
    let reads = test.storage.document_reads();
    advance(&test.clock, 1_000_000.0);
    touch_session_document(&test.session).await;
    settle_line(&test.session).await;
    assert_eq!(count_of(&test.session, conversation_id).await, counter_json(2.0));
    assert_eq!(test.storage.document_reads(), reads);
    watch.stop().await;
    advance(&test.clock, 1_000_000.0);
    bump(&test.session, conversation_id, 1.0).await;
    settle_line(&test.session).await;
    assert_eq!(state.value()["count"], Json::from(3.0));
    state.dispose().unwrap();
    advance(&test.clock, 1_000_000.0);
    test.session.commit(|_| async { Ok(()) }, &context()).await.unwrap();
    touch_session_document(&test.session).await;
    settle_line(&test.session).await;
    // The session document was just used; the counter is gone.
    assert_eq!(test.sizes().cached, 1);
    test.session.close(&context()).await.unwrap();
}

#[tokio::test]
async fn never_takes_a_tracker_from_a_transaction_that_holds_it() {
    let test = open(0.0);
    let conversation_id = create_conversation(&test.session).await;
    bump(&test.session, conversation_id, 1.0).await;
    let (inside, release) = (Deferred::default(), Deferred::default());
    let (entered, gate) = (inside.clone(), release.clone());
    let hold = move |tx: session::Tx| async move {
        let draft = tx.doc(&COUNTER, conversation_id).await?;
        entered.resolve();
        gate.wait().await;
        draft.edit(|counter| counter.count += 10.0)
    };
    let commit = tokio::spawn(test.session.commit(hold, &context()));
    inside.wait().await;
    // Retention passes while the transaction holds the document; eviction waits on the line behind it.
    advance(&test.clock, 1_000_000.0);
    release.resolve();
    commit.await.unwrap().unwrap();
    assert_eq!(count_of(&test.session, conversation_id).await, counter_json(11.0));
    bump(&test.session, conversation_id, 1.0).await;
    assert_eq!(count_of(&test.session, conversation_id).await, counter_json(12.0));
    test.session.close(&context()).await.unwrap();
}

#[tokio::test]
async fn continues_the_stored_deltas_and_bases_for_writes_after_a_reload() {
    let test = open(0.0);
    let conversation_id = create_conversation(&test.session).await;
    for _ in 0..5 {
        bump(&test.session, conversation_id, 1.0).await;
        settle_line(&test.session).await;
    }
    assert_eq!(count_of(&test.session, conversation_id).await, counter_json(5.0));
    // Every bump after the first is a change of the same incarnation, never a second creation.
    let creations = test.storage.commits().into_iter().flatten().filter(
        |write| matches!(write, StorageWrite::DocumentCreate { record, .. } if record.kind == "test.retention.counter"),
    );
    assert_eq!(creations.count(), 1);
    test.session.close(&context()).await.unwrap();
    // And a fresh Session over the same storage reads the same value.
    test.storage.reopen();
    let options = SessionOptions { document_retention_ms: Some(0.0), ..SessionOptions::default() };
    let reopened = create_session(test.storage.clone(), options);
    assert_eq!(count_of(&reopened, conversation_id).await, counter_json(5.0));
    reopened.close(&context()).await.unwrap();
}

#[tokio::test]
async fn bounds_a_third_party_family_with_a_member_per_event() {
    let test = open(0.0);
    let conversation_id = create_conversation(&test.session).await;
    for seq in 0..200_u64 {
        let create = move |tx: session::Tx| async move {
            tx.family_doc(&EVENTS, conversation_id, &seq.to_string(), &seq).await.map(drop)
        };
        test.session.commit(create, &context()).await.unwrap();
    }
    emptied(&test).await;
    let found = test.session.family_snapshot(&EVENTS, conversation_id, "123", &context()).await.unwrap();
    assert_eq!(found, Some(Json::from(json!({ "seq": 123 }))));
    test.session.close(&context()).await.unwrap();
}

#[tokio::test]
async fn keeps_the_cache_of_a_long_lived_session_bounded_across_many_conversations() {
    let test = open(10_000.0);
    for _ in 0..300 {
        let conversation_id = create_conversation(&test.session).await;
        bump(&test.session, conversation_id, 1.0).await;
        advance(&test.clock, 1_000.0);
    }
    settle_line(&test.session).await;
    // Only what was used in the last 10 s, about ten conversations' worth.
    assert!(test.sizes().cached <= 12, "{:?}", test.sizes());
    test.session.close(&context()).await.unwrap();
}
