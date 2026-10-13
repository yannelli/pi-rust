//! A small executor for autocomplete futures on the UI thread. Wakers post a poll to the UI loop
//! through [`crate::ui_loop::TuiWaker`], so a provider may finish its work on any thread.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::future::{Future, poll_fn};
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll, Wake, Waker};

use crate::ui_loop::{self, TuiWaker};

type LocalTask = Pin<Box<dyn Future<Output = ()>>>;

struct TaskWaker {
    id: u64,
    ui: TuiWaker,
    scheduled: AtomicBool,
}

impl Wake for TaskWaker {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        if !self.scheduled.swap(true, Ordering::SeqCst) {
            let id = self.id;
            self.ui.post(move || poll_task(id));
        }
    }
}

struct TaskSlot {
    future: Option<LocalTask>,
    waker: Arc<TaskWaker>,
}

thread_local! {
    static TASKS: RefCell<HashMap<u64, TaskSlot>> = RefCell::new(HashMap::new());
    static NEXT_ID: Cell<u64> = const { Cell::new(1) };
}

/// Runs `future` on this thread's UI loop. The first poll happens on the next loop pass, like a
/// JavaScript microtask after the current handler returns.
pub(super) fn spawn_local(future: impl Future<Output = ()> + 'static) {
    let id = NEXT_ID.with(|next| {
        let id = next.get();
        next.set(id + 1);
        id
    });
    let waker = Arc::new(TaskWaker { id, ui: ui_loop::waker(), scheduled: AtomicBool::new(false) });
    TASKS.with(|tasks| {
        tasks.borrow_mut().insert(id, TaskSlot { future: Some(Box::pin(future)), waker: Arc::clone(&waker) });
    });
    waker.wake_by_ref();
}

fn poll_task(id: u64) {
    let taken = TASKS.with(|tasks| {
        let mut tasks = tasks.borrow_mut();
        let slot = tasks.get_mut(&id)?;
        slot.waker.scheduled.store(false, Ordering::SeqCst);
        slot.future.take().map(|future| (future, Arc::clone(&slot.waker)))
    });
    let Some((mut future, waker)) = taken else { return };
    let waker = Waker::from(waker);
    let mut context = Context::from_waker(&waker);
    let done = future.as_mut().poll(&mut context).is_ready();
    TASKS.with(|tasks| {
        let mut tasks = tasks.borrow_mut();
        if done {
            tasks.remove(&id);
        } else if let Some(slot) = tasks.get_mut(&id) {
            slot.future = Some(future);
        }
    });
}

#[derive(Default)]
struct LatchState {
    done: bool,
    wakers: Vec<Waker>,
}

/// A one-shot completion signal between autocomplete requests.
#[derive(Clone, Default)]
pub(crate) struct Latch(Rc<RefCell<LatchState>>);

impl Latch {
    pub fn set(&self) {
        let wakers = {
            let mut state = self.0.borrow_mut();
            state.done = true;
            std::mem::take(&mut state.wakers)
        };
        for waker in wakers {
            waker.wake();
        }
    }

    pub async fn wait(&self) {
        poll_fn(|cx| {
            let mut state = self.0.borrow_mut();
            if state.done {
                Poll::Ready(())
            } else {
                state.wakers.push(cx.waker().clone());
                Poll::Pending
            }
        })
        .await;
    }
}
