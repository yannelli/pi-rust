//! The remaining upstream inbox cases of queue modes, stale head writes, handoffs, and onYield continuations.

use std::sync::LazyLock;
use std::sync::atomic::{AtomicUsize, Ordering};

use pi_ai::UserContent;
use pi_durable::documents::*;
use pi_durable::harness::generation::{GenerationHooks, generation_task};
use pi_durable::session::Tx;
use pi_durable::{DraftHead, EntryId};
use serde::{Deserialize, Serialize};
use support::ControlledStorage;

use super::*;

pub fn install_generation_hooks(setup: &ChatSetup, name: &str, hooks: GenerationHooks) {
    setup.registry.install(Extension::new(name).hook(hook(&generation_task(), hooks))).unwrap();
}

/// onYield continues the run once with `more`, then lets it end.
pub fn continue_once() -> GenerationHooks {
    let yields = Arc::new(AtomicUsize::new(0));
    GenerationHooks {
        on_yield: Some(Arc::new(move |_, _, _| {
            let more = (yields.fetch_add(1, Ordering::SeqCst) == 0).then(|| UserContent::Text("more".into()));
            Box::pin(async move { Ok(more) })
        })),
        ..GenerationHooks::default()
    }
}

pub fn summary(head: EntryId) -> SubmissionDraft {
    SubmissionDraft::write(EntryDraft { head: Some(DraftHead::Entry(head)), ..EntryDraft::new("summary") })
}

