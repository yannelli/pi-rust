//! Port of upstream `test/session-observers.test.ts`: `Session::observe_doc()`, the synchronous observer of one
//! document address that watches are built on.

#[path = "support/mod.rs"]
mod support;

use std::sync::{Arc, LazyLock, Mutex, Weak};

use pi_chord::{AbortController, ChordError, Context, Json, WatchChange, with_abort_signal};
use pi_durable::documents::*;
use pi_durable::session::{Observation, Observer, Session, SessionHooks, SessionOptions, Tx, create_session};
use pi_durable::*;
use support::*;

static SESSION_COUNTER: LazyLock<DocToken<Json, SessionScope>> = LazyLock::new(|| {
    define_doc(DocSpec::new("observe.session-counter", 1, SessionScope, || json(r#"{"count":0}"#), |_, _, _| Ok(false)))
        .unwrap()
});

/// A latest conversation document, the kind a deferred commit may hold.
static CONVERSATION_COUNTER: LazyLock<DocToken<Json, LatestConversation>> = LazyLock::new(|| {
    let scope = LatestConversation(LatestFork::Initial);
    define_doc(DocSpec::new("observe.conversation-counter", 1, scope, || json(r#"{"count":0}"#), |_, _, _| Ok(false)))
        .unwrap()
});

fn counter(count: i64) -> Json {
    json(&format!(r#"{{"count":{count}}}"#))
}

/// Keeps the Session's reports, which a plain Session drops.
#[derive(Default)]
struct Reports(Mutex<Vec<(String, ReportInfo)>>);

impl SessionHooks for Reports {
    fn report(&self, error: &DurableError, info: &ReportInfo, _context: Option<&Context>) {
        self.0.lock().unwrap().push((error.to_string(), info.clone()));
    }
}

impl Reports {
    fn listener_errors(&self) -> Vec<String> {
        let reports = self.0.lock().unwrap();
        reports.iter().filter(|(_, info)| *info == ReportInfo::ListenerError).map(|(error, _)| error.clone()).collect()
    }
}

struct Opened {
    storage: Arc<ControlledStorage>,
    session: Session,
    reports: Arc<Reports>,
}

fn open() -> Opened {
    let storage = Arc::new(ControlledStorage::default());
    let reports = Arc::new(Reports::default());
    let options = SessionOptions {
        hooks: Some(reports.clone()),
        document_retention_ms: Some(600_000.0),
        ..SessionOptions::default()
    };
    Opened { session: create_session(storage.clone(), options), storage, reports }
}

#[derive(Clone, Debug, PartialEq)]
enum Call {
    /// A change, and whether it came inside the storage's `adopt`.
    Advance(WatchChange, bool),
    End(Option<String>),
}

/// An observer that records each call, and whether it ran inside the storage's `adopt`.
struct Recorder {
    storage: Arc<ControlledStorage>,
    calls: Mutex<Vec<Call>>,
}

impl Observer for Recorder {
    fn advance(&self, change: &WatchChange, _context: &Context) -> DurableResult<()> {
        self.calls.lock().unwrap().push(Call::Advance(change.clone(), self.storage.adopting()));
        Ok(())
    }

    fn end(&self, failure: Option<DurableError>) {
        self.calls.lock().unwrap().push(Call::End(failure.map(|error| error.to_string())));
    }
}

fn recorder(storage: &Arc<ControlledStorage>) -> Arc<Recorder> {
    Arc::new(Recorder { storage: storage.clone(), calls: Mutex::default() })
}

impl Recorder {
    fn calls(&self) -> Vec<Call> {
        self.calls.lock().unwrap().clone()
    }
}

/// An observer made of two closures.
struct Calls<A, E>(A, E);

impl<A, E> Observer for Calls<A, E>
where
    A: Fn(&WatchChange) -> DurableResult<()> + Send + Sync,
    E: Fn(Option<DurableError>) + Send + Sync,
{
    fn advance(&self, change: &WatchChange, _context: &Context) -> DurableResult<()> {
        (self.0)(change)
    }

    fn end(&self, failure: Option<DurableError>) {
        (self.1)(failure)
    }
}

fn ops(text: &str) -> Vec<pi_chord::delta::Op> {
    pi_chord::delta::parse_ops(&json(text)).unwrap()
}

fn advance(value: Json, ops: Vec<pi_chord::delta::Op>, retired: bool) -> Call {
    Call::Advance(WatchChange { value, ops, retired }, true)
}

async fn add(session: &Session, by: i64) {
    let change = move |tx: Tx| async move {
        tx.doc(&SESSION_COUNTER, ())
            .await?
            .update(|s| {
                let count = s["count"].as_i64().unwrap();
                s.set("count", count + by)
            })?
            .map_err(Into::into)
    };
    session.commit(change, &context()).await.unwrap();
}

async fn set_conversation_count(session: &Session, id: ConversationId, count: i64) {
    let change = move |tx: Tx| async move {
        tx.doc(&CONVERSATION_COUNTER, id).await?.update(|s| s.set("count", count))?.map_err(Into::into)
    };
    session.commit(change, &context()).await.unwrap();
}

async fn observe(session: &Session, observer: Arc<dyn Observer>) -> Option<Observation> {
    session.observe_doc(&SESSION_COUNTER, (), observer, &context()).await.unwrap()
}

async fn observe_conversation(session: &Session, id: ConversationId, observer: Arc<dyn Observer>) -> Observation {
    session.observe_doc(&CONVERSATION_COUNTER, id, observer, &context()).await.unwrap().unwrap()
}

#[tokio::test]
async fn returns_none_for_an_absent_address_and_attaches_nothing() {
    let test = open();
    let recorder = recorder(&test.storage);
    assert!(observe(&test.session, recorder.clone()).await.is_none());
    add(&test.session, 1).await;
    assert!(recorder.calls().is_empty());
    assert_eq!(test.session.document_cache_sizes().attached, 0);
    test.session.close(&context()).await.unwrap();
}

#[tokio::test]
async fn starts_at_the_committed_value_and_advances_inside_adopt_with_each_stored_commit_s_ops() {
    let test = open();
    add(&test.session, 1).await;
    let recorder = recorder(&test.storage);
    let observation = observe(&test.session, recorder.clone()).await.unwrap();
    assert_eq!(observation.value, counter(1));
    add(&test.session, 2).await;
    assert_eq!(recorder.calls(), [advance(counter(3), ops(r#"[["s",["count"],3]]"#), false)]);
    observation.detach();
    test.session.close(&context()).await.unwrap();
}

#[tokio::test]
async fn advances_when_memory_adopts_a_held_commit_before_anything_is_stored() {
    let test = open();
    let id = create_conversation(&test.session).await;
    set_conversation_count(&test.session, id, 0).await;
    let recorder = recorder(&test.storage);
    let observation = observe_conversation(&test.session, id, recorder.clone()).await;
    let stored = test.storage.commit_count();
    let change = move |tx: Tx| async move {
        tx.doc(&CONVERSATION_COUNTER, id).await?.update(|s| s.set("count", 5))?.map_err(Into::into)
    };
    let options = CommitOptions::deferred(Some(f64::INFINITY));
    test.session.commit_with_options(change, &context(), options).await.unwrap();
    assert_eq!(test.storage.commit_count(), stored);
    let held = Call::Advance(WatchChange::new(counter(5), ops(r#"[["s",["count"],5]]"#)), false);
    assert_eq!(recorder.calls(), [held]);
    observation.detach();
    test.session.close(&context()).await.unwrap();
}

#[tokio::test]
async fn follows_a_conversation_document_s_address_a_retirement_with_the_last_value_then_the_new_incarnation() {
    let test = open();
    let id = create_conversation(&test.session).await;
    set_conversation_count(&test.session, id, 2).await;
    let recorder = recorder(&test.storage);
    let observation = observe_conversation(&test.session, id, recorder.clone()).await;
    let retire = move |tx: Tx| async move { tx.retire_doc(&CONVERSATION_COUNTER, id).await };
    test.session.commit(retire, &context()).await.unwrap();
    // The address is absent now: a new attachment finds nothing, the existing one goes on.
    let absent = test.session.observe_doc(&CONVERSATION_COUNTER, id, recorder.clone(), &context()).await;
    assert!(absent.unwrap().is_none());
    set_conversation_count(&test.session, id, 9).await;
    let expected = [
        advance(observation.value.clone(), Vec::new(), true),
        advance(counter(9), vec![pi_chord::delta::Op::Replace(counter(9))], false),
    ];
    assert_eq!(recorder.calls(), expected);
    let Call::Advance(retirement, _) = &recorder.calls()[0] else { panic!() };
    assert!(retirement.value.same_ref(&observation.value));
    observation.detach();
    test.session.close(&context()).await.unwrap();
}

#[tokio::test]
async fn gets_nothing_after_detach_which_is_idempotent_and_unpins_the_document() {
    let test = open();
    add(&test.session, 1).await;
    let recorder = recorder(&test.storage);
    let observation = observe(&test.session, recorder.clone()).await.unwrap();
    assert_eq!(test.session.document_cache_sizes().attached, 1);
    observation.detach();
    observation.detach();
    assert_eq!(test.session.document_cache_sizes().attached, 0);
    add(&test.session, 1).await;
    test.session.close(&context()).await.unwrap();
    assert!(recorder.calls().is_empty());
}

#[tokio::test]
async fn ends_once_when_the_session_closes() {
    let test = open();
    add(&test.session, 1).await;
    let recorder = recorder(&test.storage);
    observe(&test.session, recorder.clone()).await.unwrap();
    test.session.close(&context()).await.unwrap();
    assert_eq!(recorder.calls(), [Call::End(None)]);
}

#[tokio::test]
async fn ends_with_the_error_that_failed_the_session() {
    let test = open();
    add(&test.session, 1).await;
    let recorder = recorder(&test.storage);
    observe(&test.session, recorder.clone()).await.unwrap();
    test.storage.fail_next_commit(DurableError::error("disk gone"));
    let change =
        |tx: Tx| async move { tx.doc(&SESSION_COUNTER, ()).await?.update(|s| s.set("count", 2))?.map_err(Into::into) };
    let error = test.session.commit(change, &context()).await.unwrap_err();
    assert!(error.to_string().contains("disk gone"));
    test.session.closed().await;
    assert_eq!(recorder.calls(), [Call::End(Some("disk gone".into()))]);
}

#[tokio::test]
async fn reports_a_throwing_observer_detaches_it_and_ends_it_with_the_error_the_commit_and_other_observers_go_on() {
    let test = open();
    add(&test.session, 1).await;
    let ends: Ends = Arc::default();
    let throwing = Calls(|_: &WatchChange| Err(DurableError::error("observer bug")), record_end(&ends));
    observe(&test.session, Arc::new(throwing)).await.unwrap();
    let healthy = recorder(&test.storage);
    observe(&test.session, healthy.clone()).await.unwrap();
    add(&test.session, 1).await;
    add(&test.session, 1).await;
    assert_eq!(test.reports.listener_errors(), ["observer bug"]);
    assert_eq!(*ends.lock().unwrap(), [Some("observer bug".to_owned())]);
    assert_eq!(healthy.calls().len(), 2);
    assert_eq!(test.session.snapshot(&SESSION_COUNTER, (), &context()).await.unwrap(), Some(counter(3)));
    test.session.close(&context()).await.unwrap();
    // Detached at its failure: the close does not end it again.
    assert_eq!(ends.lock().unwrap().len(), 1);
    assert!(test.session.failure().is_none());
}

#[tokio::test]
async fn attaches_atomically_on_the_line_a_commit_queued_before_the_attachment_is_in_its_value() {
    let test = open();
    add(&test.session, 1).await;
    let recorder = recorder(&test.storage);
    let session = test.session.clone();
    let committing = tokio::spawn(async move { add(&session, 10).await });
    tokio::task::yield_now().await;
    let observation = observe(&test.session, recorder.clone()).await.unwrap();
    committing.await.unwrap();
    assert_eq!(observation.value, counter(11));
    assert!(recorder.calls().is_empty());
    observation.detach();
    test.session.close(&context()).await.unwrap();
}

#[tokio::test]
async fn rejects_an_attachment_whose_context_is_cancelled_and_attaches_nothing() {
    let test = open();
    add(&test.session, 1).await;
    let controller = AbortController::new();
    controller.abort(Some(ChordError::error("gone")));
    let cancelled = with_abort_signal(&controller.signal(), &context());
    let attached = test.session.observe_doc(&SESSION_COUNTER, (), recorder(&test.storage), &cancelled).await;
    assert!(attached.unwrap_err().to_string().contains("gone"));
    assert_eq!(test.session.document_cache_sizes().attached, 0);
    test.session.close(&context()).await.unwrap();
}

#[tokio::test]
async fn keeps_no_detached_observer_alive() {
    let test = open();
    add(&test.session, 1).await;
    // Attach an observer only the Session references; `detach` says whether to detach it before returning.
    let attach = async |detach: bool| -> (Observation, Weak<Recorder>) {
        let observer = recorder(&test.storage);
        let weak = Arc::downgrade(&observer);
        let observation = observe(&test.session, observer).await.unwrap();
        if detach {
            observation.detach();
        }
        (observation, weak)
    };
    // Control: an attached observer stays, so the drop below is real.
    let (attached, observer) = attach(false).await;
    assert!(observer.upgrade().is_some());
    attached.detach();
    let (_detached, observer) = attach(true).await;
    assert!(observer.upgrade().is_none());
    test.session.close(&context()).await.unwrap();
}

#[tokio::test]
async fn rejects_on_a_closed_session() {
    let test = open();
    add(&test.session, 1).await;
    test.session.close(&context()).await.unwrap();
    let closed = test.session.observe_doc(&SESSION_COUNTER, (), recorder(&test.storage), &context()).await;
    assert!(closed.unwrap_err().to_string().contains("Session is closed"));
}

/// A Session with a conversation counter at 1, and the commit that retires it and creates it again at 2.
async fn retiring() -> (Opened, ConversationId) {
    let test = open();
    let id = create_conversation(&test.session).await;
    set_conversation_count(&test.session, id, 1).await;
    (test, id)
}

async fn recreate(session: &Session, id: ConversationId) {
    let change = move |tx: Tx| async move {
        tx.retire_doc(&CONVERSATION_COUNTER, id).await?;
        tx.doc(&CONVERSATION_COUNTER, id).await?.update(|s| s.set("count", 2))?.map_err(Into::into)
    };
    session.commit(change, &context()).await.unwrap();
}

type Ends = Arc<Mutex<Vec<Option<String>>>>;

fn record_end(ends: &Ends) -> impl Fn(Option<DurableError>) + Send + Sync + use<> {
    let sink = ends.clone();
    move |failure| sink.lock().unwrap().push(failure.map(|error| error.to_string()))
}

#[tokio::test]
async fn gets_nothing_more_of_the_commit_once_it_fails_on_the_retirement_and_pins_nothing_for_the_recreation() {
    let (test, id) = retiring().await;
    let (changes, ends): (Arc<Mutex<Vec<WatchChange>>>, Ends) = Default::default();
    let sink = changes.clone();
    let failing = Calls(
        move |change: &WatchChange| {
            sink.lock().unwrap().push(change.clone());
            Err(DurableError::error("observer bug"))
        },
        record_end(&ends),
    );
    let observation = observe_conversation(&test.session, id, Arc::new(failing)).await;
    recreate(&test.session, id).await;
    let retirement = WatchChange { value: observation.value.clone(), ops: Vec::new(), retired: true };
    assert_eq!(*changes.lock().unwrap(), [retirement]);
    assert_eq!(*ends.lock().unwrap(), [Some("observer bug".to_owned())]);
    assert_eq!(test.reports.listener_errors(), ["observer bug"]);
    assert_eq!(test.session.document_cache_sizes().attached, 0);
    observation.detach();
    assert_eq!(test.session.document_cache_sizes().attached, 0);
    test.session.close(&context()).await.unwrap();
}

#[tokio::test]
async fn gets_nothing_more_of_the_commit_once_it_detaches_in_the_retirement_call_and_pins_nothing() {
    let (test, id) = retiring().await;
    let (changes, ends): (Arc<Mutex<Vec<WatchChange>>>, Ends) = Default::default();
    let attached: Arc<Mutex<Option<Observation>>> = Arc::default();
    let (sink, own) = (changes.clone(), attached.clone());
    let detaching = Calls(
        move |change: &WatchChange| {
            sink.lock().unwrap().push(change.clone());
            if let Some(observation) = own.lock().unwrap().as_ref() {
                observation.detach();
            }
            Ok(())
        },
        record_end(&ends),
    );
    let observation = observe_conversation(&test.session, id, Arc::new(detaching)).await;
    *attached.lock().unwrap() = Some(observation.clone());
    recreate(&test.session, id).await;
    let retirement = WatchChange { value: observation.value.clone(), ops: Vec::new(), retired: true };
    assert_eq!(*changes.lock().unwrap(), [retirement]);
    assert_eq!(test.session.document_cache_sizes().attached, 0);
    test.session.close(&context()).await.unwrap();
    assert!(ends.lock().unwrap().is_empty());
    attached.lock().unwrap().take();
}

#[tokio::test]
async fn gets_nothing_of_the_commit_when_another_observer_s_call_detached_it_first_and_pins_nothing() {
    let (test, id) = retiring().await;
    let second: Arc<Mutex<Option<Observation>>> = Arc::default();
    let other = second.clone();
    let first_observer = Calls(
        move |_: &WatchChange| {
            if let Some(observation) = other.lock().unwrap().as_ref() {
                observation.detach();
            }
            Ok(())
        },
        |_| {},
    );
    let first = observe_conversation(&test.session, id, Arc::new(first_observer)).await;
    let detached = recorder(&test.storage);
    *second.lock().unwrap() = Some(observe_conversation(&test.session, id, detached.clone()).await);
    recreate(&test.session, id).await;
    assert!(detached.calls().is_empty());
    // Only the first observer pins the new incarnation; its detach leaves nothing pinned.
    first.detach();
    assert_eq!(test.session.document_cache_sizes().attached, 0);
    test.session.close(&context()).await.unwrap();
    second.lock().unwrap().take();
}
