//! Watch delivery: exact frames, serialization, overflow, and retirement folding.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use pi_chord::delta::{Op, apply_immutable};
use pi_durable::session::WatchEnd;
use pi_durable::{CommitChange, DocumentCommitChange};

use super::support::*;
use super::*;

#[tokio::test]
async fn never_creates_an_absent_document() {
    let test = open_test_session();
    let commits = test.storage.commit_count();
    assert!(test.session.watch_doc(&STATE_DOC, (), &context()).await.unwrap().is_none());
    assert_eq!(test.storage.commit_count(), commits);
    assert_eq!(test.storage.mints(), 0);
}

fn last_document(test: &TestSession) -> (Json, Vec<Op>) {
    let publication = test.last_publication();
    let Some(CommitChange::Document(DocumentCommitChange::Document { value: Some(value), ops, .. })) =
        publication.changes.first()
    else {
        panic!("expected a document change")
    };
    (value.clone(), ops.clone())
}

#[tokio::test]
async fn keeps_the_acquisition_revision_until_start_and_delivers_exact_committed_frames() {
    let test = create_state().await;
    let watch = watch_state(&test.session).await;
    let initial = watch.value();
    set_value(&test.session, 1).await;
    flush().await;
    let first = last_document(&test);
    set_value(&test.session, 2).await;
    flush().await;
    let second = last_document(&test);
    assert!(watch.value().same_ref(&initial));
    let inline = Arc::new(AtomicBool::new(true));
    let deliveries: Frames = Arc::default();
    let (sink, flag, observed) = (deliveries.clone(), inline.clone(), watch.clone());
    watch
        .start(move |value, ops, _| {
            assert!(!flag.load(Ordering::SeqCst));
            assert!(observed.value().same_ref(&value));
            sink.lock().unwrap().push((value, ops));
            async { Ok(()) }
        })
        .unwrap();
    inline.store(false, Ordering::SeqCst);
    assert!(deliveries.lock().unwrap().is_empty());
    flush().await;
    let deliveries = deliveries.lock().unwrap().clone();
    assert_eq!(deliveries.iter().map(|(v, _)| value_of(v)).collect::<Vec<_>>(), [1, 2]);
    assert!(deliveries[0].0.same_ref(&first.0));
    assert_eq!(deliveries[0].1, first.1);
    assert!(deliveries[1].0.same_ref(&second.0));
    assert_eq!(deliveries[1].1, second.1);
    assert_eq!(value_of(&initial), 0);
    watch.stop().await;
}

#[tokio::test]
async fn serializes_callbacks_and_buffers_exact_frames_committed_while_one_is_in_flight() {
    let test = create_state().await;
    let watch = watch_state(&test.session).await;
    let (entered, release) = (Deferred::default(), Deferred::default());
    let values: Arc<Mutex<Vec<i64>>> = Arc::default();
    let (active, max_active) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let (sink, gate_in, gate_out) = (values.clone(), entered.clone(), release.clone());
    let (now, max) = (active.clone(), max_active.clone());
    watch
        .start(move |value, _, _| {
            let (sink, gate_in, gate_out, now, max) =
                (sink.clone(), gate_in.clone(), gate_out.clone(), now.clone(), max.clone());
            async move {
                let current = now.fetch_add(1, Ordering::SeqCst) + 1;
                max.fetch_max(current, Ordering::SeqCst);
                let count = {
                    let mut values = sink.lock().unwrap();
                    values.push(value_of(&value));
                    values.len()
                };
                if count == 1 {
                    gate_in.resolve();
                    gate_out.wait().await;
                }
                now.fetch_sub(1, Ordering::SeqCst);
                Ok(())
            }
        })
        .unwrap();
    set_value(&test.session, 1).await;
    entered.wait().await;
    for value in 2..=20 {
        set_value(&test.session, value).await;
    }
    assert_eq!(*values.lock().unwrap(), [1]);
    release.resolve();
    flush().await;
    assert_eq!(max_active.load(Ordering::SeqCst), 1);
    assert_eq!(*values.lock().unwrap(), (1..=20).collect::<Vec<_>>());
    watch.stop().await;
}

#[tokio::test]
async fn allows_a_listener_to_initiate_a_later_session_commit() {
    let test = create_state().await;
    let watch = watch_state(&test.session).await;
    let completed = Deferred::default();
    let values: Arc<Mutex<Vec<i64>>> = Arc::default();
    let (sink, done, session) = (values.clone(), completed.clone(), test.session.clone());
    watch
        .start(move |value, _, _| {
            let (sink, done, session) = (sink.clone(), done.clone(), session.clone());
            async move {
                let current = value_of(&value);
                sink.lock().unwrap().push(current);
                if current == 1 {
                    set_value(&session, 2).await;
                } else {
                    done.resolve();
                }
                Ok(())
            }
        })
        .unwrap();
    set_value(&test.session, 1).await;
    completed.wait().await;
    assert_eq!(*values.lock().unwrap(), [1, 2]);
    watch.stop().await;
}

