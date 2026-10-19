//! Host timers (upstream `timers.ts`): every timer of the Harness and its tasks is armed through
//! `HarnessOptions::set_timer`, or a Tokio timer by default.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::errors::DurableError;

/// What a timer runs when it fires; called at most once.
pub type TimerCallback = Box<dyn FnOnce() + Send>;
/// Disarms a timer; a timer that already fired is unaffected.
pub type Disarm = Box<dyn FnOnce() + Send>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TimerOptions {
    /// The timer only frees memory, and the host may decline it.
    pub background: bool,
}

/// A host timer, given as `HarnessOptions::set_timer`: call the callback once, about `delay_ms` from now, and return
/// what disarms it. `background` timers only free memory, and a host may decline one by returning `None`, for example
/// where a pending timer keeps the process from hibernating; the Harness then frees that memory at later commits.
/// Other timers pace or resume work: one the host declines is armed with a Tokio timer. A timer should not fire
/// early; callers check their clock again when it fires.
pub type SetTimer = Arc<dyn Fn(TimerCallback, f64, TimerOptions) -> Option<Disarm> + Send + Sync>;

/// The default `HarnessOptions::set_timer`: a Tokio timer. A Tokio timer never keeps the process alive, so no
/// background timer is declined.
pub fn native_timer(callback: TimerCallback, delay_ms: f64, _options: TimerOptions) -> Option<Disarm> {
    let delay = Duration::try_from_secs_f64(delay_ms.max(0.0) / 1000.0).unwrap_or(Duration::MAX);
    let handle = tokio::spawn(async move {
        tokio::time::sleep(delay).await;
        callback();
    });
    Some(Box::new(move || handle.abort()))
}

/// Timers the Harness arms through the host's `set_timer`.
#[derive(Clone)]
pub(crate) struct Timers {
    set_timer: Option<SetTimer>,
    report: Arc<dyn Fn(&DurableError) + Send + Sync>,
    reported: Arc<AtomicBool>,
}

impl Timers {
    pub fn new(set_timer: Option<SetTimer>, report: Arc<dyn Fn(&DurableError) + Send + Sync>) -> Self {
        Self { set_timer, report, reported: Arc::default() }
    }

    /// A timer that paces or resumes work. It must fire, or work stalls: one the host declines is armed with a Tokio
    /// timer, and the first such decline is reported.
    pub fn foreground(&self, callback: TimerCallback, delay_ms: f64) -> Disarm {
        let Some(set_timer) = &self.set_timer else {
            return native_timer(callback, delay_ms, TimerOptions { background: false }).unwrap_or_else(noop);
        };
        let slot = Arc::new(Mutex::new(Some(callback)));
        let host = slot.clone();
        let fire: TimerCallback = Box::new(move || {
            if let Some(callback) = take(&host) {
                callback();
            }
        });
        if let Some(disarm) = set_timer(fire, delay_ms, TimerOptions { background: false }) {
            return disarm;
        }
        if !self.reported.swap(true, Ordering::SeqCst) {
            let message = "HarnessOptions::set_timer declined a timer that paces work; using a Tokio timer";
            (self.report)(&DurableError::error(message));
        }
        let Some(callback) = take(&slot) else { return noop() };
        native_timer(callback, delay_ms, TimerOptions { background: false }).unwrap_or_else(noop)
    }

    /// A timer that only frees memory; `None` when the host declines it.
    pub fn background(&self, callback: TimerCallback, delay_ms: f64) -> Option<Disarm> {
        match &self.set_timer {
            Some(set_timer) => set_timer(callback, delay_ms, TimerOptions { background: true }),
            None => native_timer(callback, delay_ms, TimerOptions { background: true }),
        }
    }
}

fn take(slot: &Mutex<Option<TimerCallback>>) -> Option<TimerCallback> {
    slot.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take()
}

fn noop() -> Disarm {
    Box::new(|| {})
}
