//! Upstream `harness-submissions.test.ts`, "resumes a wait only once observeCommits() observers saw the commit that
//! settled it".

use std::collections::HashSet;
use std::sync::Mutex;
use std::sync::atomic::Ordering;

use super::support::commit_observer;
use super::*;

#[tokio::test]
async fn resumes_a_wait_only_once_observe_commits_observers_saw_the_commit_that_settled_it() {
    let setup = setup();
    setup.faux.set_responses([faux_assistant_message("hi", FauxMessageOptions::default())]);
    let storage = Arc::new(ControlledStorage::default());
    storage.resolve_late.store(true, Ordering::SeqCst);
    let (harness, root) = open_chat(storage, &setup).await;
    let settled = Arc::new(Mutex::new(HashSet::new()));
    let sink = settled.clone();
    harness
        .observe_commits(commit_observer(move |publication, _| {
            for change in &publication.changes {
                if let CommitChange::Submission(record) = change
                    && record.status == SubmissionStatus::Done
                {
                    sink.lock().unwrap().insert(record.id);
                }
            }
            Ok(())
        }))
        .unwrap();
    let submission = root.submit(SubmissionDraft::input("hello"), &ctx()).await.unwrap();
    submission.wait(&ctx()).await.unwrap();
    assert!(settled.lock().unwrap().contains(&submission.id));
    harness.close(&ctx()).await.unwrap();
}
