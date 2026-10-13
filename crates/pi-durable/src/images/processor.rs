//! An `ImageProcessor` on the pure-Rust `image` crate, in place of upstream's Photon in WebAssembly.

use std::io::Cursor;

use async_trait::async_trait;
use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::PngEncoder;
use image::imageops::FilterType;
use image::metadata::Orientation;
use image::{DynamicImage, ImageEncoder, RgbaImage};

use super::{
    INLINE_IMAGE_TYPES, ImageLimits, ImageProcessor, ImageResize, ImageSize, PreparedImage, base64_length,
    exif_orientation, to_base64,
};

/// JPEG qualities tried in order.
const JPEG_QUALITIES: [u8; 4] = [80, 70, 55, 40];

/// Prepares images with the `image` crate. An image already upright, inline, and within the limits is returned as it
/// is once it decodes. Otherwise it is turned upright, scaled to fit, and encoded as PNG or JPEG at falling qualities
/// (JPEG first for JPEG sources), then at 3/4 of the size at a time until the base64 fits. BMP becomes PNG.
#[derive(Clone, Copy, Debug, Default)]
pub struct ImageCrateProcessor;

/// The image processor of this crate; the work runs on tokio's blocking pool.
pub fn create_image_processor() -> ImageCrateProcessor {
    ImageCrateProcessor
}

#[async_trait]
impl ImageProcessor for ImageCrateProcessor {
    async fn prepare(&self, bytes: &[u8], mime_type: &str, limits: ImageLimits) -> Option<PreparedImage> {
        let (bytes, mime_type) = (bytes.to_vec(), mime_type.to_string());
        tokio::task::spawn_blocking(move || prepare(&bytes, &mime_type, limits)).await.ok().flatten()
    }
}

/// Prepare an image on the calling thread.
pub(crate) fn prepare(bytes: &[u8], mime_type: &str, limits: ImageLimits) -> Option<PreparedImage> {
    let orientation = exif_orientation(bytes);
    let inline = INLINE_IMAGE_TYPES.contains(&mime_type);
    // Decoded even when it is returned as it is: a damaged file is an error, not an image the provider rejects.
    let mut image = image::load_from_memory(bytes).ok()?;
    if inline
        && orientation == 1
        && image.width() <= limits.max_width
        && image.height() <= limits.max_height
        && base64_length(bytes.len()) <= limits.max_bytes
    {
        return Some(PreparedImage {
            data: to_base64(bytes),
            mime_type: mime_type.to_string(),
            resized: None,
            converted_from: None,
        });
    }
    if let Some(orientation) = Orientation::from_exif(orientation) {
        image.apply_orientation(orientation);
    }
    let image = image.into_rgba8();
    let (width, height) = image.dimensions();
    let scale =
        1f64.min(f64::from(limits.max_width) / f64::from(width)).min(f64::from(limits.max_height) / f64::from(height));
    let scaled = |side: u32| ((f64::from(side) * scale).round() as u32).max(1);
    let mut target = ImageSize { width: scaled(width), height: scaled(height) };
    loop {
        if let Some((data, encoded_type)) = encode_within(&image, target, limits.max_bytes, mime_type == "image/jpeg") {
            let resized = (target.width != width || target.height != height)
                .then_some(ImageResize { from: ImageSize { width, height }, to: target });
            let converted_from = (encoded_type != mime_type).then(|| mime_type.to_string());
            return Some(PreparedImage { data, mime_type: encoded_type.to_string(), resized, converted_from });
        }
        if target.width == 1 && target.height == 1 {
            return None;
        }
        target = ImageSize { width: (target.width * 3 / 4).max(1), height: (target.height * 3 / 4).max(1) };
    }
}

fn png(image: &RgbaImage) -> Option<Vec<u8>> {
    let mut bytes = Vec::new();
    let encoder = PngEncoder::new(&mut bytes);
    encoder.write_image(image.as_raw(), image.width(), image.height(), image::ExtendedColorType::Rgba8).ok()?;
    Some(bytes)
}

fn jpeg(image: &RgbaImage, quality: u8) -> Option<Vec<u8>> {
    let rgb = DynamicImage::ImageRgba8(image.clone()).into_rgb8();
    let mut bytes = Cursor::new(Vec::new());
    let encoder = JpegEncoder::new_with_quality(&mut bytes, quality);
    encoder.write_image(rgb.as_raw(), rgb.width(), rgb.height(), image::ExtendedColorType::Rgb8).ok()?;
    Some(bytes.into_inner())
}

/// `image` at `size` in the first encoding whose base64 fits `max_bytes`: PNG, then JPEG at falling qualities, or the
/// JPEG qualities first when `jpeg_first`.
fn encode_within(
    image: &RgbaImage,
    size: ImageSize,
    max_bytes: usize,
    jpeg_first: bool,
) -> Option<(String, &'static str)> {
    let resized;
    let scaled = if size.width == image.width() && size.height == image.height() {
        image
    } else {
        resized = image::imageops::resize(image, size.width, size.height, FilterType::Lanczos3);
        &resized
    };
    let mut candidates: Vec<Option<u8>> = JPEG_QUALITIES.iter().copied().map(Some).collect();
    if jpeg_first {
        candidates.push(None);
    } else {
        candidates.insert(0, None);
    }
    for candidate in candidates {
        let (bytes, mime_type) = match candidate {
            None => (png(scaled)?, "image/png"),
            Some(quality) => (jpeg(scaled, quality)?, "image/jpeg"),
        };
        if base64_length(bytes.len()) <= max_bytes {
            return Some((to_base64(&bytes), mime_type));
        }
    }
    None
}
