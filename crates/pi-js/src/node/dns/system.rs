//! Process-wide DNS settings and hosts for synchronous JavaScript resolver calls.

use std::sync::{Arc, Condvar, LazyLock, Mutex, Once};
use std::time::{Duration, Instant};

use super::{config, hosts};

const REFRESH_AFTER: Duration = Duration::from_secs(5);
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Default)]
struct State<T> {
    snapshot: Arc<T>,
    checked: Option<Instant>,
    loading_since: Option<Instant>,
}

#[derive(Default)]
struct Cache<T> {
    state: Mutex<State<T>>,
    ready: Condvar,
    empty: Arc<T>,
}

impl<T: Default + Send + Sync + 'static> Cache<T> {
    fn read(
        self: &Arc<Self>,
        now: Instant,
        load: impl FnOnce() -> T + Send + std::panic::UnwindSafe + 'static,
    ) -> Arc<T> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(started) = state.loading_since {
            return if now.saturating_duration_since(started) >= DISCOVERY_TIMEOUT {
                Arc::clone(&self.empty)
            } else {
                Arc::clone(&state.snapshot)
            };
        }
        let snapshot = Arc::clone(&state.snapshot);
        if state.checked.is_some_and(|last| now.saturating_duration_since(last) < REFRESH_AFTER) {
            return snapshot;
        }
        state.loading_since = Some(now);
        drop(state);

        let cache = Arc::clone(self);
        let discover = move || cache.finish(std::panic::catch_unwind(load).unwrap_or_default());
        if std::thread::Builder::new().name("pi-dns-config".into()).spawn(discover).is_err() {
            self.finish(T::default());
            return Arc::clone(&self.empty);
        }
        snapshot
    }

    fn finish(&self, result: T) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let previous = std::mem::replace(&mut state.snapshot, Arc::new(result));
        state.checked = Some(Instant::now());
        state.loading_since = None;
        self.ready.notify_all();
        drop(state);
        drop(previous);
    }

    fn wait_until(&self, deadline: Instant) {
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        drop(self.ready.wait_timeout_while(state, deadline.saturating_duration_since(Instant::now()), |s| {
            s.loading_since.is_some()
        }));
    }
}

static CONFIG: LazyLock<Arc<Cache<config::Config>>> = LazyLock::new(Arc::default);
static HOSTS: LazyLock<Arc<Cache<hosts::Hosts>>> = LazyLock::new(Arc::default);

/// The first QuickJS runtime waits during startup so getServers() can answer synchronously.
pub(crate) fn prime() {
    static START: Once = Once::new();
    START.call_once(|| {
        let deadline = Instant::now() + DISCOVERY_TIMEOUT;
        settings();
        hosts();
        CONFIG.wait_until(deadline);
        HOSTS.wait_until(deadline);
    });
}

pub(super) fn settings() -> Arc<config::Config> {
    CONFIG.read(Instant::now(), config::system)
}

pub(super) fn hosts() -> Arc<hosts::Hosts> {
    HOSTS.read(Instant::now(), hosts::system)
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use super::*;

    #[test]
    fn callers_share_one_refresh_and_can_read_while_discovery_is_blocked() {
        let cache = Arc::new(Cache::default());
        let mut initial = config::Config::default();
        initial.servers.push("127.0.0.1:53".parse().unwrap());
        cache.finish(initial);

        let (started, worker) = mpsc::sync_channel(0);
        let (release, resume) = mpsc::sync_channel(0);
        let expired = Instant::now() + REFRESH_AFTER;
        let previous = cache.read(expired, move || {
            started.send(()).unwrap();
            resume.recv_timeout(DISCOVERY_TIMEOUT).unwrap();
            config::Config::default()
        });
        worker.recv_timeout(DISCOVERY_TIMEOUT).unwrap();
        let during = cache.read(expired, || panic!("duplicate discovery"));
        assert!(Arc::ptr_eq(&previous, &during));
        assert_eq!(during.servers.len(), 1);
        let stalled = cache.read(expired + DISCOVERY_TIMEOUT, || panic!("overlapping discovery"));
        assert!(stalled.servers.is_empty());

        release.send(()).unwrap();
        cache.wait_until(Instant::now() + DISCOVERY_TIMEOUT);
        let after = cache.read(Instant::now(), || panic!("fresh snapshot was discarded"));
        assert!(after.servers.is_empty());
        assert!(!Arc::ptr_eq(&after, &stalled));
    }
}
