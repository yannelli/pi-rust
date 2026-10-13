//! Fit images inside dimension and encoded-size limits (upstream `image-resize-core.ts`).
//! Within limits the original bytes are kept; otherwise the image is resized with Lanczos3
//! and re-encoded as PNG or JPEG, shrinking quality and then size until it fits.

use std::io::Cursor;

use base64::Engine;
use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::PngEncoder;
use image::imageops::FilterType;
use image::{DynamicImage, ImageEncoder, RgbaImage};
use pi_ai::ImageResizeOptions;

use super::exif::apply_exif_orientation;
use crate::export::json::js_to_fixed;

pub const DEFAULT_MAX_WIDTH: u32 = 2000;
pub const DEFAULT_MAX_HEIGHT: u32 = 2000;
/// 4.5MB of base64 payload, below Anthropic's 5MB limit.
pub const DEFAULT_MAX_BYTES: u64 = 4_718_592;
pub const DEFAULT_JPEG_QUALITY: u8 = 80;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResizedImage {
    /// Base64 image data.
    pub data: String,
    pub mime_type: String,
    pub original_width: u32,
    pub original_height: u32,
    pub width: u32,
    pub height: u32,
    pub was_resized: bool,
}

/// Limits with upstream defaults filled in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResizeLimits {
    pub max_width: u32,
    pub max_height: u32,
    pub max_bytes: u64,
    pub jpeg_quality: u8,
}

impl ResizeLimits {
    pub fn from_options(options: Option<&ImageResizeOptions>) -> Self {
        let options = options.copied().unwrap_or_default();
        Self {
            max_width: options.max_width.unwrap_or(DEFAULT_MAX_WIDTH),
            max_height: options.max_height.unwrap_or(DEFAULT_MAX_HEIGHT),
            max_bytes: options.max_bytes.unwrap_or(DEFAULT_MAX_BYTES),
            jpeg_quality: options.jpeg_quality.unwrap_or(DEFAULT_JPEG_QUALITY),
        }
    }
}

/// Decode with EXIF orientation applied.
pub fn decode_oriented(bytes: &[u8]) -> Option<DynamicImage> {
    let image = image::load_from_memory(bytes).ok()?;
    Some(apply_exif_orientation(image, bytes))
}

pub fn encode_png(image: &RgbaImage) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let encoder = PngEncoder::new(Cursor::new(&mut out));
    encoder.write_image(image.as_raw(), image.width(), image.height(), image::ExtendedColorType::Rgba8).ok()?;
    Some(out)
}

pub fn encode_jpeg(image: &RgbaImage, quality: u8) -> Option<Vec<u8>> {
    let rgb = DynamicImage::ImageRgba8(image.clone()).into_rgb8();
    let mut out = Vec::new();
    let encoder = JpegEncoder::new_with_quality(Cursor::new(&mut out), quality.clamp(1, 100));
    encoder.write_image(rgb.as_raw(), rgb.width(), rgb.height(), image::ExtendedColorType::Rgb8).ok()?;
    Some(out)
}

fn base64_len(bytes: usize) -> u64 {
    bytes.div_ceil(3) as u64 * 4
}

/// JavaScript `Math.round` for non-negative values.
fn js_round(value: f64) -> u32 {
    (value + 0.5).floor() as u32
}

/// Resize to fit `options` (defaults: 2000x2000, 4.5MB of base64, JPEG quality 80).
/// Returns `None` when the image cannot be decoded or cannot get below the byte limit.
pub fn resize_image(input: &[u8], mime_type: &str, options: Option<&ImageResizeOptions>) -> Option<ResizedImage> {
    let limits = ResizeLimits::from_options(options);
    let image = decode_oriented(input)?;
    let (original_width, original_height) = (image.width(), image.height());

    if original_width <= limits.max_width
        && original_height <= limits.max_height
        && base64_len(input.len()) < limits.max_bytes
    {
        let format = mime_type.split('/').nth(1).unwrap_or("png");
        let mime_type = if mime_type.is_empty() { format!("image/{format}") } else { mime_type.to_string() };
        return Some(ResizedImage {
            data: base64::engine::general_purpose::STANDARD.encode(input),
            mime_type,
            original_width,
            original_height,
            width: original_width,
            height: original_height,
            was_resized: false,
        });
    }

    let (mut width, mut height) = (original_width, original_height);
    if width > limits.max_width {
        height = js_round(f64::from(height) * f64::from(limits.max_width) / f64::from(width));
        width = limits.max_width;
    }
    if height > limits.max_height {
        width = js_round(f64::from(width) * f64::from(limits.max_height) / f64::from(height));
        height = limits.max_height;
    }

    let mut qualities = vec![limits.jpeg_quality];
    for quality in [85, 70, 55, 40] {
        if !qualities.contains(&quality) {
            qualities.push(quality);
        }
    }
    let rgba = image.into_rgba8();
    loop {
        let resized = image::imageops::resize(&rgba, width.max(1), height.max(1), FilterType::Lanczos3);
        // Candidates in upstream order (PNG, then JPEG by quality); the first that fits wins.
        let candidates = std::iter::once((encode_png(&resized), "image/png"))
            .chain(qualities.iter().map(|quality| (encode_jpeg(&resized, *quality), "image/jpeg")));
        for (bytes, mime_type) in candidates {
            let Some(bytes) = bytes else { continue };
            if base64_len(bytes.len()) < limits.max_bytes {
                return Some(ResizedImage {
                    data: base64::engine::general_purpose::STANDARD.encode(&bytes),
                    mime_type: mime_type.to_string(),
                    original_width,
                    original_height,
                    width,
                    height,
                    was_resized: true,
                });
            }
        }
        if width == 1 && height == 1 {
            return None;
        }
        let next_width = if width == 1 { 1 } else { ((f64::from(width) * 0.75).floor() as u32).max(1) };
        let next_height = if height == 1 { 1 } else { ((f64::from(height) * 0.75).floor() as u32).max(1) };
        if (next_width, next_height) == (width, height) {
            return None;
        }
        (width, height) = (next_width, next_height);
    }
}

/// Coordinate note for resized images (upstream `formatDimensionNote`).
pub fn format_dimension_note(result: &ResizedImage) -> Option<String> {
    if !result.was_resized {
        return None;
    }
    let scale = js_to_fixed(f64::from(result.original_width) / f64::from(result.width), 2);
    Some(format!(
        "[Image: original {}x{}, displayed at {}x{}. Multiply coordinates by {scale} to map to original image.]",
        result.original_width, result.original_height, result.width, result.height
    ))
}
