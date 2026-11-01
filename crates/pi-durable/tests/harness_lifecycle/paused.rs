//! Scheduling on a paused Harness and registry changes before resume.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use pi_durable::harness::*;
use pi_durable::session::Tx;
use pi_durable::storage::MemoryStorage;
use pi_durable::*;

use super::sealed::{Operation, op};
use super::support::task_support::{eventually, open_tasks};
use super::support::{context, flush};
use super::{notes_doc, one_step, runner, set_text, start};

struct Paused {
    harness: Harness,
    root: Conversation,
    id: TaskId,
    submission_id: SubmissionId,
    ran: Arc<AtomicBool>,
}

/// Open a paused Harness with one background task pending and a queued write submission.
async fn paused() -> Paused {
    let ran = Arc::new(AtomicBool::new(false));
    let flag = ran.clone();
    let marker = one_step(
        "test.marker",
        runner(move || {
            flag.store(true, Ordering::SeqCst);
            async {}
        }),
    );
    let harness = open_tasks(Arc::new(MemoryStorage::new()), vec![marker.clone()]).await.harness;
    let root = harness.root(&context(), CreateOptions::default()).await.unwrap();
    // Background, so idle waits and conversation abort leave it alone.
    let id = start(&root, &marker, true).await;
    let conversation_id = root.id;
    let create = move |tx: Tx| async move {
        Ok(tx.create_submission(SubmissionCreate::queued(conversation_id, SubmissionType::Write, None)).await?.id)
    };
    let submission_id = harness.commit(create, &context()).await.unwrap();
    Paused { harness, root, id, submission_id, ran }
}

#[tokio::test]
async fn never_schedules_from_a_read_only_viewer() {
    let Paused { harness, root, id, submission_id, ran } = paused().await;
    let notes = notes_doc("test.viewer-notes");
    set_text(&harness, &notes, "").await;
    let c = context();
    harness.inspect(&c).await.unwrap();
    harness.get_task(id, &c).await.unwrap();
    harness.usage(&c).await.unwrap();
    harness.conversation(root.id, &c).await.unwrap();
    harness.snapshot(&notes, (), &c).await.unwrap();
    harness.document_state(&notes, (), &c).await.unwrap().unwrap().dispose().unwrap();
    harness.watch_doc(&notes, (), &c).await.unwrap().unwrap().stop().await;
    harness.submission(submission_id, &c).await.unwrap().unwrap().status(&c).await.unwrap();
    root.agent(&c).await.unwrap();
    root.context(&c, None).await.unwrap();
    root.entries(EntryRange::default(), 10, None, &c).await.unwrap();
    root.view_state(&c).await.unwrap().dispose().unwrap();
    root.watch(&c, None).await.unwrap().stop().await;
    harness.task_graph(&c).await.unwrap().dispose().unwrap();
    harness.watch_task_graph(&c).await.unwrap().stop().await;
    flush().await;
    flush().await;
    assert!(!ran.load(Ordering::SeqCst));
    assert_eq!(harness.inspect(&c).await.unwrap().scheduling, Scheduling::Paused);
    harness.close(&c).await.unwrap();
}

#[tokio::test]
async fn schedules_from_every_progress_call() {
    let names = ["submit", "compact", "abort", "conversationIdle", "submissionWait", "waitForTask", "harnessIdle"];
    for name in names {
        let opened = paused().await;
        let (harness, root, c) = (opened.harness.clone(), opened.root.clone(), context());
        let (id, submission_id) = (opened.id, opened.submission_id);
        let call: Operation = match name {
            "submit" => op(async move { root.submit(SubmissionDraft::write(EntryDraft::new("note")), &c).await }),
            "compact" => op(async move { root.compact(None, &c).await }),
            "abort" => op(async move { root.abort(&c, false).await }),
            "conversationIdle" => op(async move { root.wait_for_idle(&c).await }),
            "submissionWait" => op(async move { harness.submission(submission_id, &c).await?.unwrap().wait(&c).await }),
            "waitForTask" => op(async move { harness.wait_for_task(id, &c).await }),
            _ => op(async move { harness.wait_for_idle(&c).await }),
        };
        // A queued write's wait settles only on placement; close rejects it.
        let pending = tokio::spawn(call);
        let ran = opened.ran.clone();
        eventually(|| {
            let ran = ran.load(Ordering::SeqCst);
            async move { ran }
        })
        .await;
        opened.harness.close(&context()).await.unwrap();
        let _ = pending.await;
    }
}

#[tokio::test]
async fn runs_what_the_registry_holds_at_resume_a_definition_installed_or_replaced_after_open() {
    let log = Arc::new(std::sync::Mutex::new(Vec::<&'static str>::new()));
    let define = |label: &'static str| {
        let sink = log.clone();
        one_step(
            "test.late-definition",
            runner(move || {
                sink.lock().unwrap().push(label);
                async {}
            }),
        )
    };
    let opened = open_tasks(Arc::new(MemoryStorage::new()), Vec::new()).await;
    let harness = opened.harness;
    let root = harness.root(&context(), CreateOptions::default()).await.unwrap();
    let missing = start(&root, &define("unused"), false).await;
    let states =
        |inspection: HarnessInspection| inspection.tasks.into_iter().map(|task| task.state).collect::<Vec<_>>();
    let blocked = states(harness.inspect(&context()).await.unwrap());
    assert!(
        matches!(blocked[..], [TaskInspectionState::Blocked { reason: BlockedReason::MissingTask, .. }]),
        "{blocked:?}"
    );
    let install = |task: Task| opened.registry.install(Extension::new("task:test.late-definition").task(task)).unwrap();
    install(define("v1"));
    let ready = states(harness.inspect(&context()).await.unwrap());
    assert!(matches!(ready[..], [TaskInspectionState::Pending { .. }]), "{ready:?}");
    // The same extension name replaces the definition in place, still before resume.
    opened.registry.uninstall("task:test.late-definition").unwrap();
    install(define("v2"));
    harness.resume().unwrap();
    harness.wait_for_task(missing, &context()).await.unwrap();
    assert_eq!(*log.lock().unwrap(), ["v2"]);
    harness.close(&context()).await.unwrap();
}
