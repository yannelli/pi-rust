//! System clipboard for `/copy` and paste (upstream `utils/clipboard.ts`, `utils/wsl.ts`).
//! Platform tools do the work: pbcopy and pbpaste, wl-clipboard, xclip, xsel, Termux:API,
//! PowerShell and clip.exe, with OSC 52 for remote and display-less terminals.
//! Calls block for up to a few seconds; async callers should use `spawn_blocking`.

pub mod command;
mod image;
mod paste;
mod windows;

#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::io::Write;
use std::time::Duration;

use anyhow::bail;
use base64::Engine;

pub use command::{ClipboardRunner, SystemClipboardRunner, run_clipboard_command};
pub use image::{
    ClipboardImage, extension_for_image_mime_type, is_wayland_session, read_clipboard_image, read_clipboard_image_with,
};
pub use paste::{read_clipboard_file_paths, read_clipboard_file_paths_with, save_clipboard_image};

const MAX_OSC52_ENCODED_LENGTH: usize = 100_000;
const COMMAND_TIMEOUT: Duration = Duration::from_millis(5000);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Platform {
    Linux,
    MacOs,
    Windows,
    Android,
    Other,
}

impl Platform {
    pub fn current() -> Self {
        if cfg!(target_os = "android") {
            Self::Android
        } else if cfg!(target_os = "linux") {
            Self::Linux
        } else if cfg!(target_os = "macos") {
            Self::MacOs
        } else if cfg!(windows) {
            Self::Windows
        } else {
            Self::Other
        }
    }
}

/// Platform, environment, and `/proc/version` as the clipboard code sees them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClipboardEnv {
    pub platform: Platform,
    pub vars: HashMap<String, String>,
    pub proc_version: Option<String>,
}

impl ClipboardEnv {
    pub fn from_process() -> Self {
        Self {
            platform: Platform::current(),
            vars: std::env::vars().collect(),
            proc_version: std::fs::read_to_string("/proc/version").ok(),
        }
    }

    /// A non-empty variable (JavaScript truthiness).
    pub fn var(&self, name: &str) -> Option<&str> {
        self.vars.get(name).map(String::as_str).filter(|value| !value.is_empty())
    }

    fn has(&self, name: &str) -> bool {
        self.var(name).is_some()
    }
}

/// Windows Subsystem for Linux, where Windows programs are reachable through interop.
pub fn is_wsl(env: &ClipboardEnv) -> bool {
    if env.has("WSL_DISTRO_NAME") || env.has("WSLENV") {
        return true;
    }
    env.proc_version.as_deref().is_some_and(|release| {
        let release = release.to_lowercase();
        release.contains("microsoft") || release.contains("wsl")
    })
}

fn is_remote_session(env: &ClipboardEnv) -> bool {
    env.has("SSH_CONNECTION") || env.has("SSH_CLIENT") || env.has("MOSH_CONNECTION")
}

/// The environment, a command runner, and where OSC 52 sequences are written.
pub struct ClipboardContext<'a> {
    pub env: ClipboardEnv,
    pub runner: &'a dyn ClipboardRunner,
    pub write_terminal: &'a dyn Fn(&str),
}

fn write_stdout(sequence: &str) {
    let mut stdout = std::io::stdout();
    let _ = stdout.write_all(sequence.as_bytes());
    let _ = stdout.flush();
}

