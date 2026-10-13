//! Startup timing instrumentation, on with `PI_TIMING=1` (upstream `core/timings.ts`).

use std::sync::{LazyLock, Mutex};
use std::time::Instant;

pub const MAIN: &str = "main";
pub const EXTENSIONS: &str = "extensions";

#[derive(Debug)]
struct Namespace {
    name: String,
    timings: Vec<(String, u128)>,
    last: Instant,
}

/// Timings per namespace; disabled timers record nothing.
#[derive(Debug)]
pub struct Timer {
    enabled: bool,
    namespaces: Mutex<Vec<Namespace>>,
}

impl Timer {
    pub fn new(enabled: bool) -> Self {
        Self { enabled, namespaces: Mutex::new(Vec::new()) }
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Start `namespace` over from now.
    pub fn reset(&self, namespace: &str) {
        if !self.enabled {
            return;
        }
        let mut namespaces = self.namespaces.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let fresh = Namespace { name: namespace.into(), timings: Vec::new(), last: Instant::now() };
        match namespaces.iter_mut().find(|entry| entry.name == namespace) {
            Some(entry) => *entry = fresh,
            None => namespaces.push(fresh),
        }
    }

    /// Record the time since the last mark of `namespace` under `label`.
    pub fn time(&self, label: &str, namespace: &str) {
        if !self.enabled {
            return;
        }
        let now = Instant::now();
        let mut namespaces = self.namespaces.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if !namespaces.iter().any(|entry| entry.name == namespace) {
            namespaces.push(Namespace { name: namespace.into(), timings: Vec::new(), last: now });
        }
        let entry = namespaces.iter_mut().find(|entry| entry.name == namespace).expect("namespace exists");
        entry.timings.push((label.into(), now.duration_since(entry.last).as_millis()));
        entry.last = now;
    }

    /// The report printed to stderr: one group per namespace with a total.
    pub fn format(&self) -> String {
        if !self.enabled {
            return String::new();
        }
        let namespaces = self.namespaces.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut output = String::new();
        for namespace in namespaces.iter().filter(|namespace| !namespace.timings.is_empty()) {
            let title = format!("Startup Timings: {}", namespace.name);
            output.push_str(&format!("\n--- {title} ---\n"));
            for (label, ms) in &namespace.timings {
                output.push_str(&format!("  {label}: {ms}ms\n"));
            }
            let total: u128 = namespace.timings.iter().map(|(_, ms)| ms).sum();
            output.push_str(&format!("  TOTAL: {total}ms\n{}\n\n", "-".repeat(title.len() + 8)));
        }
        output
    }
}

static TIMER: LazyLock<Timer> =
    LazyLock::new(|| Timer::new(pi_ai::env::process_env_value("PI_TIMING").as_deref() == Some("1")));

/// The process timer, enabled by `PI_TIMING=1`.
pub fn timer() -> &'static Timer {
    &TIMER
}

pub fn reset_timings(namespace: &str) {
    TIMER.reset(namespace);
}

pub fn time(label: &str) {
    TIMER.time(label, MAIN);
}

pub fn time_in(label: &str, namespace: &str) {
    TIMER.time(label, namespace);
}

/// Print the collected timings to stderr when enabled.
pub fn print_timings() {
    let report = TIMER.format();
    if !report.is_empty() {
        eprint!("{report}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_labels_per_namespace_with_totals() {
        let timer = Timer::new(true);
        timer.reset(MAIN);
        timer.time("args", MAIN);
        timer.time("load", EXTENSIONS);
        timer.time("settings", MAIN);
        let report = timer.format();
        assert!(report.starts_with("\n--- Startup Timings: main ---\n  args: "), "{report}");
        assert!(report.contains("  settings: "));
        assert!(report.contains("\n--- Startup Timings: extensions ---\n  load: 0ms\n  TOTAL: 0ms\n"));
        assert!(report.contains(&format!("\n{}\n", "-".repeat("Startup Timings: main".len() + 8))));

        let disabled = Timer::new(false);
        disabled.time("x", MAIN);
        assert!(!disabled.enabled());
        assert_eq!(disabled.format(), "");
    }
}
