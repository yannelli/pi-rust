//! Time sources of a recorder (upstream `Date.now` and `performance.now`), so tests can fix span and interval times.

use std::sync::{Arc, LazyLock};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

type MillisFn = Arc<dyn Fn() -> f64 + Send + Sync>;

/// Time sources of a recorder: wall-clock epoch milliseconds for span and interval starts, and a monotonic
/// millisecond counter for durations and event offsets.
#[derive(Clone)]
pub struct TelemetryClock {
    wall_ms: MillisFn,
    monotonic_ms: MillisFn,
}

impl TelemetryClock {
    pub fn new(
        wall_ms: impl Fn() -> f64 + Send + Sync + 'static,
        monotonic_ms: impl Fn() -> f64 + Send + Sync + 'static,
    ) -> Self {
        Self { wall_ms: Arc::new(wall_ms), monotonic_ms: Arc::new(monotonic_ms) }
    }

    /// The current wall-clock time in epoch milliseconds.
    pub fn wall_ms(&self) -> f64 {
        (self.wall_ms)()
    }

    /// The current monotonic time in milliseconds.
    pub fn monotonic_ms(&self) -> f64 {
        (self.monotonic_ms)()
    }
}

impl Default for TelemetryClock {
    fn default() -> Self {
        static START: LazyLock<Instant> = LazyLock::new(Instant::now);
        let wall = || SystemTime::now().duration_since(UNIX_EPOCH).map_or(0.0, |elapsed| elapsed.as_millis() as f64);
        Self::new(wall, || START.elapsed().as_secs_f64() * 1000.0)
    }
}
