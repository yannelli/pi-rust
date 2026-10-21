//! The publication queue (upstream `publish.ts`): FIFO delivery, re-entrant enqueues, and held publications. The
//! queue is process-wide, so one test covers it in this binary.

use std::sync::{Arc, Mutex};

use pi_chord::host2::{enqueue, hold_publications};

#[test]
fn notifications_run_in_enqueue_order_and_wait_for_holds() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let push = |entry: &'static str| {
        let log = log.clone();
        move || log.lock().unwrap().push(entry)
    };

    // A task that enqueues from inside runs its follow-up after everything enqueued before it.
    let inner = push("inner");
    let outer_log = log.clone();
    enqueue(move || {
        outer_log.lock().unwrap().push("outer");
        enqueue(inner);
    });
    assert_eq!(*log.lock().unwrap(), ["outer", "inner"]);
    log.lock().unwrap().clear();

    let returned = hold_publications(|| {
        enqueue(push("first"));
        hold_publications(|| enqueue(push("nested")));
        enqueue(push("last"));
        assert!(log.lock().unwrap().is_empty(), "nothing is delivered while held");
        7
    });
    assert_eq!(returned, 7);
    assert_eq!(*log.lock().unwrap(), ["first", "nested", "last"]);
    log.lock().unwrap().clear();

    // A panicking task does not wedge the queue.
    let caught = std::panic::catch_unwind(|| enqueue(|| panic!("task failed")));
    assert!(caught.is_err());
    enqueue(push("after"));
    assert_eq!(*log.lock().unwrap(), ["after"]);
    let held = std::panic::catch_unwind(|| hold_publications(|| panic!("held failed")));
    assert!(held.is_err());
    enqueue(push("released"));
    assert_eq!(*log.lock().unwrap(), ["after", "released"]);
}
