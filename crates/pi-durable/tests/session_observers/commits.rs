//! Upstream `session-observers.test.ts`, "observeCommits" and "observeDoc throwing asynchronously".

use std::sync::atomic::{AtomicUsize, Ordering};

use pi_durable::session::{CommitObservation, CommitObserver, WatchEnd};

use super::*;

#[derive(Clone, Debug, PartialEq)]
enum CommitCall {
    Seq,
    /// The end's kind and error.
    End(&'static str, Option<String>),
}

/// A commit observer recording each publication and its end.
#[derive(Default)]
struct Commits(Mutex<Vec<CommitCall>>);

impl CommitObserver for Commits {
    fn advance(&self, publication: &CommitPublication, _context: &Context) -> DurableResult<()> {
        assert!(publication.seq.get() > 0);
        self.0.lock().unwrap().push(CommitCall::Seq);
        Ok(())
    }

    fn end(&self, end: WatchEnd) {
        self.0.lock().unwrap().push(CommitCall::End(end.kind(), end.error().map(ToString::to_string)));
    }
}

impl Commits {
    fn calls(&self) -> Vec<CommitCall> {
        self.0.lock().unwrap().clone()
    }
}

#[tokio::test]
async fn gets_every_stored_commit_then_ended_once_the_session_closed_after_its_last_publication() {
    let test = open();
    let commits = Arc::new(Commits::default());
    test.session.observe_commits(commits.clone()).unwrap();
    add(&test.session, 1).await;
    add(&test.session, 1).await;
    test.session.close(&context()).await.unwrap();
    assert_eq!(commits.calls(), [CommitCall::Seq, CommitCall::Seq, CommitCall::End("ended", None)]);
}

#[tokio::test]
async fn ends_failed_with_the_error_that_failed_the_session() {
    let test = open();
    let commits = Arc::new(Commits::default());
    test.session.observe_commits(commits.clone()).unwrap();
    test.storage.fail_next_commit(DurableError::error("disk gone"));
    let change =
        |tx: Tx| async move { tx.doc(&SESSION_COUNTER, ()).await?.update(|s| s.set("count", 1))?.map_err(Into::into) };
    let error = test.session.commit(change, &context()).await.unwrap_err();
    assert!(error.to_string().contains("disk gone"));
    test.session.closed().await;
    assert_eq!(commits.calls(), [CommitCall::End("failed", Some("disk gone".into()))]);
}

#[tokio::test]
async fn gets_nothing_after_detach_which_is_idempotent_not_even_its_end() {
    let test = open();
    let commits = Arc::new(Commits::default());
    let observation = test.session.observe_commits(commits.clone()).unwrap();
    add(&test.session, 1).await;
    flush().await;
    observation.detach();
    observation.detach();
    add(&test.session, 1).await;
    test.session.close(&context()).await.unwrap();
    assert_eq!(commits.calls().len(), 1);
}

/// A commit observer whose `advance` fails, counting its calls and recording its ends.
struct Failing {
    calls: AtomicUsize,
    ends: Mutex<Vec<CommitCall>>,
}

impl CommitObserver for Failing {
    fn advance(&self, _publication: &CommitPublication, _context: &Context) -> DurableResult<()> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Err(DurableError::error("commit observer bug"))
    }

    fn end(&self, end: WatchEnd) {
        self.ends.lock().unwrap().push(CommitCall::End(end.kind(), end.error().map(ToString::to_string)));
    }
}

#[tokio::test]
async fn reports_a_throwing_observer_detaches_it_and_ends_it_listener_error_the_others_go_on() {
    let test = open();
    let failing = Arc::new(Failing { calls: AtomicUsize::new(0), ends: Mutex::default() });
    test.session.observe_commits(failing.clone()).unwrap();
    let healthy = Arc::new(Commits::default());
    test.session.observe_commits(healthy.clone()).unwrap();
    add(&test.session, 1).await;
    add(&test.session, 1).await;
    flush().await;
    assert_eq!(failing.calls.load(Ordering::SeqCst), 1);
    assert_eq!(test.reports.listener_errors(), ["commit observer bug"]);
    let ended = CommitCall::End("listener_error", Some("commit observer bug".into()));
    assert_eq!(failing.ends.lock().unwrap().as_slice(), std::slice::from_ref(&ended));
    assert_eq!(healthy.calls().len(), 2);
    test.session.close(&context()).await.unwrap();
    assert_eq!(*failing.ends.lock().unwrap(), [ended]);
}

