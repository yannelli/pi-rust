use std::sync::Mutex;
use std::time::Duration;

use tokio::sync::watch;
use tokio::time::{Instant, sleep_until};

/// Node checks c-ares timeouts on socket activity and on a repeating channel timer.
pub(super) struct Timer {
    interval: Duration,
    active: Mutex<(usize, Instant)>,
    activity: watch::Sender<Instant>,
}

impl Timer {
    pub(super) fn new(timeout: Duration) -> Self {
        let now = Instant::now();
        let interval = if timeout.is_zero() { Duration::from_secs(1) } else { timeout.min(Duration::from_secs(1)) };
        Self { interval, active: Mutex::new((0, now)), activity: watch::channel(now).0 }
    }

    pub(super) fn enter(&self) -> Running<'_> {
        let mut active = self.active.lock().expect("DNS timer lock");
        if active.0 == 0 {
            active.1 = Instant::now();
        }
        active.0 += 1;
        Running { timer: self, origin: active.1 }
    }
}

pub(super) struct Running<'a> {
    timer: &'a Timer,
    origin: Instant,
}

impl Running<'_> {
    pub(super) fn poll_time(&self, deadline: Instant) -> Instant {
        let elapsed = deadline.duration_since(self.origin);
        let interval = self.timer.interval.as_nanos();
        let remainder = elapsed.as_nanos() % interval;
        let padding = if remainder == 0 { 0 } else { (interval - remainder) as u64 };
        deadline + Duration::from_nanos(padding)
    }

    pub(super) fn activity(&self) {
        self.timer.activity.send_replace(Instant::now());
    }

    pub(super) async fn expired(&self, deadline: Instant) {
        sleep_until(deadline).await;
        let mut activity = self.timer.activity.subscribe();
        tokio::select! {
            () = sleep_until(self.poll_time(deadline)) => {},
            _ = activity.wait_for(|last| *last >= deadline) => {},
        }
    }
}

impl Drop for Running<'_> {
    fn drop(&mut self) {
        self.timer.active.lock().expect("DNS timer lock").0 -= 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_shorter_measured_deadline_waits_for_the_channels_next_tick() {
        let timer = Timer::new(Duration::from_secs(3));
        let running = timer.enter();
        for (deadline, tick) in [(250, 1000), (1000, 1000), (1001, 2000)] {
            assert_eq!(
                running.poll_time(running.origin + Duration::from_millis(deadline)),
                running.origin + Duration::from_millis(tick)
            );
        }
        let overlapping = timer.enter();
        assert_eq!(running.origin, overlapping.origin);
    }
}
