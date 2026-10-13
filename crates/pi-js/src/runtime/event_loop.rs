//! The JavaScript thread: runtime setup, the event loop, and teardown.

use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, RecvTimeoutError, TryRecvError};
use std::time::Instant;

use rquickjs::{Context, Ctx, Function, Persistent, Runtime, Value};

use super::state::{LoopState, Message, state};
use super::{UncaughtKind, report_error, report_value};
use crate::error::JsError;

pub(crate) struct LoopSetup {
    pub memory_limit: Option<usize>,
    pub max_stack_size: usize,
}

/// Create the QuickJS runtime and context; the caller runs the bootstrap.
pub(crate) fn create(setup: &LoopSetup, state: &LoopState) -> Result<(Runtime, Context), JsError> {
    let runtime =
        Runtime::new().map_err(|error| JsError::new(format!("Failed to create the QuickJS runtime: {error}")))?;
    if let Some(limit) = setup.memory_limit {
        runtime.set_memory_limit(limit);
    }
    runtime.set_max_stack_size(setup.max_stack_size);
    let flag = state.shared.interrupt.clone();
    runtime.set_interrupt_handler(Some(Box::new(move || flag.load(Ordering::Relaxed))));
    runtime.set_host_promise_rejection_tracker(Some(Box::new(track_rejection)));
    crate::modules::install_loader(&runtime);
    let context = Context::full(&runtime)
        .map_err(|error| JsError::new(format!("Failed to create the QuickJS context: {error}")))?;
    Ok((runtime, context))
}

fn track_rejection<'js>(ctx: Ctx<'js>, promise: Value<'js>, reason: Value<'js>, handled: bool) {
    let state = state();
    let mut rejections = state.rejections.borrow_mut();
    if handled {
        rejections.retain(|(stored, _)| stored.clone().restore(&ctx).map(|value| value != promise).unwrap_or(false));
    } else {
        rejections.push((Persistent::save(&ctx, promise), Persistent::save(&ctx, reason)));
    }
}

/// Run until shutdown: jobs, due timers, immediates, then messages, sleeping until the next timer.
pub(crate) fn run(runtime: &Runtime, context: &Context, receiver: &Receiver<Message>, state: &LoopState) {
    loop {
        state.shared.interrupt.store(false, Ordering::SeqCst);
        drain_jobs(runtime, context, state);
        run_timers(runtime, context, state);
        run_immediates(runtime, context, state);
        loop {
            match receiver.try_recv() {
                Ok(message) => handle(message, runtime, context, state),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => state.shutting_down.set(true),
            }
            if state.shutting_down.get() {
                break;
            }
        }
        if state.shutting_down.get() {
            break;
        }
        if is_idle(runtime, state) {
            for waiter in state.idle_waiters.borrow_mut().drain(..) {
                let _ = waiter.send(());
            }
        }
        if runtime.is_job_pending() || !state.immediates.borrow().is_empty() {
            continue;
        }
        let deadline = state.timers.borrow().next_deadline();
        let message = match deadline {
            Some(deadline) => match receiver.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(message) => message,
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => break,
            },
            None => match receiver.recv() {
                Ok(message) => message,
                Err(_) => break,
            },
        };
        handle(message, runtime, context, state);
    }
}

fn is_idle(runtime: &Runtime, state: &LoopState) -> bool {
    !runtime.is_job_pending()
        && state.busy.get() == 0
        && state.immediates.borrow().is_empty()
        && !state.timers.borrow().has_referenced()
}

fn handle(message: Message, runtime: &Runtime, context: &Context, state: &LoopState) {
    match message {
        Message::Task(task) => {
            context.with(|ctx| task(&ctx));
            drain_jobs(runtime, context, state);
        }
        Message::Release(id) => {
            state.refs.borrow_mut().remove(&id);
        }
        Message::Idle(waiter) => state.idle_waiters.borrow_mut().push(waiter),
        Message::Shutdown => state.shutting_down.set(true),
    }
}

/// Run queued `nextTick` callbacks, then every queued job, repeating while ticks were added,
/// as Node's `processTicksAndRejections` does; then report rejections nobody handled.
pub(crate) fn drain_jobs(runtime: &Runtime, context: &Context, state: &LoopState) {
    loop {
        if state.ticks_pending.replace(false) {
            run_ticks(context, state);
        }
        loop {
            match runtime.execute_pending_job() {
                Ok(true) => {}
                Ok(false) => break,
                Err(exception) => exception.0.with(|ctx| {
                    let value = ctx.catch();
                    report_value(&ctx, UncaughtKind::Exception, value);
                }),
            }
        }
        if !state.ticks_pending.get() {
            break;
        }
    }
    let rejections = std::mem::take(&mut *state.rejections.borrow_mut());
    if rejections.is_empty() {
        return;
    }
    context.with(|ctx| {
        for (_, reason) in rejections {
            if let Ok(reason) = reason.restore(&ctx) {
                report_value(&ctx, UncaughtKind::Rejection, reason);
            }
        }
    });
    if runtime.is_job_pending() {
        drain_jobs(runtime, context, state);
    }
}

fn run_ticks(context: &Context, state: &LoopState) {
    context.with(|ctx| {
        let result = state
            .internal(&ctx)
            .and_then(|internal| internal.get::<_, Function>("runTicks"))
            .and_then(|run| run.call::<_, ()>(()));
        if let Err(error) = result {
            report_error(&ctx, UncaughtKind::Exception, error);
        }
    });
}

fn run_timers(runtime: &Runtime, context: &Context, state: &LoopState) {
    let now = Instant::now();
    loop {
        let Some(callback) = state.timers.borrow_mut().pop_due(now) else { break };
        call_callback(context, callback);
        drain_jobs(runtime, context, state);
        if state.shutting_down.get() {
            return;
        }
    }
}

fn run_immediates(runtime: &Runtime, context: &Context, state: &LoopState) {
    let batch: Vec<_> = state.immediates.borrow_mut().drain(..).collect();
    for (_, callback) in batch {
        call_callback(context, callback);
        drain_jobs(runtime, context, state);
        if state.shutting_down.get() {
            return;
        }
    }
}

fn call_callback(context: &Context, callback: Persistent<Function<'static>>) {
    context.with(|ctx| {
        let result = callback.restore(&ctx).and_then(|callback| callback.call::<_, ()>(()));
        if let Err(error) = result {
            report_error(&ctx, UncaughtKind::Exception, error);
        }
    });
}