pub async fn reason(submission: &Submission) -> (SubmissionStatus, Option<String>) {
    let status = submission.status(&ctx()).await.unwrap();
    (status.status, status.reason)
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
struct Marker {
    n: u32,
}

static MARKER_DOC: LazyLock<DocToken<Marker, SessionScope>> = LazyLock::new(|| {
    define_doc(DocSpec::new("test.marker", 1, SessionScope, Marker::default, |_, _, _| Ok(false))).unwrap()
});

#[tokio::test]
async fn reads_queue_modes_when_the_final_boundarys_commit_runs_on_the_session_line() {
    let setup = setup();
    let (first, reached, release) = gated("first");
    setup.faux.set_responses([first, answer("both")]);
    let yielded = Deferred::default();
    let signal = yielded.clone();
    let observe = GenerationHooks {
        on_yield: Some(Arc::new(move |_, _, _| {
            signal.resolve();
            Box::pin(async { Ok(None) })
        })),
        ..GenerationHooks::default()
    };
    install_generation_hooks(&setup, "observe", observe);
    let storage = Arc::new(ControlledStorage::default());
    let (harness, root) = open_chat(storage.clone(), &setup).await;
    root.submit(SubmissionDraft::input("a"), &ctx()).await.unwrap();
    reached.notified().await;
    let f1 = root.submit(SubmissionDraft::input("f1"), &ctx()).await.unwrap();
    let f2 = root.submit(SubmissionDraft::input("f2"), &ctx()).await.unwrap();
    // Occupy the line, let the answer queue its boundary commit behind it, then change the mode.
    let held = storage.hold_commits();
    let occupier = root.clone();
    let occupying = tokio::spawn(async move {
        let bump = |tx: Tx| async move { tx.doc(&MARKER_DOC, ()).await?.edit(|marker| marker.n += 1) };
        occupier.commit(bump, &ctx()).await
    });
    held.entered().await;
    release.resolve();
    yielded.wait().await;
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    setup.settings.lock().unwrap().follow_up_mode = Some(QueueMode::All);
    held.release();
    occupying.await.unwrap().unwrap();
    let settled = f2.wait(&ctx()).await.unwrap();
    let first_status = f1.status(&ctx()).await.unwrap();
    assert_eq!((first_status.status, first_status.answer), (settled.status, settled.answer));
    assert_eq!(setup.faux.state().call_count, 2);
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn makes_a_queued_head_write_stale_when_it_targets_an_entry_before_the_active_range() {
    let setup = setup();
    let (first, reached, release) = gated("first");
    setup.faux.set_responses([first, answer("second")]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    let id = root.id;
    let note = move |tx: Tx| async move { tx.append_entry(id, EntryDraft::new("note")).await };
    let old = root.commit(note, &ctx()).await.unwrap();
    root.reset(None, &ctx()).await.unwrap();
    let reset = root.context(&ctx(), None).await.unwrap().head.unwrap();
    let input = root.submit(SubmissionDraft::input("a"), &ctx()).await.unwrap();
    reached.notified().await;
    let stale = root.submit(summary(old.id), &ctx()).await.unwrap();
    let fresh = root.submit(summary(reset.id), &ctx()).await.unwrap();
    release.resolve();
    input.wait(&ctx()).await.unwrap();
    assert_eq!(reason(&stale).await, (SubmissionStatus::Unanswered, Some("stale".into())));
    assert_eq!(reason(&fresh).await.0, SubmissionStatus::Done);
    assert!(inbox(&harness, &root).await.is_empty());

    // The fresh summary's marker starts the range at the reset: a target inside the range is not stale, even when
    // it is older than the marker itself. A reset placed earlier in the same boundary makes it stale.
    let inside = all_entries(&root).await.into_iter().find(|entry| entry.kind == "pi.user").unwrap();
    let second = root.submit(SubmissionDraft::input("b"), &ctx()).await.unwrap();
    let kept = root.submit(summary(inside.id), &ctx()).await.unwrap();
    second.wait(&ctx()).await.unwrap();
    assert_eq!(reason(&kept).await.0, SubmissionStatus::Done);
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn makes_a_head_write_stale_behind_a_reset_placed_earlier_in_the_same_boundary() {
    let setup = setup();
    let (first, reached, release) = gated("first");
    setup.faux.set_responses([first]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    let input = root.submit(SubmissionDraft::input("a"), &ctx()).await.unwrap();
    reached.notified().await;
    let target = all_entries(&root).await[0].id;
    root.reset(None, &ctx()).await.unwrap();
    let summary = root.submit(summary(target), &ctx()).await.unwrap();
    release.resolve();
    input.wait(&ctx()).await.unwrap();
    assert_eq!(reason(&summary).await, (SubmissionStatus::Unanswered, Some("stale".into())));
    harness.close(&ctx()).await.unwrap();
}

pub fn control(control: ToolControl) -> ToolExecutionResult {
    ToolExecutionResult { output: Some(Vec::new()), control: Some(control), ..ToolExecutionResult::default() }
}

#[tokio::test]
async fn ends_the_run_after_the_handoff_when_the_round_also_terminates() {
    let setup = setup();
    let gate = Deferred::default();
    gate.resolve();
    let both = ToolControl { handoff: Some("continue here".into()), terminate: Some(true), ..ToolControl::default() };
    hold_tool(&setup, gate, control(both));
    setup.faux.set_responses([hold_call()]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    let settled = root.submit(SubmissionDraft::input("a"), &ctx()).await.unwrap().wait(&ctx()).await.unwrap();
    let entries = all_entries(&root).await;
    let calling = entries.iter().find(|entry| entry.kind == "pi.assistant").unwrap();
    assert_eq!((settled.status, settled.answer), (SubmissionStatus::Done, Some(calling.id)));
    assert_eq!(transcript(&entries), ["pi.user:a", "pi.assistant", "pi.tool-result", "pi.reset:continue here"]);
    assert_eq!(setup.faux.state().call_count, 1);
    assert_eq!(harness.snapshot(&LIVE_DOC, root.id, &ctx()).await.unwrap(), Some(Json::from(json!({}))));
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn drops_an_on_yield_continuation_when_the_final_boundary_selects_a_follow_up() {
    let setup = setup();
    let (first, reached, release) = gated("first");
    setup.faux.set_responses([first, answer("second")]);
    install_generation_hooks(&setup, "continue", continue_once());
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    let input = root.submit(SubmissionDraft::input("a"), &ctx()).await.unwrap();
    reached.notified().await;
    let follow_up = root.submit(SubmissionDraft::input("f"), &ctx()).await.unwrap();
    release.resolve();
    follow_up.wait(&ctx()).await.unwrap();
    assert_eq!(reason(&input).await.0, SubmissionStatus::Done);
    let expected = ["pi.user:a", "pi.assistant:first", "pi.user:f", "pi.assistant:second"];
    assert_eq!(transcript(&all_entries(&root).await), expected);
    harness.close(&ctx()).await.unwrap();
}

pub fn failing() -> (FauxResponseStep, Arc<Notify>, Deferred) {
    let error = FauxMessageOptions {
        stop_reason: Some(StopReason::Error),
        error_message: Some("invalid request".into()),
        ..FauxMessageOptions::default()
    };
    let (reached, gate) = (Arc::new(Notify::new()), Deferred::default());
    let (notify, held) = (reached.clone(), gate.clone());
    let step = FauxResponseStep::async_factory(move |_, _, _, _| {
        let (notify, held, error) = (notify.clone(), held.clone(), error.clone());
        Box::pin(async move {
            notify.notify_one();
            held.wait().await;
            Ok(faux_assistant_message(Vec::new(), error))
        })
    });
    (step, reached, gate)
}

#[tokio::test]
async fn settles_a_stale_write_at_once_while_idle_and_a_queued_one_behind_waiting_items() {
    let setup = setup();
    let (failing, reached, release) = failing();
    setup.faux.set_responses([failing, answer("for f")]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    let id = root.id;
    let note = move |tx: Tx| async move { tx.append_entry(id, EntryDraft::new("note")).await };
    let old = root.commit(note, &ctx()).await.unwrap();
    root.reset(None, &ctx()).await.unwrap();
    let before = all_entries(&root).await.len();
    let idle = root.submit(summary(old.id), &ctx()).await.unwrap().wait(&ctx()).await.unwrap();
    assert_eq!((idle.status, idle.reason.as_deref()), (SubmissionStatus::Unanswered, Some("stale")));
    assert_eq!(all_entries(&root).await.len(), before);

    // After a failed run, a follow-up waits; a stale write queues behind it and the boundary rejects it.
    let input = root.submit(SubmissionDraft::input("a"), &ctx()).await.unwrap();
    reached.notified().await;
    let f = root.submit(SubmissionDraft::input("f"), &ctx()).await.unwrap();
    release.resolve();
    input.wait(&ctx()).await.unwrap();
    harness.wait_for_idle(&ctx()).await.unwrap();
    let queued = root.submit(summary(old.id), &ctx()).await.unwrap().wait(&ctx()).await.unwrap();
    assert_eq!((queued.status, queued.reason.as_deref()), (SubmissionStatus::Unanswered, Some("stale")));
    assert_eq!(f.wait(&ctx()).await.unwrap().status, SubmissionStatus::Done);
    harness.close(&ctx()).await.unwrap();
}
