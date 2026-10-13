//! EXIF orientation of JPEG and WebP files, read from the bytes without decoding the image.

/// The EXIF orientation of a JPEG or WebP, 1 to 8; 1 when absent or unreadable.
pub fn exif_orientation(bytes: &[u8]) -> u8 {
    let Some(tiff) = tiff_start(bytes) else { return 1 };
    if tiff + 8 > bytes.len() {
        return 1;
    }
    let little = bytes[tiff] == 0x49 && bytes[tiff + 1] == 0x49;
    let u16_at = |at: usize| -> Option<u16> {
        let pair = [*bytes.get(at)?, *bytes.get(at + 1)?];
        Some(if little { u16::from_le_bytes(pair) } else { u16::from_be_bytes(pair) })
    };
    let u32_at = |at: usize| -> Option<u32> {
        let quad = [*bytes.get(at)?, *bytes.get(at + 1)?, *bytes.get(at + 2)?, *bytes.get(at + 3)?];
        Some(if little { u32::from_le_bytes(quad) } else { u32::from_be_bytes(quad) })
    };
    let Some(directory) = u32_at(tiff + 4).map(|offset| tiff + offset as usize) else { return 1 };
    if directory + 2 > bytes.len() {
        return 1;
    }
    let entries = u16_at(directory).unwrap_or(0);
    for index in 0..usize::from(entries) {
        let entry = directory + 2 + index * 12;
        if entry + 12 > bytes.len() {
            return 1;
        }
        // Tag 0x0112 is Orientation, a SHORT whose value sits in the entry.
        if u16_at(entry) == Some(0x0112) {
            let value = u16_at(entry + 8).unwrap_or(1);
            return if (1..=8).contains(&value) { value as u8 } else { 1 };
        }
    }
    1
}

/// Where the TIFF header of a JPEG's APP1 or a WebP's EXIF chunk starts.
fn tiff_start(bytes: &[u8]) -> Option<usize> {
    if bytes.starts_with(&[0xff, 0xd8]) {
        let mut offset = 2;
        while offset + 4 <= bytes.len() {
            if bytes[offset] != 0xff {
                return None;
            }
            let marker = bytes[offset + 1];
            if marker == 0xff {
                offset += 1;
                continue;
            }
            if marker == 0xe1 && is_exif_header(bytes, offset + 4) {
                return Some(offset + 10);
            }
            offset += 2 + usize::from(u16::from_be_bytes([bytes[offset + 2], bytes[offset + 3]]));
        }
        return None;
    }
    if ascii(bytes, 0, "RIFF") && ascii(bytes, 8, "WEBP") {
        let mut offset = 12;
        while offset + 8 <= bytes.len() {
            let size = u32::from_le_bytes([bytes[offset + 4], bytes[offset + 5], bytes[offset + 6], bytes[offset + 7]]);
            // Some WebP files prefix the TIFF header with "Exif\0\0".
            if ascii(bytes, offset, "EXIF") {
                return Some(if is_exif_header(bytes, offset + 8) { offset + 14 } else { offset + 8 });
            }
            // RIFF chunks are padded to an even size.
            offset += 8 + size as usize + (size % 2) as usize;
        }
    }
    None
}

fn is_exif_header(bytes: &[u8], offset: usize) -> bool {
    ascii(bytes, offset, "Exif") && bytes.get(offset + 4) == Some(&0) && bytes.get(offset + 5) == Some(&0)
}

fn ascii(bytes: &[u8], offset: usize, text: &str) -> bool {
    bytes.get(offset..offset + text.len()) == Some(text.as_bytes())
}
