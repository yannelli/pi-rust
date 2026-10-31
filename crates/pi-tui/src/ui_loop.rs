//! The per-thread UI loop: terminal events, cross-thread wakeups, timers, and render scheduling.
//!
//! See the crate docs for the threading model.

use std::any::Any;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use tokio::sync::Notify;

use crate::tui::TuiError;

/// A message posted from another thread and returned by the loop functions.
pub type LoopMessage = Box<dyn Any + Send>;

pub(crate) enum LoopEvent {
    Input { tui: u64, data: String },
    Resize { tui: u64 },
    RequestRender { force: bool },
    Call(Box<dyn FnOnce() + Send>),
    Message(LoopMessage),
    Wake,
}

/// `Send + Sync + Clone` handle that other threads use to reach the UI thread: request renders,
/// run closures on the UI thread, or post messages that [`run_once`] returns.
#[derive(Clone)]
pub struct TuiWaker {
    tx: Sender<LoopEvent>,
    notify: Arc<Notify>,
}

impl std::fmt::Debug for TuiWaker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TuiWaker")
    }
}

impl TuiWaker {
    pub(crate) fn send(&self, event: LoopEvent) {
        if self.tx.send(event).is_ok() {
            self.notify.notify_one();
        }
    }

    /// Requests a throttled render of every TUI on the UI thread.
    pub fn request_render(&self) {
        self.send(LoopEvent::RequestRender { force: false });
    }

    /// Requests a full redraw of every TUI on the UI thread.
    pub fn request_full_render(&self) {
        self.send(LoopEvent::RequestRender { force: true });
    }

    /// Runs `f` on the UI thread during the next loop iteration.
    pub fn post(&self, f: impl FnOnce() + Send + 'static) {
        self.send(LoopEvent::Call(Box::new(f)));
    }

    /// Posts a message; the UI thread receives it from [`run_once`] or [`run_pending`].
    pub fn send_message<M: Any + Send>(&self, message: M) {
        self.send(LoopEvent::Message(Box::new(message)));
    }

    /// Wakes a blocked loop without other effects.
    pub fn wake(&self) {
        self.send(LoopEvent::Wake);
    }
}

/// Where a terminal delivers input and resize events. `Send + Clone`, so input threads can own it.
#[derive(Clone, Debug)]
pub struct TerminalSink {
    waker: TuiWaker,
    tui: u64,
}

impl TerminalSink {
    pub(crate) fn new(waker: TuiWaker, tui: u64) -> Self {
        Self { waker, tui }
    }

    /// Delivers one input sequence to the TUI that started the terminal.
    pub fn input(&self, data: impl Into<String>) {
        self.waker.send(LoopEvent::Input { tui: self.tui, data: data.into() });
    }

    /// Notifies the TUI that the terminal size changed.
    pub fn resize(&self) {
        self.waker.send(LoopEvent::Resize { tui: self.tui });
    }

    /// The waker of the UI loop this sink belongs to.
    pub fn waker(&self) -> &TuiWaker {
        &self.waker
    }
}

/// Identifier of a timer created with [`set_timeout`] or [`set_interval`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TimerId(u64);

type TimerCallback = Rc<RefCell<dyn FnMut()>>;

struct Timer {
    deadline: Instant,
    interval: Option<Duration>,
    callback: TimerCallback,
}

/// Renderer hooks the loop drives. Implemented by the TUI.
pub(crate) trait LoopClient {
    fn id(&self) -> u64;
    fn handle_terminal_input(&self, data: String);
    fn handle_resize(&self);
    fn request_render_from_loop(&self, force: bool);
    fn render_deadline(&self) -> Option<Instant>;
    fn flush_render(&self, now: Instant) -> Result<(), TuiError>;
}

struct LoopState {
    tx: Sender<LoopEvent>,
    rx: Receiver<LoopEvent>,
    notify: Arc<Notify>,
    timers: RefCell<HashMap<u64, Timer>>,
    next_timer_id: Cell<u64>,
    clients: RefCell<Vec<Weak<dyn LoopClient>>>,
}

thread_local! {
    static LOOP: LoopState = {
        let (tx, rx) = mpsc::channel();
        LoopState {
            tx,
            rx,
            notify: Arc::new(Notify::new()),
            timers: RefCell::new(HashMap::new()),
            next_timer_id: Cell::new(1),
            clients: RefCell::new(Vec::new()),
        }
    };
}

/// Returns the waker of this thread's UI loop.
pub fn waker() -> TuiWaker {
    LOOP.with(|state| TuiWaker { tx: state.tx.clone(), notify: state.notify.clone() })
}

pub(crate) fn register_client(client: Weak<dyn LoopClient>) {
    LOOP.with(|state| {
        let mut clients = state.clients.borrow_mut();
        clients.retain(|c| c.strong_count() > 0);
        clients.push(client);
    });
}

fn live_clients() -> Vec<Rc<dyn LoopClient>> {
    LOOP.with(|state| {
        let mut clients = state.clients.borrow_mut();
        clients.retain(|c| c.strong_count() > 0);
        clients.iter().filter_map(Weak::upgrade).collect()
    })
}

fn add_timer(delay: Duration, interval: Option<Duration>, callback: TimerCallback) -> TimerId {
    LOOP.with(|state| {
        let id = state.next_timer_id.get();
        state.next_timer_id.set(id + 1);
        let deadline = Instant::now() + delay;
        state.timers.borrow_mut().insert(id, Timer { deadline, interval, callback });
        TimerId(id)
    })
}

