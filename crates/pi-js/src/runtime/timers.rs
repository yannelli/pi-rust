//! Timer queue for `setTimeout` and `setInterval`, ordered by deadline then creation.

use std::collections::{BTreeSet, HashMap};
use std::time::{Duration, Instant};

use rquickjs::{Function, Persistent};

struct Timer {
    callback: Persistent<Function<'static>>,
    deadline: Instant,
    delay: Duration,
    repeat: bool,
    referenced: bool,
}

#[derive(Default)]
pub(crate) struct Timers {
    queue: BTreeSet<(Instant, u64)>,
    entries: HashMap<u64, Timer>,
}

impl Timers {
    pub fn insert(&mut self, id: u64, callback: Persistent<Function<'static>>, delay: Duration, repeat: bool) {
        let deadline = Instant::now() + delay;
        self.queue.insert((deadline, id));
        self.entries.insert(id, Timer { callback, deadline, delay, repeat, referenced: true });
    }

    pub fn remove(&mut self, id: u64) {
        if let Some(timer) = self.entries.remove(&id) {
            self.queue.remove(&(timer.deadline, id));
        }
    }

    pub fn set_referenced(&mut self, id: u64, referenced: bool) {
        if let Some(timer) = self.entries.get_mut(&id) {
            timer.referenced = referenced;
        }
    }

    pub fn is_referenced(&self, id: u64) -> bool {
        self.entries.get(&id).is_some_and(|timer| timer.referenced)
    }

    /// Restart the timer's delay from now (`timeout.refresh()`).
    pub fn refresh(&mut self, id: u64) {
        if let Some(timer) = self.entries.get_mut(&id) {
            self.queue.remove(&(timer.deadline, id));
            timer.deadline = Instant::now() + timer.delay;
            self.queue.insert((timer.deadline, id));
        }
    }

    pub fn next_deadline(&self) -> Option<Instant> {
        self.queue.first().map(|(deadline, _)| *deadline)
    }

    pub fn has_referenced(&self) -> bool {
        self.entries.values().any(|timer| timer.referenced)
    }

    /// Take the next timer due at `now`. Intervals are re-armed; timeouts are removed.
    pub fn pop_due(&mut self, now: Instant) -> Option<Persistent<Function<'static>>> {
        let (deadline, id) = *self.queue.first()?;
        if deadline > now {
            return None;
        }
        self.queue.remove(&(deadline, id));
        let timer = self.entries.get_mut(&id)?;
        let callback = timer.callback.clone();
        if timer.repeat {
            timer.deadline = now + timer.delay.max(Duration::from_millis(1));
            self.queue.insert((timer.deadline, id));
        } else {
            self.entries.remove(&id);
        }
        Some(callback)
    }

    pub fn clear(&mut self) {
        self.queue.clear();
        self.entries.clear();
    }
}
