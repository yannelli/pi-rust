//! Port of upstream `test/session-adoption.test.ts`: the Session adopts inside `Storage::commit()`.

#[path = "support/mod.rs"]
mod support;

use std::sync::{Arc, LazyLock, Mutex};

use pi_chord::{Context, Json};
use pi_durable::documents::*;
use pi_durable::session::{Session, SessionEnd, SessionHooks, SessionOptions, create_session};
use pi_durable::{DurableError, ReportInfo};
use support::*;

static COUNTER_DOC: LazyLock<DocToken<Json, SessionScope>> = LazyLock::new(|| {
    let initial = || json(r#"{"count":0}"#);
    define_doc(DocSpec::new("adoption.counter", 1, SessionScope, initial, |_, _, _| Ok(false))).unwrap()
});

async fn bump(session: &Session) -> Result<(), DurableError> {
    session
        .commit(
            |tx| async move {
                let draft = tx.doc(&COUNTER_DOC, ()).await?;
                draft.update(|state| {
                    let count = state.get("count").and_then(Json::as_i64).unwrap_or(0);
                    state.set("count", count + 1)
                })??;
                Ok(())
            },
            &context(),
        )
        .await
}

fn failure(end: SessionEnd) -> String {
    match end {
        SessionEnd::Failed(error) => error.to_string(),
        SessionEnd::Closed => panic!("the Session closed without failing"),
    }
}

#[tokio::test]
async fn adopts_synchronously_inside_the_commit_before_storage_resolves() {
    let test = open_test_session();
    bump(&test.session).await.unwrap();
    assert_eq!(*test.storage.adoption_order.lock().unwrap(), ["adopt", "adopted", "resolve"]);
    let snapshot = test.session.snapshot(&COUNTER_DOC, (), &context()).await.unwrap();
    assert_eq!(snapshot, Some(json(r#"{"count":1}"#)));
    test.session.close(&context()).await.unwrap();
}

#[tokio::test]
async fn fails_the_session_without_discarding_when_storage_rejects_after_adoption() {
    let test = open_test_session();
    bump(&test.session).await.unwrap();
    *test.storage.adoption_fault.lock().unwrap() = Some(AdoptionFault::RejectAfterAdopt);
    assert_eq!(message(bump(&test.session).await), "reclaim failed");
    assert_eq!(test.storage.adoption_order.lock().unwrap()[3..], ["adopt", "adopted", "resolve"]);
    assert_eq!(failure(test.session.closed().await), "reclaim failed");
}

#[tokio::test]
async fn discards_the_staged_changes_when_storage_rejects_before_adoption() {
    let test = open_test_session();
    bump(&test.session).await.unwrap();
    let publications = test.publication_count();
    test.storage.fail_next_commit(DurableError::storage("disk full"));
    assert_eq!(message(bump(&test.session).await), "disk full");
    assert_eq!(test.publication_count(), publications);
    assert_eq!(failure(test.session.closed().await), "disk full");
}

#[tokio::test]
async fn fails_the_session_when_storage_resolves_without_adopting() {
    let test = open_test_session();
    *test.storage.adoption_fault.lock().unwrap() = Some(AdoptionFault::SkipAdopt);
    assert!(message(bump(&test.session).await).contains("without adopting"));
    assert!(failure(test.session.closed().await).contains("without adopting"));
}

/// Record `label` in the order of the storage's `adopt`, `adopted`, and `resolve`.
fn mark(test: &TestSession, label: &'static str) -> impl Fn() + Send + Sync + use<> {
    let storage = test.storage.clone();
    move || storage.adoption_order.lock().unwrap().push(label)
}

#[tokio::test]
async fn runs_its_own_observers_then_host_listeners_inside_adopt_before_storage_resolves() {
    let test = open_test_session();
    let (observed, host) = (mark(&test, "observed"), mark(&test, "host"));
    let storage = test.storage.clone();
    let inside = Arc::new(Mutex::new(Vec::new()));
    let seen = inside.clone();
    test.session
        .follow_commits(move |_, _| {
            seen.lock().unwrap().push(storage.adopting());
            observed();
            Ok(())
        })
        .unwrap();
    test.session
        .observe_commits(commit_observer(move |_, _| {
            host();
            Ok(())
        }))
        .unwrap();
    bump(&test.session).await.unwrap();
    assert_eq!(*test.storage.adoption_order.lock().unwrap(), ["adopt", "observed", "host", "adopted", "resolve"]);
    assert_eq!(*inside.lock().unwrap(), [true]);
    test.session.close(&context()).await.unwrap();
}

#[tokio::test]
async fn fails_the_session_after_storage_returned_for_a_failure_of_its_own_observers_the_others_and_hosts_got_it() {
    let test = open_test_session();
    let (observed, after, host, close) =
        (mark(&test, "observed"), mark(&test, "observed after"), mark(&test, "host"), mark(&test, "close"));
    test.session
        .follow_commits(move |_, _| {
            observed();
            Err(DurableError::error("observer bug"))
        })
        .unwrap();
    test.session
        .follow_commits(move |_, _| {
            after();
            Ok(())
        })
        .unwrap();
    let session = test.session.clone();
    test.session
        .observe_commits(commit_observer(move |_, _| {
            assert!(session.failure().is_none());
            {
                host();
                Ok(())
            }
        }))
        .unwrap();
    test.session.subscribe_close(close).unwrap();
    bump(&test.session).await.unwrap();
    // `adopted` and `resolve`: adopt returned to Storage, and the Session failed only after that.
    let order = ["adopt", "observed", "observed after", "host", "adopted", "resolve", "close"];
    assert_eq!(*test.storage.adoption_order.lock().unwrap(), order);
    assert_eq!(failure(test.session.closed().await), "observer bug");
    test.storage.reopen();
    let reopened = create_session(test.storage.clone(), SessionOptions::default());
    let snapshot = reopened.snapshot(&COUNTER_DOC, (), &context()).await.unwrap();
    assert_eq!(snapshot, Some(json(r#"{"count":1}"#)));
    reopened.close(&context()).await.unwrap();
}

#[tokio::test]
async fn publishes_a_commit_that_storage_rejects_after_adopting_it_then_fails_the_session() {
    let test = open_test_session();
    bump(&test.session).await.unwrap();
    let (observed, host) = (mark(&test, "observed"), mark(&test, "host"));
    test.session
        .follow_commits(move |_, _| {
            observed();
            Ok(())
        })
        .unwrap();
    test.session
        .observe_commits(commit_observer(move |_, _| {
            host();
            Ok(())
        }))
        .unwrap();
    let publications = test.publication_count();
    test.storage.adoption_order.lock().unwrap().clear();
    *test.storage.adoption_fault.lock().unwrap() = Some(AdoptionFault::RejectAfterAdopt);
    assert_eq!(message(bump(&test.session).await), "reclaim failed");
    assert_eq!(*test.storage.adoption_order.lock().unwrap(), ["adopt", "observed", "host", "adopted", "resolve"]);
    assert_eq!(test.publication_count(), publications + 1);
    assert_eq!(failure(test.session.closed().await), "reclaim failed");
}

#[tokio::test]
async fn fails_with_the_storage_error_when_it_rejects_after_adopting_and_reports_an_observer_s_failure() {
    let reports = Arc::new(Mutex::new(Vec::new()));
    let storage = Arc::new(ControlledStorage::default());
    let options = SessionOptions { hooks: Some(Arc::new(Reports(reports.clone()))), ..SessionOptions::default() };
    let session = create_session(storage.clone(), options);
    bump(&session).await.unwrap();
    session.follow_commits(|_, _| Err(DurableError::error("observer bug"))).unwrap();
    *storage.adoption_fault.lock().unwrap() = Some(AdoptionFault::RejectAfterAdopt);
    assert_eq!(message(bump(&session).await), "reclaim failed");
    assert_eq!(failure(session.closed().await), "reclaim failed");
    let reports = reports.lock().unwrap().clone();
    assert!(reports.contains(&("observer bug".to_owned(), ReportInfo::ListenerError)), "{reports:?}");
}

struct Reports(Arc<Mutex<Vec<(String, ReportInfo)>>>);

impl SessionHooks for Reports {
    fn report(&self, error: &DurableError, info: &ReportInfo, _context: Option<&Context>) {
        self.0.lock().unwrap().push((error.to_string(), info.clone()));
    }
}
