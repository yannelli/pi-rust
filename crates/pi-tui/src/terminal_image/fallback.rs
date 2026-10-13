//! OSC 8 hyperlinks and the text shown when the terminal cannot draw an image.

use std::fmt::Write as _;
use std::path::Path;

use super::ImageDimensions;
use super::capabilities::get_capabilities;

/// Wraps `text` in an OSC 8 hyperlink to `url`. Terminals without OSC 8 support
/// ignore the sequences and show only the text.
pub fn hyperlink(text: &str, url: &str) -> String {
    format!("\x1b]8;;{url}\x1b\\{text}\x1b]8;;\x1b\\")
}

fn shorten_image_path(filename: &str, home: Option<&str>) -> String {
    if let Some(home) = home.filter(|home| !home.is_empty()) {
        if let Some(rest) = filename.strip_prefix(home) {
            if rest.is_empty() || rest.starts_with('/') || rest.starts_with('\\') {
                return format!("~{rest}");
            }
        }
    }
    filename.to_owned()
}

fn is_separator(byte: u8) -> bool {
    byte == b'/' || (cfg!(windows) && byte == b'\\')
}

/// Absolute paths as Node's `path.isAbsolute` sees them on this platform.
fn is_absolute_path(path: &str) -> bool {
    let bytes = path.as_bytes();
    if bytes.first().is_some_and(|&byte| is_separator(byte)) {
        return true;
    }
    cfg!(windows) && bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && is_separator(bytes[2])
}

/// Resolves `.` and `..` segments and repeated separators of an absolute path, keeping a trailing slash.
fn normalize_segments(path: &str) -> String {
    let mut segments: Vec<&str> = Vec::new();
    for segment in path.split(|ch| ch == '/' || (cfg!(windows) && ch == '\\')) {
        match segment {
            "" | "." => {}
            ".." => {
                segments.pop();
            }
            other => segments.push(other),
        }
    }
    let mut normalized = format!("/{}", segments.join("/"));
    let trailing = path.ends_with('/') || (cfg!(windows) && path.ends_with('\\'));
    if trailing && !normalized.ends_with('/') {
        normalized.push('/');
    }
    normalized
}

fn percent_encode_path(path: &str) -> String {
    let mut encoded = String::with_capacity(path.len());
    for byte in path.bytes() {
        let keep = byte.is_ascii_graphic()
            && !matches!(byte, b'"' | b'#' | b'%' | b'<' | b'>' | b'?' | b'`' | b'{' | b'}' | b'\\');
        if keep {
            encoded.push(char::from(byte));
        } else {
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    encoded
}

/// A `file://` URL for an absolute path, encoded like Node's `pathToFileURL`.
fn path_to_file_url(path: &str) -> String {
    if cfg!(windows) {
        let bytes = path.as_bytes();
        if bytes.len() >= 2 && bytes[1] == b':' {
            let drive = &path[..2];
            return format!("file:///{drive}{}", percent_encode_path(&normalize_segments(&path[2..])));
        }
    }
    format!("file://{}", percent_encode_path(&normalize_segments(path)))
}

/// Text shown when the terminal cannot draw an inline image. Paths under the home directory are
/// shortened to `~/...`; absolute paths link to `file://` when OSC 8 hyperlinks are available.
pub fn image_fallback(mime_type: &str, dimensions: Option<ImageDimensions>, filename: Option<&str>) -> String {
    let home = std::env::home_dir();
    image_fallback_with_home(mime_type, dimensions, filename, home.as_deref())
}

/// Like [`image_fallback`], with the home directory given instead of read from the environment.
pub fn image_fallback_with_home(
    mime_type: &str,
    dimensions: Option<ImageDimensions>,
    filename: Option<&str>,
    home: Option<&Path>,
) -> String {
    let mut parts = Vec::new();
    if let Some(filename) = filename.filter(|name| !name.is_empty()) {
        let home = home.map(|home| home.to_string_lossy());
        let display = shorten_image_path(filename, home.as_deref());
        if get_capabilities().hyperlinks && is_absolute_path(filename) {
            parts.push(hyperlink(&display, &path_to_file_url(filename)));
        } else {
            parts.push(display);
        }
    }
    parts.push(format!("[{mime_type}]"));
    if let Some(dimensions) = dimensions {
        parts.push(format!("{}x{}", dimensions.width_px, dimensions.height_px));
    }
    format!("[Image: {}]", parts.join(" "))
}