/// A commit observer that detaches itself in `advance` and then fails.
#[derive(Default)]
struct SelfDetaching {
    observation: Mutex<Option<CommitObservation>>,
    calls: AtomicUsize,
    ends: Mutex<Vec<CommitCall>>,
}

impl CommitObserver for SelfDetaching {
    fn advance(&self, _publication: &CommitPublication, _context: &Context) -> DurableResult<()> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if let Some(observation) = self.observation.lock().unwrap().take() {
            observation.detach();
        }
        Err(DurableError::error("detached then failed"))
    }

    fn end(&self, end: WatchEnd) {
        self.ends.lock().unwrap().push(CommitCall::End(end.kind(), end.error().map(ToString::to_string)));
    }
}

#[tokio::test]
async fn reports_an_observer_that_detached_itself_before_failing_and_never_ends_it() {
    let test = open();
    let observer = Arc::new(SelfDetaching::default());
    let observation = test.session.observe_commits(observer.clone()).unwrap();
    *observer.observation.lock().unwrap() = Some(observation);
    add(&test.session, 1).await;
    add(&test.session, 1).await;
    flush().await;
    assert_eq!(observer.calls.load(Ordering::SeqCst), 1);
    assert_eq!(test.reports.listener_errors(), ["detached then failed"]);
    test.session.close(&context()).await.unwrap();
    assert!(observer.ends.lock().unwrap().is_empty());
}

/// A commit observer whose `end` detaches another observer.
struct DetachingOnEnd {
    other: Mutex<Option<CommitObservation>>,
    ends: Mutex<Vec<CommitCall>>,
}

impl CommitObserver for DetachingOnEnd {
    fn advance(&self, _publication: &CommitPublication, _context: &Context) -> DurableResult<()> {
        Ok(())
    }

    fn end(&self, end: WatchEnd) {
        self.ends.lock().unwrap().push(CommitCall::End(end.kind(), end.error().map(ToString::to_string)));
        if let Some(other) = self.other.lock().unwrap().take() {
            other.detach();
        }
    }
}

#[tokio::test]
async fn ends_no_observer_that_an_earlier_observers_end_detached_at_close() {
    let test = open();
    let first = Arc::new(DetachingOnEnd { other: Mutex::default(), ends: Mutex::default() });
    test.session.observe_commits(first.clone()).unwrap();
    let second = Arc::new(Commits::default());
    let observation = test.session.observe_commits(second.clone()).unwrap();
    *first.other.lock().unwrap() = Some(observation);
    add(&test.session, 1).await;
    flush().await;
    test.session.close(&context()).await.unwrap();
    assert_eq!(*first.ends.lock().unwrap(), [CommitCall::End("ended", None)]);
    assert_eq!(second.calls(), [CommitCall::Seq]);
    assert!(test.reports.0.lock().unwrap().is_empty());
}

// Upstream's observer returns a rejected promise; a Rust observer's `advance` is synchronous and returns the error.
#[tokio::test]
async fn reports_an_observer_whose_call_returns_a_rejected_promise_then_detaches_it_and_ends_it_listener_error() {
    let test = open();
    add(&test.session, 1).await;
    let (calls, ends): (Arc<AtomicUsize>, Ends) = Default::default();
    let counted = calls.clone();
    let failing = Calls(
        move |_: &ValueCall| {
            counted.fetch_add(1, Ordering::SeqCst);
            Err(DurableError::error("async observer bug"))
        },
        record_end(&ends),
    );
    observe(&test.session, Arc::new(failing)).await.unwrap();
    add(&test.session, 1).await;
    flush().await;
    add(&test.session, 1).await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(test.reports.listener_errors(), ["async observer bug"]);
    assert_eq!(*ends.lock().unwrap(), [("listener_error", Some("async observer bug".to_owned()))]);
    test.session.close(&context()).await.unwrap();
    assert_eq!(ends.lock().unwrap().len(), 1);
}
