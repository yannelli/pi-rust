//! Terminal capability detection, the process-wide capability cache, and cell dimensions.

use std::io::Read as _;
use std::process::{Command, Stdio};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use super::{CellDimensions, ImageProtocol, TerminalCapabilities};
use crate::colors::TerminalColorMode;
use crate::env::{EnvLookup, ProcessEnv};

/// Capabilities that replace detected values. `None` keeps the detected value;
/// `images: Some(None)` turns images off.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct CapabilityOverrides {
    /// Image protocol override.
    pub images: Option<Option<ImageProtocol>>,
    /// Truecolor override.
    pub true_color: Option<bool>,
    /// Hyperlink override. When set, the tmux probe is skipped.
    pub hyperlinks: Option<bool>,
}

struct CapabilityState {
    cached: Option<TerminalCapabilities>,
    overrides: CapabilityOverrides,
}

static CAPABILITY_STATE: Mutex<CapabilityState> = Mutex::new(CapabilityState {
    cached: None,
    overrides: CapabilityOverrides { images: None, true_color: None, hyperlinks: None },
});
// Default cell dimensions; the TUI updates them when the terminal answers the size query.
static CELL_DIMENSIONS: Mutex<CellDimensions> = Mutex::new(CellDimensions { width_px: 9, height_px: 18 });

const TMUX_PROBE_TIMEOUT: Duration = Duration::from_millis(250);

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The current cell size in pixels.
pub fn get_cell_dimensions() -> CellDimensions {
    *lock(&CELL_DIMENSIONS)
}

/// Sets the cell size in pixels, usually from the terminal's reply to a size query.
pub fn set_cell_dimensions(dims: CellDimensions) {
    *lock(&CELL_DIMENSIONS) = dims;
}

