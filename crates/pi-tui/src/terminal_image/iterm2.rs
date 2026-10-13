//! iTerm2 inline images (OSC 1337 `File=`).

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;

/// Options for [`encode_iterm2`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Iterm2EncodeOptions {
    /// Width: cells (`"10"`), pixels (`"100px"`), percent (`"50%"`), or `"auto"`.
    pub width: Option<String>,
    /// Height, in the same forms as `width`.
    pub height: Option<String>,
    /// File name shown by iTerm2. Empty names are omitted.
    pub name: Option<String>,
    /// Whether to keep the aspect ratio. Defaults to true.
    pub preserve_aspect_ratio: Option<bool>,
    /// Whether to show the image inline instead of downloading it. Defaults to true.
    pub inline: Option<bool>,
}

/// Decoded size of base64 text, computed the way Node's `Buffer.byteLength(data, "base64")` does.
fn base64_byte_length(data: &str) -> usize {
    let bytes = data.as_bytes();
    let mut length = bytes.len();
    if length > 0 && bytes[length - 1] == b'=' {
        length -= 1;
    }
    if length > 1 && bytes[length - 1] == b'=' {
        length -= 1;
    }
    (length * 3) >> 2
}

/// Encodes base64 image data as an iTerm2 OSC 1337 inline image sequence.
pub fn encode_iterm2(base64_data: &str, options: &Iterm2EncodeOptions) -> String {
    let mut params = vec![
        format!("inline={}", if options.inline == Some(false) { 0 } else { 1 }),
        format!("size={}", base64_byte_length(base64_data)),
    ];
    if let Some(width) = &options.width {
        params.push(format!("width={width}"));
    }
    if let Some(height) = &options.height {
        params.push(format!("height={height}"));
    }
    if let Some(name) = options.name.as_deref().filter(|name| !name.is_empty()) {
        params.push(format!("name={}", STANDARD.encode(name)));
    }
    if options.preserve_aspect_ratio == Some(false) {
        params.push("preserveAspectRatio=0".to_owned());
    }
    format!("\x1b]1337;File={}:{base64_data}\x07", params.join(";"))
}
