//! Image format sniffing for `read` (upstream `tools/image.ts`): the supported MIME type of a file's header and the
//! dimensions an image declares, without decoding it.

use async_trait::async_trait;

use crate::errors::DurableResult;
use crate::images::ImageSize;

const PNG_SIGNATURE: [u8; 8] = [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];
/// Bytes every check except the APNG chunk walk needs: BMP reads up to offset 29.
const HEADER_BYTES: u64 = 32;
const BLOCK_BYTES: u64 = 64 * 1024;

/// Positional reads of a file of `size()` bytes.
#[async_trait]
pub trait ByteSource: Send + Sync {
    fn size(&self) -> u64;
    async fn read(&self, offset: u64, length: u64) -> DurableResult<Vec<u8>>;
}

/// `detect_supported_image_mime_type` of a whole file, reading only its header and, for PNG, the chunk headers up to
/// the first `acTL` or `IDAT`.
pub async fn detect_supported_image_mime_type_of(source: &dyn ByteSource) -> DurableResult<Option<&'static str>> {
    let header = source.read(0, HEADER_BYTES).await?;
    if !header.starts_with(&PNG_SIGNATURE) {
        return Ok(detect_supported_image_mime_type(&header));
    }
    Ok((is_png(&header) && !is_animated_png_of(source).await?).then_some("image/png"))
}

/// `is_animated_png` over a file read in blocks.
async fn is_animated_png_of(source: &dyn ByteSource) -> DurableResult<bool> {
    let mut block: Vec<u8> = Vec::new();
    let mut block_start = 0u64;
    let size = source.size();
    let mut offset = PNG_SIGNATURE.len() as u64;
    while offset + 8 <= size {
        if offset < block_start || offset + 8 > block_start + block.len() as u64 {
            block_start = offset;
            block = source.read(offset, BLOCK_BYTES).await?;
        }
        let start = (offset - block_start) as usize;
        let header = &block[start.min(block.len())..(start + 8).min(block.len())];
        let chunk_length = read_u32_be(header, 0) as u64;
        if starts_with_ascii(header, 4, "acTL") {
            return Ok(true);
        }
        if starts_with_ascii(header, 4, "IDAT") {
            return Ok(false);
        }
        let next = offset + 8 + chunk_length + 4;
        if next <= offset || next > size {
            return Ok(false);
        }
        offset = next;
    }
    Ok(false)
}

/// The MIME type of a supported image by its leading bytes: JPEG, still PNG, GIF, WebP, or BMP.
pub fn detect_supported_image_mime_type(buffer: &[u8]) -> Option<&'static str> {
    if buffer.starts_with(&[0xff, 0xd8, 0xff]) {
        return (buffer.get(3) != Some(&0xf7)).then_some("image/jpeg");
    }
    if buffer.starts_with(&PNG_SIGNATURE) {
        return (is_png(buffer) && !is_animated_png(buffer)).then_some("image/png");
    }
    if starts_with_ascii(buffer, 0, "GIF87a") || starts_with_ascii(buffer, 0, "GIF89a") {
        return Some("image/gif");
    }
    if starts_with_ascii(buffer, 0, "RIFF") && starts_with_ascii(buffer, 8, "WEBP") {
        return Some("image/webp");
    }
    if starts_with_ascii(buffer, 0, "BM") && is_bmp(buffer) {
        return Some("image/bmp");
    }
    None
}

fn is_png(buffer: &[u8]) -> bool {
    buffer.len() >= 16 && read_u32_be(buffer, PNG_SIGNATURE.len()) == 13 && starts_with_ascii(buffer, 12, "IHDR")
}

fn is_animated_png(buffer: &[u8]) -> bool {
    let mut offset = PNG_SIGNATURE.len();
    while offset + 8 <= buffer.len() {
        let chunk_length = read_u32_be(buffer, offset) as usize;
        if starts_with_ascii(buffer, offset + 4, "acTL") {
            return true;
        }
        if starts_with_ascii(buffer, offset + 4, "IDAT") {
            return false;
        }
        let next = offset.saturating_add(8 + 4).saturating_add(chunk_length);
        if next <= offset || next > buffer.len() {
            return false;
        }
        offset = next;
    }
    false
}

