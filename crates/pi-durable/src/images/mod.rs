//! Image preparation for the model: decoding, EXIF orientation, conversion, and resizing (upstream `images` and the
//! `ImageProcessor` types of `tools/image-processor.ts`).

mod exif;
mod processor;

use async_trait::async_trait;
use base64::Engine;
use serde::{Deserialize, Serialize};

pub use exif::exif_orientation;
pub use processor::{ImageCrateProcessor, create_image_processor};

/// Limits an image the model sees must keep; the defaults are the coding agent's.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageLimits {
    /// Pixels.
    pub max_width: u32,
    /// Pixels.
    pub max_height: u32,
    /// Bytes of base64.
    pub max_bytes: usize,
}

/// 2000 by 2000 pixels and 4.5 MiB of base64, below Anthropic's 5 MB.
pub const DEFAULT_IMAGE_LIMITS: ImageLimits =
    ImageLimits { max_width: 2000, max_height: 2000, max_bytes: 4 * 1024 * 1024 + 512 * 1024 };

/// Width and height in pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageSize {
    pub width: u32,
    pub height: u32,
}

/// The size of a resized image before and after, for the model to map coordinates back.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageResize {
    pub from: ImageSize,
    pub to: ImageSize,
}

/// An image the model can take: a supported format, within the limits, as base64.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PreparedImage {
    pub data: String,
    pub mime_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resized: Option<ImageResize>,
    /// The original format, set when it differs from `mime_type`: BMP becomes PNG, a large PNG may become JPEG.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub converted_from: Option<String>,
}

/// Decodes, orients, resizes, and re-encodes images so the model can take them.
#[async_trait]
pub trait ImageProcessor: Send + Sync {
    /// The image prepared to fit `limits`, or `None` when it cannot be decoded or made to fit.
    async fn prepare(&self, bytes: &[u8], mime_type: &str, limits: ImageLimits) -> Option<PreparedImage>;
}

/// Formats every provider takes inline; others, such as BMP, need converting.
pub const INLINE_IMAGE_TYPES: [&str; 4] = ["image/png", "image/jpeg", "image/gif", "image/webp"];

/// Standard base64 of `bytes`.
pub fn to_base64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// The length of the base64 of `byte_length` bytes.
pub fn base64_length(byte_length: usize) -> usize {
    byte_length.div_ceil(3) * 4
}
