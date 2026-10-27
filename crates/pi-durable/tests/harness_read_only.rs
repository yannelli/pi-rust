//! Port of upstream `test/harness-read-only.test.ts`: a read-only Harness, a viewer of a session another process
//! drives, such as a synced copy on another device.

mod support;

use std::sync::{Arc, Mutex};

use pi_durable::entries::USER_ENTRY;
use pi_durable::harness::live::LIVE_DOC;
use pi_durable::harness::*;
use pi_durable::session::Tx;
use pi_durable::storage::sqlite::open_sqlite_storage;
use pi_durable::testing::faux::RegisterFauxProviderOptions;
use pi_durable::{DurableError, EntryDraft, TaskId, TaskStatus};
use support::chat::answer;
use support::harness_support::*;

fn sqlite_path(directory: &tempfile::TempDir) -> String {
    directory.path().join("session.sqlite").to_string_lossy().into_owned()
}

fn setup() -> ChatSetup {
    chat_setup(RegisterFauxProviderOptions::default())
}

async fn open_read_only(path: &str, setup: &ChatSetup, on_wake: Option<Arc<Mutex<Vec<Wake>>>>) -> Harness {
    let mut options = setup.options();
    options.read_only = true;
    if let Some(wakes) = on_wake {
        options.on_wake = Some(Arc::new(move |wake| {
            wakes.lock().unwrap().push(wake);
            Ok(())
        }));
    }
    Harness::open(Arc::new(open_sqlite_storage(path).await.unwrap()), options, &ctx()).await.unwrap()
}

/// The next sequence the file would commit, read beside every open handle; `None` before the schema exists.
fn head(path: &str) -> Option<i64> {
    let connection = rusqlite::Connection::open(path).unwrap();
    let exists =
        connection.query_row("SELECT 1 FROM sqlite_master WHERE name = 'durable_metadata'", [], |_| Ok(())).is_ok();
    if !exists {
        return None;
    }
    connection.query_row("SELECT next_seq FROM durable_metadata WHERE singleton = 1", [], |row| row.get(0)).ok()
}

/// A session whose root generation is running in another Harness, still open, waiting for the model.
async fn running_session(path: &str, setup: &ChatSetup) -> (Harness, Conversation) {
    let (step, reached) = unanswered();
    setup.faux.set_responses(vec![step]);
    let reaching = reached.notified();
    let (harness, root) = open_chat(Arc::new(open_sqlite_storage(path).await.unwrap()), setup).await;
    root.submit(SubmissionDraft::input("slow"), &ctx()).await.unwrap();
    reaching.await;
    (harness, root)
}

fn read_only<T>(result: Result<T, DurableError>) {
    match result {
        Err(DurableError::ReadOnlySession(_)) => {}
        Err(error) => panic!("expected ReadOnlySession, got {error:?}"),
        Ok(_) => panic!("expected ReadOnlySession"),
    }
}

async fn run_of(harness: &Harness, root: &Conversation) -> TaskId {
    let live = harness.snapshot(&LIVE_DOC, root.id, &ctx()).await.unwrap().unwrap();
    live.get("run").and_then(|run| run.get("taskId")).unwrap().deserialize_into().unwrap()
}

#[tokio::test]
async fn opens_a_session_with_work_running_elsewhere_without_writing() {
    let directory = tempfile::tempdir().unwrap();
    let path = sqlite_path(&directory);
    let (driver, _) = running_session(&path, &setup()).await;
    let before = head(&path);
    let wakes = Arc::new(Mutex::new(Vec::new()));

    let viewer = open_read_only(&path, &setup(), Some(wakes.clone())).await;
    let root = viewer.root(&ctx(), CreateOptions::default()).await.unwrap();
    assert_eq!(kinds(&all_entries(&root).await), [USER_ENTRY.kind()]);
    let run = run_of(&viewer, &root).await;
    assert_eq!(viewer.get_task(run, &ctx()).await.unwrap().unwrap().status(), TaskStatus::Running);
    assert_eq!(viewer.inspect(&ctx()).await.unwrap().scheduling, Scheduling::Paused);
    assert_eq!(viewer.wake().unwrap(), Wake { active: false, at: None });
    viewer.close(&ctx()).await.unwrap();

    assert_eq!(head(&path), before);
    assert!(wakes.lock().unwrap().is_empty());
    driver.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn refuses_writes_and_scheduling_and_stays_usable() {
    let directory = tempfile::tempdir().unwrap();
    let path = sqlite_path(&directory);
    let setup = setup();
    setup.faux.set_responses(vec![answer("one")]);
    let (driver, root) = open_chat(Arc::new(open_sqlite_storage(&path).await.unwrap()), &setup).await;
    root.submit(SubmissionDraft::input("hi"), &ctx()).await.unwrap().wait(&ctx()).await.unwrap();
    driver.close(&ctx()).await.unwrap();
    let before = head(&path);

    let viewer_setup = self::setup();
    let viewer = open_read_only(&path, &viewer_setup, None).await;
    let root = viewer.root(&ctx(), CreateOptions::default()).await.unwrap();
    let id = root.id;
    read_only(
        root.commit(move |tx: Tx| async move { tx.append_entry(id, EntryDraft::new("app.note")).await }, &ctx()).await,
    );
    read_only(root.submit(SubmissionDraft::input("again"), &ctx()).await);
    read_only(viewer.resume());
    read_only(viewer.wait_for_idle(&ctx()).await);
    read_only(root.abort(&ctx(), false).await);
    read_only(viewer.create_conversation(CreateOptions::default(), &ctx()).await);

    // Still healthy: reads work, and a commit that writes nothing succeeds.
    assert_eq!(root.commit(|_| async { Ok("nothing") }, &ctx()).await.unwrap(), "nothing");
    assert_eq!(all_entries(&root).await.len(), 2);
    viewer.close(&ctx()).await.unwrap();
    assert_eq!(head(&path), before);
    assert!(viewer_setup.reports().is_empty());
}

#[tokio::test]
async fn refuses_to_create_the_root_of_an_empty_storage() {
    let directory = tempfile::tempdir().unwrap();
    let path = sqlite_path(&directory);
    let viewer = open_read_only(&path, &setup(), None).await;
    let options = CreateOptions::default().with_agent(AgentChange::model(faux_model()));
    read_only(viewer.root(&ctx(), options).await);
    viewer.close(&ctx()).await.unwrap();
    assert_eq!(head(&path), Some(1));
}

#[tokio::test]
async fn leaves_recovery_to_the_next_writable_open() {
    let directory = tempfile::tempdir().unwrap();
    let path = sqlite_path(&directory);
    let (driver, root) = running_session(&path, &setup()).await;
    let task_id = run_of(&driver, &root).await;

    let viewer = open_read_only(&path, &setup(), None).await;
    viewer.close(&ctx()).await.unwrap();

    // The writable open recovers the task once, as if the viewer had never opened it.
    let (writer, _) = open_chat(Arc::new(open_sqlite_storage(&path).await.unwrap()), &setup()).await;
    let record = writer.get_task(task_id, &ctx()).await.unwrap().unwrap();
    assert_eq!(record.status(), TaskStatus::Pending);
    assert_eq!(record.restarts, Some(1));
    writer.close(&ctx()).await.unwrap();
    driver.close(&ctx()).await.unwrap();
}