/// Runs `callback` once after `delay` on this thread (upstream `setTimeout`).
pub fn set_timeout(delay: Duration, callback: impl FnOnce() + 'static) -> TimerId {
    let mut callback = Some(callback);
    add_timer(
        delay,
        None,
        Rc::new(RefCell::new(move || {
            if let Some(callback) = callback.take() {
                callback();
            }
        })),
    )
}

/// Runs `callback` every `period` on this thread until cleared (upstream `setInterval`).
pub fn set_interval(period: Duration, callback: impl FnMut() + 'static) -> TimerId {
    add_timer(period, Some(period), Rc::new(RefCell::new(callback)))
}

/// Cancels a timer. Unknown timers and an already-destroyed UI loop are ignored.
pub fn clear_timer(id: TimerId) {
    let removed = LOOP.try_with(|state| state.timers.borrow_mut().remove(&id.0));
    // Dropping a callback may cancel another timer; release the map borrow first.
    drop(removed);
}

/// Whether the timer is still scheduled.
pub fn timer_active(id: TimerId) -> bool {
    LOOP.with(|state| state.timers.borrow().contains_key(&id.0))
}

/// Earliest time the loop has work: a timer or a throttled render.
pub fn next_deadline() -> Option<Instant> {
    let timer = LOOP.with(|state| state.timers.borrow().values().map(|t| t.deadline).min());
    let render = live_clients().iter().filter_map(|c| c.render_deadline()).min();
    match (timer, render) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

fn run_due_timers(now: Instant) {
    let mut due: Vec<(Instant, u64)> = LOOP.with(|state| {
        state.timers.borrow().iter().filter(|(_, t)| t.deadline <= now).map(|(id, t)| (t.deadline, *id)).collect()
    });
    due.sort();
    for (_, id) in due {
        let callback = LOOP.with(|state| {
            let mut timers = state.timers.borrow_mut();
            let timer = timers.get_mut(&id)?;
            let callback = timer.callback.clone();
            match timer.interval {
                Some(period) => timer.deadline = now.max(timer.deadline + period),
                None => {
                    timers.remove(&id);
                }
            }
            Some(callback)
        });
        if let Some(callback) = callback {
            (callback.borrow_mut())();
        }
    }
}

fn dispatch(event: LoopEvent, messages: &mut Vec<LoopMessage>) {
    match event {
        LoopEvent::Input { tui, data } => {
            if let Some(client) = live_clients().into_iter().find(|c| c.id() == tui) {
                client.handle_terminal_input(data);
            }
        }
        LoopEvent::Resize { tui } => {
            if let Some(client) = live_clients().into_iter().find(|c| c.id() == tui) {
                client.handle_resize();
            }
        }
        LoopEvent::RequestRender { force } => {
            for client in live_clients() {
                client.request_render_from_loop(force);
            }
        }
        LoopEvent::Call(f) => f(),
        LoopEvent::Message(message) => messages.push(message),
        LoopEvent::Wake => {}
    }
}

/// Processes queued events, due timers, and pending renders without blocking. Returns the
/// messages posted with [`TuiWaker::send_message`].
pub fn run_pending() -> Result<Vec<LoopMessage>, TuiError> {
    let mut messages = Vec::new();
    loop {
        let event = LOOP.with(|state| state.rx.try_recv().ok());
        let Some(event) = event else { break };
        dispatch(event, &mut messages);
    }
    run_due_timers(Instant::now());
    let now = Instant::now();
    for client in live_clients() {
        client.flush_render(now)?;
    }
    Ok(messages)
}

/// Blocks until an event arrives, the next deadline passes, or `timeout` elapses, then runs
/// [`run_pending`]. `None` waits without a timeout.
pub fn run_once(timeout: Option<Duration>) -> Result<Vec<LoopMessage>, TuiError> {
    let now = Instant::now();
    let mut wait_until = timeout.map(|t| now + t);
    if let Some(deadline) = next_deadline() {
        wait_until = Some(wait_until.map_or(deadline, |w| w.min(deadline)));
    }
    let event = LOOP.with(|state| match wait_until {
        Some(until) => state.rx.recv_timeout(until.saturating_duration_since(now)).ok(),
        None => state.rx.recv().ok(),
    });
    let mut messages = Vec::new();
    if let Some(event) = event {
        dispatch(event, &mut messages);
    }
    messages.extend(run_pending()?);
    Ok(messages)
}

/// Async variant of [`run_once`] for a current-thread runtime or `LocalSet` on the UI thread.
pub async fn run_once_async() -> Result<Vec<LoopMessage>, TuiError> {
    // Each send stores a wakeup in the Notify, so events queued before this call wake it at once.
    let notify = LOOP.with(|state| state.notify.clone());
    match next_deadline() {
        Some(deadline) => {
            let _ = tokio::time::timeout_at(deadline.into(), notify.notified()).await;
        }
        None => notify.notified().await,
    }
    run_pending()
}

/// Runs the loop until `duration` has passed. Useful in tests that wait for timers.
pub fn run_for(duration: Duration) -> Result<Vec<LoopMessage>, TuiError> {
    let end = Instant::now() + duration;
    let mut messages = Vec::new();
    loop {
        let now = Instant::now();
        if now >= end {
            break;
        }
        messages.extend(run_once(Some(end - now))?);
    }
    messages.extend(run_pending()?);
    Ok(messages)
}