#[tokio::test]
async fn collapses_101_pending_commits_to_one_root_replacement() {
    let test = create_state().await;
    let watch = watch_state(&test.session).await;
    for value in 1..=101 {
        set_value(&test.session, value).await;
    }
    let frames = record(&watch);
    flush().await;
    let frames = frames.lock().unwrap().clone();
    assert_eq!(frames.len(), 1);
    assert_eq!(value_of(&frames[0].0), 101);
    assert_eq!(frames[0].1, vec![Op::Replace(watch.value())]);
    watch.stop().await;
}

#[tokio::test]
async fn never_folds_the_in_flight_frame_into_an_overflow_reset() {
    let test = create_state().await;
    let watch = watch_state(&test.session).await;
    let (entered, release) = (Deferred::default(), Deferred::default());
    let frames: Frames = Arc::default();
    let (sink, gate_in, gate_out) = (frames.clone(), entered.clone(), release.clone());
    watch
        .start(move |value, ops, _| {
            let (sink, gate_in, gate_out) = (sink.clone(), gate_in.clone(), gate_out.clone());
            async move {
                let first = {
                    let mut frames = sink.lock().unwrap();
                    frames.push((value, ops));
                    frames.len() == 1
                };
                if first {
                    gate_in.resolve();
                    gate_out.wait().await;
                }
                Ok(())
            }
        })
        .unwrap();
    set_value(&test.session, 1).await;
    entered.wait().await;
    for value in 2..=102 {
        set_value(&test.session, value).await;
    }
    release.resolve();
    flush().await;
    let frames = frames.lock().unwrap().clone();
    assert_eq!(frames.len(), 2);
    assert_eq!(value_of(&frames[0].0), 1);
    assert_eq!(value_of(&frames[1].0), 102);
    assert_eq!(frames[1].1, vec![Op::Replace(watch.value())]);
    watch.stop().await;
}

#[tokio::test]
async fn folds_retirement_into_an_overflow_reset_and_then_closes() {
    let test = create_state().await;
    let watch = watch_state(&test.session).await;
    for value in 1..=100 {
        set_value(&test.session, value).await;
    }
    retire_state(&test.session).await;
    let frames = record(&watch);
    assert!(matches!(watch.closed().await, WatchEnd::Retired));
    assert_eq!(*frames.lock().unwrap(), vec![(Json::Null, vec![Op::Replace(Json::Null)])]);
}

/// Upstream delivers a nonempty no-op batch here. The Rust tracker diffs drafts, so the same shift and unshift is
/// an empty batch that commits nothing and delivers nothing.
#[tokio::test]
async fn delivers_nothing_for_a_structural_change_the_tracker_proves_is_a_no_op() {
    let test = create_state().await;
    let watch = watch_state(&test.session).await;
    let initial = watch.value();
    test.session
        .commit(
            |tx| async move {
                tx.doc(&STATE_DOC, ()).await?.update(|state| {
                    let items = state.get_mut("items").unwrap().as_array_mut().unwrap();
                    let first = items.remove(0);
                    items.insert(0, first);
                })
            },
            &context(),
        )
        .await
        .unwrap();
    let newest = test.session.snapshot(&STATE_DOC, (), &context()).await.unwrap().unwrap();
    assert!(newest.same_ref(&initial));
    let frames = record(&watch);
    flush().await;
    assert!(frames.lock().unwrap().is_empty());
    watch.stop().await;
}

#[tokio::test]
async fn can_replay_every_delivered_exact_operation_batch_from_the_acquisition_revision() {
    let test = create_state().await;
    let watch = watch_state(&test.session).await;
    let replica = Arc::new(Mutex::new(watch.value()));
    let state = replica.clone();
    watch
        .start(move |value, ops, _| {
            let mut replica = state.lock().unwrap();
            *replica = apply_immutable(Some(&replica), &ops).unwrap();
            assert_eq!(*replica, value);
            async { Ok(()) }
        })
        .unwrap();
    test.session
        .commit(
            |tx| async move {
                tx.doc(&STATE_DOC, ()).await?.update(|state| {
                    state.set("value", 3).unwrap();
                    state.get_mut("items").unwrap().as_array_mut().unwrap().remove(0);
                })
            },
            &context(),
        )
        .await
        .unwrap();
    flush().await;
    assert_eq!(*replica.lock().unwrap(), watch.value());
    watch.stop().await;
}
