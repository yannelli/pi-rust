//! Clipboard images for paste (upstream `utils/clipboard-image.ts`): wl-paste, xclip,
//! PowerShell on Windows and WSL, and osascript on macOS. Other formats become PNG.

use std::time::Duration;

use super::command::DEFAULT_TIMEOUT;
use super::{ClipboardContext, ClipboardEnv, Platform, is_wsl, windows, with_system_context};
use crate::images::convert_image_bytes_to_png;
use crate::tools::detect_supported_image_mime_type;

const SUPPORTED_IMAGE_MIME_TYPES: [&str; 4] = ["image/png", "image/jpeg", "image/webp", "image/gif"];
const LIST_TIMEOUT: Duration = Duration::from_millis(1000);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClipboardImage {
    pub bytes: Vec<u8>,
    pub mime_type: String,
}

/// A backend's answer: failed (try the next one), no image, or an image.
enum Read {
    Unavailable,
    Empty,
    Image(ClipboardImage),
}

pub fn is_wayland_session(env: &ClipboardEnv) -> bool {
    env.has("WAYLAND_DISPLAY") || env.var("XDG_SESSION_TYPE") == Some("wayland")
}

fn base_mime_type(mime_type: &str) -> String {
    mime_type.split(';').next().unwrap_or_default().trim().to_lowercase()
}

/// File extension for a pasted image type.
pub fn extension_for_image_mime_type(mime_type: &str) -> Option<&'static str> {
    match base_mime_type(mime_type).as_str() {
        "image/png" => Some("png"),
        "image/jpeg" => Some("jpg"),
        "image/webp" => Some("webp"),
        "image/gif" => Some("gif"),
        _ => None,
    }
}

fn select_preferred_image_mime_type(types: &str) -> Option<String> {
    let types: Vec<&str> = types.lines().map(str::trim).filter(|t| !t.is_empty()).collect();
    SUPPORTED_IMAGE_MIME_TYPES
        .iter()
        .find_map(|preferred| types.iter().find(|t| base_mime_type(t) == *preferred))
        .or_else(|| types.iter().find(|t| base_mime_type(t).starts_with("image/")))
        .map(|t| (*t).to_string())
}

fn run(context: &ClipboardContext<'_>, command: &str, args: &[&str], timeout: Duration) -> Option<Vec<u8>> {
    let args: Vec<String> = args.iter().map(|arg| (*arg).to_string()).collect();
    context.runner.run(command, &args, None, timeout)
}

/// List types, then fetch the preferred image type. An empty clipboard is `Empty`, so a
/// Wayland session never falls through to stale X11 contents.
fn read_listed(
    context: &ClipboardContext<'_>,
    command: &str,
    list: &[&str],
    fetch: impl Fn(&str) -> Vec<String>,
) -> Read {
    let Some(types) = run(context, command, list, LIST_TIMEOUT) else { return Read::Unavailable };
    let Some(selected) = select_preferred_image_mime_type(&String::from_utf8_lossy(&types)) else { return Read::Empty };
    let args = fetch(&selected);
    match context.runner.run(command, &args, None, DEFAULT_TIMEOUT) {
        None => Read::Unavailable,
        Some(bytes) if bytes.is_empty() => Read::Empty,
        Some(bytes) => Read::Image(ClipboardImage { bytes, mime_type: base_mime_type(&selected) }),
    }
}

fn read_via_wl_paste(context: &ClipboardContext<'_>) -> Read {
    read_listed(context, "wl-paste", &["--list-types"], |selected| {
        vec!["--type".into(), selected.into(), "--no-newline".into()]
    })
}

fn read_via_xclip(context: &ClipboardContext<'_>) -> Read {
    read_listed(context, "xclip", &["-selection", "clipboard", "-t", "TARGETS", "-o"], |selected| {
        vec!["-selection".into(), "clipboard".into(), "-t".into(), selected.into(), "-o".into()]
    })
}

fn png(bytes: Vec<u8>) -> ClipboardImage {
    ClipboardImage { bytes, mime_type: "image/png".into() }
}

/// `the clipboard as «class PNGf»` prints `«data PNGf<hex>»`.
fn parse_osascript_png(output: &[u8]) -> Option<Vec<u8>> {
    let text = String::from_utf8_lossy(output);
    let hex = text.split_once("«data PNGf")?.1.split('»').next()?.trim();
    if hex.len() % 2 != 0 {
        return None;
    }
    (0..hex.len()).step_by(2).map(|at| u8::from_str_radix(hex.get(at..at + 2)?, 16).ok()).collect()
}

fn read_native(context: &ClipboardContext<'_>) -> Read {
    let bytes = match context.env.platform {
        Platform::MacOs => {
            let script = ["-e", "the clipboard as «class PNGf»"];
            run(context, "osascript", &script, DEFAULT_TIMEOUT).and_then(|out| parse_osascript_png(&out))
        }
        Platform::Windows => windows::read_image_png(context, false),
        _ => return Read::Unavailable,
    };
    match bytes {
        Some(bytes) if !bytes.is_empty() => {
            let mime_type = detect_supported_image_mime_type(&bytes).unwrap_or("application/octet-stream");
            Read::Image(ClipboardImage { mime_type: mime_type.to_string(), bytes })
        }
        _ => Read::Empty,
    }
}

/// Read an image from the system clipboard.
pub fn read_clipboard_image() -> Option<ClipboardImage> {
    with_system_context(read_clipboard_image_with)
}

pub fn read_clipboard_image_with(context: &ClipboardContext<'_>) -> Option<ClipboardImage> {
    let env = &context.env;
    if env.has("TERMUX_VERSION") {
        return None;
    }
    let mut image = Read::Unavailable;
    if env.platform == Platform::Linux {
        let wsl = is_wsl(env);
        if is_wayland_session(env) || wsl {
            image = read_via_wl_paste(context);
        }
        if matches!(image, Read::Unavailable) {
            image = read_via_xclip(context);
        }
        // Windows screenshots reach WSL only through PowerShell; keep Linux's answer otherwise.
        if wsl && !matches!(image, Read::Image(_)) {
            if let Some(bytes) = windows::read_image_png(context, true) {
                image = Read::Image(png(bytes));
            }
        }
        if matches!(image, Read::Unavailable) {
            image = read_native(context);
        }
    } else {
        image = read_native(context);
    }

    let Read::Image(image) = image else { return None };
    if SUPPORTED_IMAGE_MIME_TYPES.contains(&base_mime_type(&image.mime_type).as_str()) {
        return Some(image);
    }
    // For example Windows DIB data wrapped as BMP.
    convert_image_bytes_to_png(&image.bytes).map(png)
}
