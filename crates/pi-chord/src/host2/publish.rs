//! Port of upstream `publish.ts`: one FIFO queue for every observer notification (state events, shape pushes,
//! registry tables, listener invocations), so a write made from inside a notification cannot overtake the write
//! being published. [`hold_publications`] defers delivery until a multi-step change (a reload commit) is complete.

use std::collections::VecDeque;
use std::sync::{Mutex, MutexGuard, PoisonError};

type Task = Box<dyn FnOnce() + Send>;

struct Queue {
    tasks: VecDeque<Task>,
    held: usize,
    draining: bool,
}

/// Process-wide, like upstream's module state: a hold on one thread defers deliveries enqueued on any thread.
static QUEUE: Mutex<Queue> = Mutex::new(Queue { tasks: VecDeque::new(), held: 0, draining: false });

fn queue() -> MutexGuard<'static, Queue> {
    QUEUE.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Runs `task` after everything enqueued before it. Tasks must not panic (report errors instead).
pub fn enqueue(task: impl FnOnce() + Send + 'static) {
    queue().tasks.push_back(Box::new(task));
    drain();
}

/// Releases a hold, and delivers what waited unless the thread is unwinding.
struct Hold;

impl Drop for Hold {
    fn drop(&mut self) {
        queue().held -= 1;
        if !std::thread::panicking() {
            drain();
        }
    }
}

/// Delivers nothing until `f` returns, then everything it enqueued, in order.
pub fn hold_publications<T>(f: impl FnOnce() -> T) -> T {
    queue().held += 1;
    let _hold = Hold;
    f()
}

/// Clears `draining` when a task panics, so later tasks still run.
struct Draining {
    armed: bool,
}

impl Drop for Draining {
    fn drop(&mut self) {
        if self.armed {
            queue().draining = false;
        }
    }
}

fn drain() {
    {
        let mut state = queue();
        if state.draining || state.held > 0 {
            return;
        }
        state.draining = true;
    }
    let mut guard = Draining { armed: true };
    loop {
        let task = {
            let mut state = queue();
            match state.tasks.pop_front() {
                Some(task) => task,
                None => {
                    state.draining = false;
                    guard.armed = false;
                    return;
                }
            }
        };
        task();
    }
}
