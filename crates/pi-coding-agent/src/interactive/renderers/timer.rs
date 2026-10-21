//! The `Elapsed` and `Took` line of tool rows (upstream `startToolTimer` and `toolTimerText` in
//! `render-utils.ts`), shared by shell and codemode rows.

use std::time::{Duration, Instant};

use pi_tui::ui_loop::{self, TimerId};

use super::ToolRenderContext;

/// `1.5s`, `2m 5s`, or `1h 2m 5s`.
pub fn format_tool_duration(ms: u64) -> String {
    let seconds = ms as f64 / 1000.0;
    if seconds < 60.0 {
        return format!("{seconds:.1}s");
    }
    let total = seconds.floor() as u64;
    let (minutes, remainder) = (total / 60, total % 60);
    if minutes < 60 {
        return format!("{minutes}m {remainder}s");
    }
    format!("{}h {}m {remainder}s", minutes / 60, minutes % 60)
}

/// The row's clock, kept in the renderer state.
#[derive(Default)]
pub(super) struct ToolTimerState {
    started_at: Option<Instant>,
    ended_at: Option<Instant>,
    interval: Option<TimerId>,
}

impl Drop for ToolTimerState {
    fn drop(&mut self) {
        if let Some(interval) = self.interval.take() {
            ui_loop::clear_timer(interval);
        }
    }
}

/// Start the row's clock once execution starts. Call from `render_call`.
pub(super) fn start_tool_timer(context: &mut ToolRenderContext<'_>) {
    let started = context.execution_started;
    let state = context.state.get_or_default::<ToolTimerState>();
    if started && state.started_at.is_none() {
        state.started_at = Some(Instant::now());
        state.ended_at = None;
    }
}

/// The row's timing line, from `render_result`: `Elapsed 4.2s` while the result is partial,
/// redrawn every second, and `Took 4.2s` once it is final. `None` when the row has no start
/// time and no recorded duration.
pub(super) fn tool_timer_text(context: &mut ToolRenderContext<'_>) -> Option<String> {
    let invalidate = context.invalidate.clone();
    let (is_partial, is_error, duration_ms) = (context.is_partial, context.is_error, context.duration_ms);
    let state = context.state.get_or_default::<ToolTimerState>();
    if state.started_at.is_some() && is_partial && state.interval.is_none() {
        state.interval = Some(ui_loop::set_interval(Duration::from_secs(1), move || invalidate()));
    }
    if !is_partial || is_error {
        state.ended_at.get_or_insert_with(Instant::now);
        if let Some(interval) = state.interval.take() {
            ui_loop::clear_timer(interval);
        }
    }
    // A final result's recorded duration wins: it is monotonic and survives reloads. The
    // renderer's own clock is the fallback for live progress and for results stored without one.
    if let (false, Some(duration)) = (is_partial, duration_ms) {
        return Some(format!("Took {}", format_tool_duration(duration)));
    }
    let started = state.started_at?;
    let label = if is_partial { "Elapsed" } else { "Took" };
    let elapsed = state.ended_at.unwrap_or_else(Instant::now).duration_since(started).as_millis() as u64;
    Some(format!("{label} {}", format_tool_duration(elapsed)))
}

#[cfg(test)]
mod tests {
    use super::format_tool_duration;

    #[test]
    fn formats_durations() {
        assert_eq!(format_tool_duration(1500), "1.5s");
        assert_eq!(format_tool_duration(125_000), "2m 5s");
        assert_eq!(format_tool_duration(3_725_000), "1h 2m 5s");
    }
}