/// The width and height an image of `mime_type` declares in its header, without decoding it: PNG's IHDR, GIF's
/// screen, WebP's VP8, VP8L, or VP8X chunk, JPEG's first SOF segment. `None` when the header cannot be read.
pub fn image_dimensions(bytes: &[u8], mime_type: &str) -> Option<ImageSize> {
    let size = |width: u32, height: u32| Some(ImageSize { width, height });
    match mime_type {
        "image/png" if bytes.len() >= 24 => size(read_u32_be(bytes, 16), read_u32_be(bytes, 20)),
        "image/gif" if bytes.len() >= 10 => size(read_u16_le(bytes, 6), read_u16_le(bytes, 8)),
        "image/webp" if bytes.len() >= 30 => {
            if starts_with_ascii(bytes, 12, "VP8 ") {
                return size(read_u16_le(bytes, 26) & 0x3fff, read_u16_le(bytes, 28) & 0x3fff);
            }
            if starts_with_ascii(bytes, 12, "VP8L") {
                let bits = read_u32_le(bytes, 21);
                return size((bits & 0x3fff) + 1, ((bits >> 14) & 0x3fff) + 1);
            }
            if starts_with_ascii(bytes, 12, "VP8X") {
                let uint24 = |offset: usize| read_u16_le(bytes, offset) + ((byte(bytes, offset + 2)) << 16);
                return size(uint24(24) + 1, uint24(27) + 1);
            }
            None
        }
        "image/jpeg" => jpeg_dimensions(bytes),
        _ => None,
    }
}

fn jpeg_dimensions(bytes: &[u8]) -> Option<ImageSize> {
    let mut offset = 2;
    while offset + 9 <= bytes.len() {
        if bytes[offset] != 0xff {
            return None;
        }
        let marker = bytes[offset + 1];
        if marker == 0xff {
            offset += 1;
            continue;
        }
        // SOF0 to SOF15, except DHT (C4), JPG (C8), and DAC (CC), which share the range.
        if (0xc0..=0xcf).contains(&marker) && marker != 0xc4 && marker != 0xc8 && marker != 0xcc {
            return Some(ImageSize { width: read_u16_be(bytes, offset + 7), height: read_u16_be(bytes, offset + 5) });
        }
        offset += 2 + read_u16_be(bytes, offset + 2) as usize;
    }
    None
}

fn is_bmp(buffer: &[u8]) -> bool {
    if buffer.len() < 26 {
        return false;
    }
    let declared_file_size = read_u32_le(buffer, 2) as u64;
    let pixel_data_offset = read_u32_le(buffer, 10) as u64;
    let dib_header_size = read_u32_le(buffer, 14) as u64;
    if declared_file_size != 0 && declared_file_size < 26 {
        return false;
    }
    if pixel_data_offset < 14 + dib_header_size {
        return false;
    }
    if declared_file_size != 0 && pixel_data_offset >= declared_file_size {
        return false;
    }
    let (color_planes, bits_per_pixel) = if dib_header_size == 12 {
        (read_u16_le(buffer, 22), read_u16_le(buffer, 24))
    } else if (40..=124).contains(&dib_header_size) {
        if buffer.len() < 30 {
            return false;
        }
        (read_u16_le(buffer, 26), read_u16_le(buffer, 28))
    } else {
        return false;
    };
    color_planes == 1 && [1, 4, 8, 16, 24, 32].contains(&bits_per_pixel)
}

fn byte(buffer: &[u8], offset: usize) -> u32 {
    buffer.get(offset).copied().unwrap_or(0) as u32
}

fn read_u16_le(buffer: &[u8], offset: usize) -> u32 {
    byte(buffer, offset) + (byte(buffer, offset + 1) << 8)
}

fn read_u16_be(buffer: &[u8], offset: usize) -> u32 {
    (byte(buffer, offset) << 8) + byte(buffer, offset + 1)
}

fn read_u32_be(buffer: &[u8], offset: usize) -> u32 {
    (byte(buffer, offset) << 24)
        | (byte(buffer, offset + 1) << 16)
        | (byte(buffer, offset + 2) << 8)
        | byte(buffer, offset + 3)
}

fn read_u32_le(buffer: &[u8], offset: usize) -> u32 {
    byte(buffer, offset)
        | (byte(buffer, offset + 1) << 8)
        | (byte(buffer, offset + 2) << 16)
        | (byte(buffer, offset + 3) << 24)
}

fn starts_with_ascii(buffer: &[u8], offset: usize, text: &str) -> bool {
    buffer.get(offset..offset + text.len()) == Some(text.as_bytes())
}
