//! Image size sniffing from base64 PNG, JPEG, GIF, and WebP headers.

use super::ImageDimensions;

fn base64_value(byte: u8) -> Option<u32> {
    match byte {
        b'A'..=b'Z' => Some(u32::from(byte - b'A')),
        b'a'..=b'z' => Some(u32::from(byte - b'a') + 26),
        b'0'..=b'9' => Some(u32::from(byte - b'0') + 52),
        b'+' | b'-' => Some(62),
        b'/' | b'_' => Some(63),
        _ => None,
    }
}

/// Decodes base64 as leniently as Node's `Buffer.from(data, "base64")`: the URL-safe alphabet is
/// accepted, other characters are skipped, decoding stops at `=`, and missing padding is fine.
pub(crate) fn decode_base64_lenient(data: &str) -> Vec<u8> {
    let mut output = Vec::with_capacity(data.len() / 4 * 3 + 3);
    let mut buffer = 0u32;
    let mut bits = 0u32;
    for byte in data.bytes() {
        if byte == b'=' {
            break;
        }
        let Some(value) = base64_value(byte) else {
            continue;
        };
        buffer = (buffer << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            output.push((buffer >> bits) as u8);
            buffer &= (1 << bits) - 1;
        }
    }
    output
}

fn u16_be(buffer: &[u8], offset: usize) -> u32 {
    u32::from(u16::from_be_bytes([buffer[offset], buffer[offset + 1]]))
}

fn u16_le(buffer: &[u8], offset: usize) -> u32 {
    u32::from(u16::from_le_bytes([buffer[offset], buffer[offset + 1]]))
}

fn u32_be(buffer: &[u8], offset: usize) -> u32 {
    u32::from_be_bytes([buffer[offset], buffer[offset + 1], buffer[offset + 2], buffer[offset + 3]])
}

fn u32_le(buffer: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([buffer[offset], buffer[offset + 1], buffer[offset + 2], buffer[offset + 3]])
}

fn u24_le(buffer: &[u8], offset: usize) -> u32 {
    u32::from(buffer[offset]) | (u32::from(buffer[offset + 1]) << 8) | (u32::from(buffer[offset + 2]) << 16)
}

/// The size in a PNG's IHDR chunk.
pub fn get_png_dimensions(base64_data: &str) -> Option<ImageDimensions> {
    let buffer = decode_base64_lenient(base64_data);
    if buffer.len() < 24 || buffer[..4] != [0x89, 0x50, 0x4e, 0x47] {
        return None;
    }
    Some(ImageDimensions { width_px: u32_be(&buffer, 16), height_px: u32_be(&buffer, 20) })
}

/// The size in a JPEG's first SOF0-SOF2 frame header.
pub fn get_jpeg_dimensions(base64_data: &str) -> Option<ImageDimensions> {
    let buffer = decode_base64_lenient(base64_data);
    if buffer.len() < 2 || buffer[0] != 0xff || buffer[1] != 0xd8 {
        return None;
    }
    let mut offset = 2;
    while offset + 9 < buffer.len() {
        if buffer[offset] != 0xff {
            offset += 1;
            continue;
        }
        let marker = buffer[offset + 1];
        if (0xc0..=0xc2).contains(&marker) {
            return Some(ImageDimensions {
                width_px: u16_be(&buffer, offset + 7),
                height_px: u16_be(&buffer, offset + 5),
            });
        }
        let length = u16_be(&buffer, offset + 2) as usize;
        if length < 2 {
            return None;
        }
        offset += 2 + length;
    }
    None
}

/// The logical screen size in a GIF header.
pub fn get_gif_dimensions(base64_data: &str) -> Option<ImageDimensions> {
    let buffer = decode_base64_lenient(base64_data);
    if buffer.len() < 10 || (&buffer[..6] != b"GIF87a" && &buffer[..6] != b"GIF89a") {
        return None;
    }
    Some(ImageDimensions { width_px: u16_le(&buffer, 6), height_px: u16_le(&buffer, 8) })
}

/// The canvas size in a WebP `VP8 `, `VP8L`, or `VP8X` header.
pub fn get_webp_dimensions(base64_data: &str) -> Option<ImageDimensions> {
    let buffer = decode_base64_lenient(base64_data);
    if buffer.len() < 30 || &buffer[..4] != b"RIFF" || &buffer[8..12] != b"WEBP" {
        return None;
    }
    match &buffer[12..16] {
        b"VP8 " => {
            Some(ImageDimensions { width_px: u16_le(&buffer, 26) & 0x3fff, height_px: u16_le(&buffer, 28) & 0x3fff })
        }
        b"VP8L" => {
            let bits = u32_le(&buffer, 21);
            Some(ImageDimensions { width_px: (bits & 0x3fff) + 1, height_px: ((bits >> 14) & 0x3fff) + 1 })
        }
        b"VP8X" => Some(ImageDimensions { width_px: u24_le(&buffer, 24) + 1, height_px: u24_le(&buffer, 27) + 1 }),
        _ => None,
    }
}

/// The size of base64 image data of the given MIME type (`image/png`, `image/jpeg`, `image/gif`, `image/webp`).
pub fn get_image_dimensions(base64_data: &str, mime_type: &str) -> Option<ImageDimensions> {
    match mime_type {
        "image/png" => get_png_dimensions(base64_data),
        "image/jpeg" => get_jpeg_dimensions(base64_data),
        "image/gif" => get_gif_dimensions(base64_data),
        "image/webp" => get_webp_dimensions(base64_data),
        _ => None,
    }
}