fn with_system_context<T>(f: impl FnOnce(&ClipboardContext<'_>) -> T) -> T {
    let context = ClipboardContext {
        env: ClipboardEnv::from_process(),
        runner: &SystemClipboardRunner,
        write_terminal: &write_stdout,
    };
    f(&context)
}

fn emit_osc52(context: &ClipboardContext<'_>, text: &str) -> bool {
    let encoded = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    if encoded.len() > MAX_OSC52_ENCODED_LENGTH {
        return false;
    }
    (context.write_terminal)(&format!("\x1b]52;c;{encoded}\x07"));
    true
}

fn run(context: &ClipboardContext<'_>, command: &str, args: &[&str], input: Option<&[u8]>) -> Option<Vec<u8>> {
    let args: Vec<String> = args.iter().map(|arg| (*arg).to_string()).collect();
    context.runner.run(command, &args, input, COMMAND_TIMEOUT)
}

/// Read plain text from the system clipboard.
pub fn read_clipboard_text() -> Option<String> {
    with_system_context(read_clipboard_text_with)
}

pub fn read_clipboard_text_with(context: &ClipboardContext<'_>) -> Option<String> {
    let env = &context.env;
    let mut commands: Vec<(&str, Vec<&str>)> = Vec::new();
    if env.has("TERMUX_VERSION") {
        commands.push(("termux-clipboard-get", vec![]));
    }
    if env.platform == Platform::Linux {
        if env.has("WAYLAND_DISPLAY") {
            commands.push(("wl-paste", vec!["--no-newline", "--type", "text"]));
        }
        if env.has("DISPLAY") {
            commands.push(("xclip", vec!["-selection", "clipboard", "-out"]));
            commands.push(("xsel", vec!["--clipboard", "--output"]));
        }
    }
    for (command, args) in commands {
        if let Some(bytes) = run(context, command, &args, None) {
            return Some(String::from_utf8_lossy(&bytes).into_owned()).filter(|text| !text.is_empty());
        }
    }
    let native = match env.platform {
        Platform::MacOs => run(context, "pbpaste", &[], None),
        Platform::Windows => windows::read_text(context),
        _ => None,
    };
    native.map(|bytes| String::from_utf8_lossy(&bytes).into_owned()).filter(|text| !text.is_empty())
}

/// Copy text to the system clipboard.
pub fn copy_to_clipboard(text: &str) -> anyhow::Result<()> {
    with_system_context(|context| copy_to_clipboard_with(context, text))
}

pub fn copy_to_clipboard_with(context: &ClipboardContext<'_>, text: &str) -> anyhow::Result<()> {
    let env = &context.env;
    let platform = env.platform;
    // On Windows PowerShell stands in for the native writer; clip.exe mangles non-ASCII text.
    let mut copied = platform == Platform::Windows && windows::copy_text(context, text, false);
    if !copied {
        let mut commands: Vec<(&str, Vec<&str>)> = Vec::new();
        match platform {
            Platform::MacOs => commands.push(("pbcopy", vec![])),
            Platform::Windows => commands.push(("clip", vec![])),
            _ => {
                if env.has("TERMUX_VERSION") {
                    commands.push(("termux-clipboard-set", vec![]));
                }
                if env.has("WAYLAND_DISPLAY") {
                    commands.push(("wl-copy", vec![]));
                }
                if env.has("DISPLAY") {
                    commands.push(("xclip", vec!["-selection", "clipboard"]));
                    commands.push(("xsel", vec!["--clipboard", "--input"]));
                }
            }
        }
        copied = commands.iter().any(|(command, args)| run(context, command, args, Some(text.as_bytes())).is_some());
    }

    let mut osc52_emitted = false;
    if !copied && platform == Platform::Linux && is_wsl(env) {
        // Windows Terminal supports OSC 52; prefer it over the slower PowerShell round trip.
        if env.has("WT_SESSION") {
            osc52_emitted = emit_osc52(context, text);
        }
        // WSL without WSLg has no Linux display, so write the Windows clipboard through interop.
        copied = osc52_emitted || windows::copy_text(context, text, true);
    }
    // Nothing confirms an OSC 52 write, so a desktop session reports the failure instead.
    // Without a display the terminal is the only route, and remote sessions always emit it.
    let headless =
        platform == Platform::Linux && !env.has("DISPLAY") && !env.has("WAYLAND_DISPLAY") && !env.has("TERMUX_VERSION");
    let mut oversized = false;
    if !osc52_emitted && (is_remote_session(env) || (!copied && headless)) {
        if emit_osc52(context, text) {
            copied = true;
        } else {
            oversized = true;
        }
    }
    if copied {
        return Ok(());
    }
    if oversized {
        bail!("Clipboard unavailable: text exceeds the OSC 52 size limit");
    }
    if env.has("TERMUX_VERSION") {
        bail!("Clipboard unavailable: install the Termux:API app and `termux-api` package");
    }
    if platform == Platform::Linux {
        if env.has("WAYLAND_DISPLAY") {
            bail!("Clipboard unavailable: install `wl-clipboard` (`wl-copy`) or check Wayland access");
        }
        if env.has("DISPLAY") {
            bail!("Clipboard unavailable: install `xclip` or `xsel`, or check X11 access");
        }
    }
    bail!("Clipboard unavailable")
}
