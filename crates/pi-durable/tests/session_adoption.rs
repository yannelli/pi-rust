//! Port of upstream `test/session-adoption.test.ts`: the Session adopts inside `Storage::commit()`.

#[path = "support/mod.rs"]
mod support;

use std::sync::LazyLock;

use pi_chord::Json;
use pi_durable::DurableError;
use pi_durable::documents::*;
use pi_durable::session::{Session, SessionEnd};
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
