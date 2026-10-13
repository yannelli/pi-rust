//! Image handling for prompts and tool results: format normalization, auto-resize to the
//! model's limits, EXIF orientation, and the `images.blockImages` filter.

mod block;
pub mod exif;
mod resize;

#[cfg(test)]
mod tests;

use base64::Engine;
use pi_ai::{ContentBlock, ImageResizeOptions, Model};

pub use block::{BLOCKED_IMAGE_TEXT, block_images};
pub use resize::{
    DEFAULT_JPEG_QUALITY, DEFAULT_MAX_BYTES, DEFAULT_MAX_HEIGHT, DEFAULT_MAX_WIDTH, ResizeLimits, ResizedImage,
    decode_oriented, encode_jpeg, encode_png, format_dimension_note, resize_image,
};

/// How images entering the conversation are processed.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ProcessImageOptions {
    /// Resize to inline provider limits (`images.autoResize`, default true).
    pub auto_resize: bool,
    /// Resize profile; `None` uses the built-in defaults.
    pub resize: Option<ImageResizeOptions>,
}

impl Default for ProcessImageOptions {
    fn default() -> Self {
        Self { auto_resize: true, resize: None }
    }
}

impl ProcessImageOptions {
    /// Options with the model's `inputLimits.images.resize` profile.
    pub fn for_model(model: &Model, auto_resize: bool) -> Self {
        let resize = model.input_limits.and_then(|limits| limits.images).and_then(|images| images.resize);
        Self { auto_resize, resize }
    }
}

/// An image ready to send: base64 data, its MIME type, and notes for the model.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessedImage {
    pub data: String,
    pub mime_type: String,
    pub hints: Vec<String>,
}

fn base_mime_type(mime_type: &str) -> String {
    mime_type.split(';').next().unwrap_or_default().trim().to_lowercase()
}

fn normalize_supported_mime_type(mime_type: &str) -> Option<&'static str> {
    match base_mime_type(mime_type).as_str() {
        "image/png" => Some("image/png"),
        "image/jpeg" | "image/jpg" => Some("image/jpeg"),
        "image/gif" => Some("image/gif"),
        "image/webp" => Some("image/webp"),
        _ => None,
    }
}

/// Decode any supported format (with EXIF orientation) and encode it as PNG.
pub fn convert_image_bytes_to_png(bytes: &[u8]) -> Option<Vec<u8>> {
    encode_png(&decode_oriented(bytes)?.into_rgba8())
}

/// Base64 image to PNG for terminals that only show PNG; PNG input is returned as is.
pub fn convert_to_png(base64_data: &str, mime_type: &str) -> Option<(String, String)> {
    if mime_type == "image/png" {
        return Some((base64_data.to_string(), mime_type.to_string()));
    }
    let png = convert_image_bytes_to_png(&decode_base64_lenient(base64_data))?;
    Some((base64::engine::general_purpose::STANDARD.encode(png), "image/png".into()))
}

/// Decode like Node's `Buffer.from(data, "base64")`: URL-safe characters are accepted,
/// other characters are skipped, and decoding stops at the first `=`.
pub fn decode_base64_lenient(data: &str) -> Vec<u8> {
    use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
    let mut cleaned: String = data
        .chars()
        .take_while(|c| *c != '=')
        .filter_map(|c| match c {
            '-' => Some('+'),
            '_' => Some('/'),
            c if c.is_ascii_alphanumeric() || c == '+' || c == '/' => Some(c),
            _ => None,
        })
        .collect();
    if cleaned.len() % 4 == 1 {
        cleaned.pop();
    }
    let config = GeneralPurposeConfig::new()
        .with_decode_padding_mode(DecodePaddingMode::RequireNone)
        .with_decode_allow_trailing_bits(true);
    GeneralPurpose::new(&base64::alphabet::STANDARD, config).decode(cleaned).unwrap_or_default()
}

fn conversion_hint(from: Option<&str>, to: &str) -> Option<String> {
    from.filter(|from| *from != to).map(|from| format!("[Image converted from {from} to {to}.]"))
}

/// Normalize, then resize when enabled (upstream `processImage`). `Err` carries the
/// `[Image omitted: ...]` note that replaces the image.
pub fn process_image(bytes: &[u8], mime_type: &str, options: &ProcessImageOptions) -> Result<ProcessedImage, String> {
    let converted;
    let (bytes, mime_type, converted_from) = match normalize_supported_mime_type(mime_type) {
        Some(normalized) => (bytes, normalized, None),
        None => {
            converted = convert_image_bytes_to_png(bytes).ok_or_else(|| {
                "[Image omitted: could not be converted to a supported inline image format.]".to_string()
            })?;
            (converted.as_slice(), "image/png", Some(base_mime_type(mime_type)))
        }
    };

    let mut hints = Vec::new();
    if !options.auto_resize {
        hints.extend(conversion_hint(converted_from.as_deref(), mime_type));
        let data = base64::engine::general_purpose::STANDARD.encode(bytes);
        return Ok(ProcessedImage { data, mime_type: mime_type.to_string(), hints });
    }

    let resized = resize_image(bytes, mime_type, options.resize.as_ref())
        .ok_or_else(|| "[Image omitted: could not be resized below the inline image size limit.]".to_string())?;
    hints.extend(conversion_hint(converted_from.as_deref(), &resized.mime_type));
    hints.extend(format_dimension_note(&resized));
    Ok(ProcessedImage { data: resized.data, mime_type: resized.mime_type, hints })
}

/// Resize oversized images in tool results (upstream `normalizeToolResultImages`). Images
/// that cannot be processed are kept. Returns `None` when nothing changed.
pub fn normalize_tool_result_images(
    content: &[ContentBlock],
    options: &ProcessImageOptions,
) -> Option<Vec<ContentBlock>> {
    if !content.iter().any(|block| matches!(block, ContentBlock::Image { .. })) {
        return None;
    }
    let mut normalized = Vec::with_capacity(content.len());
    let mut changed = false;
    for block in content {
        let ContentBlock::Image { data, mime_type } = block else {
            normalized.push(block.clone());
            continue;
        };
        let decoded = decode_base64_lenient(data);
        let processed = match process_image(&decoded, mime_type, options) {
            Ok(processed)
                if processed.data != *data || processed.mime_type != *mime_type || !processed.hints.is_empty() =>
            {
                processed
            }
            _ => {
                normalized.push(block.clone());
                continue;
            }
        };
        normalized.push(ContentBlock::Image { data: processed.data, mime_type: processed.mime_type });
        if !processed.hints.is_empty() {
            normalized.push(ContentBlock::text(processed.hints.join("\n")));
        }
        changed = true;
    }
    changed.then_some(normalized)
}
