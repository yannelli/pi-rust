use std::net::SocketAddr;
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

const WINDOWS: [u64; 4] = [60, 900, 3_600, 86_400];
const RETRY_DELAY: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Default)]
struct Samples {
    period: u64,
    total_ms: u64,
    count: u64,
}

impl Samples {
    fn add(&mut self, ms: u64) {
        self.total_ms = self.total_ms.saturating_add(ms);
        self.count = self.count.saturating_add(1);
    }

    fn average(&self) -> Option<Duration> {
        (self.count >= 3).then(|| Duration::from_millis((self.total_ms / self.count).saturating_mul(5)))
    }
}

#[derive(Default)]
struct Window {
    current: Samples,
    previous: Samples,
}

impl Window {
    fn add(&mut self, period: u64, ms: u64) {
        if self.current.period != period {
            self.previous = std::mem::replace(&mut self.current, Samples { period, ..Samples::default() });
        }
        self.current.add(ms);
    }

    fn average(&self, period: u64) -> Option<Duration> {
        if self.current.period == period
            && let Some(value) = self.current.average()
        {
            return Some(value);
        }
        let previous = period.checked_sub(1)?;
        [self.current, self.previous].iter().find(|sample| sample.period == previous)?.average()
    }
}

#[derive(Default)]
struct Health {
    failures: u8,
    failed_at: Option<Instant>,
    probing: bool,
    windows: [Window; 4],
    lifetime: Samples,
}

fn clock_seconds() -> u64 {
    static ORIGIN: OnceLock<Instant> = OnceLock::new();
    ORIGIN.get_or_init(Instant::now).elapsed().as_secs()
}

pub(super) struct Server {
    pub(super) address: SocketAddr,
    pub(super) tcp: super::tcp::TcpPool,
    health: Mutex<Health>,
}

impl Server {
    pub(super) fn new(address: SocketAddr) -> Self {
        Self { address, tcp: super::tcp::TcpPool::default(), health: Mutex::default() }
    }

    fn state(&self) -> MutexGuard<'_, Health> {
        self.health.lock().expect("DNS server health lock")
    }

    pub(super) fn priority(&self) -> (u8, Option<Instant>) {
        let state = self.state();
        (state.failures, state.failed_at)
    }

    pub(super) fn timeout(&self, initial: Duration) -> Duration {
        let state = self.state();
        if state.lifetime.count < 3 {
            return initial;
        }
        let now = clock_seconds();
        state
            .windows
            .iter()
            .zip(WINDOWS)
            .find_map(|(window, width)| window.average(now / width))
            .or_else(|| state.lifetime.average())
            .unwrap_or(initial)
    }

    pub(super) fn failed(&self) {
        let mut state = self.state();
        state.failures = (state.failures + 1).min(16);
        state.failed_at = Some(Instant::now());
    }

    pub(super) fn responded(&self, latency: Option<Duration>) {
        let mut state = self.state();
        state.failures = 0;
        state.failed_at = None;
        if let Some(latency) = latency {
            let ms = u64::try_from(latency.as_millis()).unwrap_or(u64::MAX).max(1);
            let now = clock_seconds();
            for (window, width) in state.windows.iter_mut().zip(WINDOWS) {
                window.add(now / width, ms);
            }
            state.lifetime.add(ms);
        }
    }

    pub(super) fn begin_probe(&self) -> bool {
        let mut state = self.state();
        if state.probing || !state.failed_at.is_some_and(|at| at.elapsed() >= RETRY_DELAY) {
            return false;
        }
        state.probing = true;
        true
    }

    pub(super) fn probe_priority(&self) -> Option<(u8, Instant)> {
        let state = self.state();
        let failed_at = state.failed_at?;
        (!state.probing && failed_at.elapsed() >= RETRY_DELAY).then_some((state.failures, failed_at))
    }

    pub(super) fn end_probe(&self) {
        self.state().probing = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn average_needs_three_replies_and_prefers_a_recent_complete_period() {
        let mut window = Window::default();
        for ms in [20, 40, 60] {
            window.add(5, ms);
        }
        assert_eq!(window.average(6), Some(Duration::from_millis(200)));
        window.add(6, 300);
        assert_eq!(window.average(6), Some(Duration::from_millis(200)));
        assert_eq!(window.average(7), None);
        window.add(6, 100);
        window.add(6, 200);
        assert_eq!(window.average(6), Some(Duration::from_millis(1000)));
    }

    #[test]
    fn an_overdue_server_can_have_only_one_probe_until_it_recovers() {
        let server = Server::new(([127, 0, 0, 1], 53).into());
        server.failed();
        assert!(server.probe_priority().is_none());
        server.state().failed_at = Some(Instant::now() - RETRY_DELAY);
        assert!(server.probe_priority().is_some());
        assert!(server.begin_probe());
        assert!(!server.begin_probe());
        server.end_probe();
        assert!(server.begin_probe());
        server.responded(Some(Duration::from_millis(4)));
        server.end_probe();
        assert!(server.probe_priority().is_none());
    }
}
