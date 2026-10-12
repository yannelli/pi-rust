//! Mouse wheel line counts with optional acceleration (upstream `wheel-scroll.ts`).

use crate::env::{EnvLookup, ProcessEnv};

/// Lines moved per wheel event, or `Auto` to accelerate fast wheel spins.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum WheelScrollLines {
    Fixed(f64),
    #[default]
    Auto,
}

/// Events closer than this belong to one physical notch or a high-resolution source.
const BURST_GAP_MS: f64 = 5.0;
/// A pause longer than this ends a scroll gesture.
const GESTURE_GAP_MS: f64 = 200.0;
/// Average event gap that maps to one line per event.
const REFERENCE_GAP_MS: f64 = 100.0;
const MAX_AUTO_LINES: f64 = 6.0;

/// Local macOS terminals get wheel deltas the OS already accelerated, one event per line. Other
/// platforms and SSH sessions usually send one event per notch.
pub fn terminal_accelerates_wheel(env: &dyn EnvLookup) -> bool {
    cfg!(target_os = "macos")
        && env.var("SSH_CONNECTION").is_none()
        && env.var("SSH_CLIENT").is_none()
        && env.var("SSH_TTY").is_none()
}

/// Converts wheel events into line counts. In `Auto` mode on terminals that do not accelerate,
/// the count follows event velocity: notches 100 ms apart move 1 line, 50 ms apart move 2.
#[derive(Debug, Clone)]
pub struct WheelScrollAccelerator {
    lines: WheelScrollLines,
    accelerate: bool,
    last_time: f64,
    last_direction: i32,
    average_gap: Option<f64>,
    carry: f64,
}

impl WheelScrollAccelerator {
    /// Accelerator that detects whether the terminal accelerates the wheel itself.
    pub fn new(lines: WheelScrollLines) -> Self {
        Self::with_acceleration(lines, !terminal_accelerates_wheel(&ProcessEnv))
    }

    pub fn with_acceleration(lines: WheelScrollLines, accelerate: bool) -> Self {
        Self { lines, accelerate, last_time: f64::NEG_INFINITY, last_direction: 0, average_gap: None, carry: 0.0 }
    }

    pub fn set_lines(&mut self, lines: WheelScrollLines) {
        self.lines = lines;
        self.last_time = f64::NEG_INFINITY;
        self.last_direction = 0;
        self.average_gap = None;
        self.carry = 0.0;
    }

    /// Positive line count for a wheel event in `direction` (-1 or 1) at `now` milliseconds.
    pub fn next(&mut self, direction: i32, now: f64) -> u32 {
        match self.lines {
            WheelScrollLines::Fixed(lines) => {
                return if lines.is_finite() { lines.floor().max(1.0) as u32 } else { 1 };
            }
            WheelScrollLines::Auto => {}
        }
        if !self.accelerate {
            return 1;
        }
        let gap = now - self.last_time;
        let same_gesture = direction == self.last_direction && gap <= GESTURE_GAP_MS;
        self.last_time = now;
        self.last_direction = direction;
        if !same_gesture {
            self.average_gap = None;
            self.carry = 0.0;
            return 1;
        }
        if gap < BURST_GAP_MS {
            return 1;
        }
        let average = self.average_gap.map_or(gap, |avg| (avg + gap) / 2.0);
        self.average_gap = Some(average);
        let lines = (REFERENCE_GAP_MS / average).clamp(1.0, MAX_AUTO_LINES) + self.carry;
        let whole = lines.floor();
        self.carry = lines - whole;
        whole as u32
    }
}