/// Whether the attached tmux client forwards OSC 8 hyperlinks to the outer terminal.
/// tmux re-emits them only when `client_termfeatures` lists `hyperlinks`.
/// Any error, or no answer within 250 ms, counts as `false`.
pub fn probe_tmux_hyperlinks() -> bool {
    let child = Command::new("tmux")
        .args(["display-message", "-p", "#{client_termfeatures}"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn();
    let Ok(mut child) = child else {
        return false;
    };
    let deadline = Instant::now() + TMUX_PROBE_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(5)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    };
    let mut output = Vec::new();
    let read = child.stdout.take().map(|mut stdout| stdout.read_to_end(&mut output));
    if !status.success() || !matches!(read, Some(Ok(_))) {
        return false;
    }
    String::from_utf8_lossy(&output).split(',').map(str::trim).any(|feature| feature == "hyperlinks")
}

fn capabilities(images: Option<ImageProtocol>, true_color: bool, hyperlinks: bool) -> TerminalCapabilities {
    TerminalCapabilities { images, true_color, hyperlinks }
}

fn detect_from_environment(
    env: &dyn EnvLookup,
    tmux_forwards_hyperlinks: &dyn Fn() -> bool,
    is_windows_console: bool,
) -> TerminalCapabilities {
    let lower = |name: &str| env.var(name).unwrap_or_default().to_lowercase();
    let set = |name: &str| env.non_empty(name).is_some();
    let term_program = lower("TERM_PROGRAM");
    let terminal_emulator = lower("TERMINAL_EMULATOR");
    let term = lower("TERM");
    let color_term = lower("COLORTERM");
    let true_color_hint = color_term == "truecolor" || color_term == "24bit" || term.ends_with("-direct");
    let kitty = capabilities(Some(ImageProtocol::Kitty), true, true);

    // Emit OSC 8 hyperlinks only when tmux confirms it forwards them. Image protocols
    // are unreliable under tmux, so images stay off.
    if set("TMUX") || term.starts_with("tmux") {
        return capabilities(None, true_color_hint, tmux_forwards_hyperlinks());
    }
    // screen does not forward OSC 8 hyperlinks.
    if term.starts_with("screen") {
        return capabilities(None, true_color_hint, false);
    }
    // Herdr forwards hyperlinks but runs inside another terminal whose variables
    // (such as KITTY_WINDOW_ID) can leak into its panes, so it is checked first.
    if term_program == "herdr" {
        return capabilities(None, true_color_hint, true);
    }
    // Rex supports OSC 8 hyperlinks but only partially implements the Kitty graphics protocol
    // (no Unicode placeholders), so images stay off. Checked before outer-terminal detection.
    if term_program == "rex" {
        return capabilities(None, true, true);
    }
    if set("KITTY_WINDOW_ID") || term_program == "kitty" {
        return kitty;
    }
    if term_program == "ghostty" || term.contains("ghostty") || set("GHOSTTY_RESOURCES_DIR") {
        return kitty;
    }
    if set("WEZTERM_PANE") || term_program == "wezterm" {
        return kitty;
    }
    // Warp supports the Kitty graphics protocol and OSC 8 hyperlinks.
    if term_program == "warpterminal" || set("WARP_SESSION_ID") || set("WARP_TERMINAL_SESSION_UUID") {
        return kitty;
    }
    if set("ITERM_SESSION_ID") || term_program == "iterm.app" {
        return capabilities(Some(ImageProtocol::Iterm2), true, true);
    }
    if set("WT_SESSION") {
        return capabilities(None, true, true);
    }
    if matches!(term_program.as_str(), "alacritty" | "vscode" | "zed") {
        return capabilities(None, true, true);
    }
    if terminal_emulator == "jetbrains-jediterm" {
        return capabilities(None, true, false);
    }
    // Windows Terminal does not always set WT_SESSION. Modern Windows consoles support
    // truecolor; hyperlinks stay off unless support was detected above.
    if is_windows_console {
        return capabilities(None, true, false);
    }
    // Unknown terminal: terminals that swallow OSC 8 would hide the URL, so keep
    // hyperlinks off and use the plain `text (url)` form.
    capabilities(None, true_color_hint, false)
}

fn parse_boolean_override(value: Option<String>) -> Option<bool> {
    match value.as_deref() {
        Some("1") => Some(true),
        Some("0") => Some(false),
        _ => None,
    }
}

/// Detects capabilities from the process environment, probing tmux when needed.
pub fn detect_capabilities() -> TerminalCapabilities {
    detect_capabilities_with(&ProcessEnv, &probe_tmux_hyperlinks)
}

/// Detects capabilities from `env`. `tmux_forwards_hyperlinks` runs only under tmux and
/// only when `PI_HYPERLINKS` does not decide. Honors `PI_HYPERLINKS`, `PI_IMAGE_PROTOCOL`, `PI_TRUE_COLOR`.
pub fn detect_capabilities_with(
    env: &dyn EnvLookup,
    tmux_forwards_hyperlinks: &dyn Fn() -> bool,
) -> TerminalCapabilities {
    let hyperlinks = parse_boolean_override(env.var("PI_HYPERLINKS"));
    let detected = match hyperlinks {
        Some(value) => detect_from_environment(env, &move || value, cfg!(windows)),
        None => detect_from_environment(env, tmux_forwards_hyperlinks, cfg!(windows)),
    };
    let images = match env.var("PI_IMAGE_PROTOCOL").map(|value| value.to_lowercase()).as_deref() {
        Some("kitty") => Some(Some(ImageProtocol::Kitty)),
        Some("iterm2") => Some(Some(ImageProtocol::Iterm2)),
        Some("none" | "0") => Some(None),
        _ => None,
    };
    let true_color = parse_boolean_override(env.var("PI_TRUE_COLOR"));
    TerminalCapabilities {
        images: images.unwrap_or(detected.images),
        true_color: true_color.unwrap_or(detected.true_color),
        hyperlinks: hyperlinks.unwrap_or(detected.hyperlinks),
    }
}

/// The cached capabilities, detected from the process environment on first use.
pub fn get_capabilities() -> TerminalCapabilities {
    get_capabilities_with(&ProcessEnv)
}

/// The cached capabilities; on a cache miss they are detected from `env` and the overrides applied.
pub fn get_capabilities_with(env: &dyn EnvLookup) -> TerminalCapabilities {
    let mut state = lock(&CAPABILITY_STATE);
    if let Some(cached) = state.cached {
        return cached;
    }
    let overrides = state.overrides;
    let detected = match overrides.hyperlinks {
        Some(value) => detect_capabilities_with(env, &move || value),
        None => detect_capabilities_with(env, &probe_tmux_hyperlinks),
    };
    let capabilities = TerminalCapabilities {
        images: overrides.images.unwrap_or(detected.images),
        true_color: overrides.true_color.unwrap_or(detected.true_color),
        hyperlinks: overrides.hyperlinks.unwrap_or(detected.hyperlinks),
    };
    state.cached = Some(capabilities);
    capabilities
}

/// The color mode the capabilities allow.
pub fn get_terminal_color_mode(caps: &TerminalCapabilities) -> TerminalColorMode {
    if caps.true_color { TerminalColorMode::Truecolor } else { TerminalColorMode::Color256 }
}

/// Clears the cached capabilities so the next read detects them again.
pub fn reset_capabilities_cache() {
    lock(&CAPABILITY_STATE).cached = None;
}

/// Overrides selected detected capabilities. Changing the overrides clears the cache.
pub fn set_capability_overrides(overrides: CapabilityOverrides) {
    let mut state = lock(&CAPABILITY_STATE);
    if state.overrides == overrides {
        return;
    }
    state.overrides = overrides;
    state.cached = None;
}

/// Replaces the cached capabilities. Useful in tests to exercise both code paths.
pub fn set_capabilities(caps: TerminalCapabilities) {
    lock(&CAPABILITY_STATE).cached = Some(caps);
}
