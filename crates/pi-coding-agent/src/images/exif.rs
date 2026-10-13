//! EXIF orientation for JPEG and WebP (upstream `utils/exif-orientation.ts`).

use image::DynamicImage;

fn read_u16(bytes: &[u8], at: usize, little_endian: bool) -> Option<u16> {
    let pair = [*bytes.get(at)?, *bytes.get(at + 1)?];
    Some(if little_endian { u16::from_le_bytes(pair) } else { u16::from_be_bytes(pair) })
}

fn read_u32(bytes: &[u8], at: usize, little_endian: bool) -> Option<u32> {
    let quad = [*bytes.get(at)?, *bytes.get(at + 1)?, *bytes.get(at + 2)?, *bytes.get(at + 3)?];
    Some(if little_endian { u32::from_le_bytes(quad) } else { u32::from_be_bytes(quad) })
}

fn read_orientation_from_tiff(bytes: &[u8], tiff_start: usize) -> u16 {
    if tiff_start + 8 > bytes.len() {
        return 1;
    }
    let little_endian = bytes[tiff_start] == 0x49 && bytes[tiff_start + 1] == 0x49;
    let Some(ifd_offset) = read_u32(bytes, tiff_start + 4, little_endian) else { return 1 };
    let ifd_start = tiff_start + ifd_offset as usize;
    if ifd_start.saturating_add(2) > bytes.len() {
        return 1;
    }
    let entry_count = read_u16(bytes, ifd_start, little_endian).unwrap_or(0);
    for i in 0..usize::from(entry_count) {
        let entry = ifd_start + 2 + i * 12;
        if entry + 12 > bytes.len() {
            return 1;
        }
        if read_u16(bytes, entry, little_endian) == Some(0x0112) {
            let value = read_u16(bytes, entry + 8, little_endian).unwrap_or(1);
            return if (1..=8).contains(&value) { value } else { 1 };
        }
    }
    1
}

fn has_exif_header(bytes: &[u8], at: usize) -> bool {
    bytes.get(at..at + 6) == Some(b"Exif\0\0")
}

fn find_jpeg_tiff_offset(bytes: &[u8]) -> Option<usize> {
    let mut offset = 2;
    while offset + 1 < bytes.len() {
        if bytes[offset] != 0xff {
            return None;
        }
        let marker = bytes[offset + 1];
        if marker == 0xff {
            offset += 1;
            continue;
        }
        if marker == 0xe1 {
            let segment_start = offset + 4;
            if segment_start >= bytes.len() || segment_start + 6 > bytes.len() {
                return None;
            }
            if has_exif_header(bytes, segment_start) {
                return Some(segment_start + 6);
            }
        }
        let length = usize::from(read_u16(bytes, offset + 2, false)?);
        offset += 2 + length;
    }
    None
}

fn find_webp_tiff_offset(bytes: &[u8]) -> Option<usize> {
    let mut offset = 12;
    while offset + 8 <= bytes.len() {
        let chunk_size = read_u32(bytes, offset + 4, true)? as usize;
        let data_start = offset + 8;
        if &bytes[offset..offset + 4] == b"EXIF" {
            if data_start.saturating_add(chunk_size) > bytes.len() {
                return None;
            }
            let has_prefix = chunk_size >= 6 && has_exif_header(bytes, data_start);
            return Some(if has_prefix { data_start + 6 } else { data_start });
        }
        // RIFF chunks are padded to an even size.
        offset = data_start.saturating_add(chunk_size).saturating_add(chunk_size % 2);
    }
    None
}

/// The EXIF orientation tag (1 to 8) of a JPEG or WebP file; 1 when absent.
pub fn exif_orientation(bytes: &[u8]) -> u16 {
    let tiff_offset = if bytes.starts_with(&[0xff, 0xd8]) {
        find_jpeg_tiff_offset(bytes)
    } else if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        find_webp_tiff_offset(bytes)
    } else {
        None
    };
    tiff_offset.map_or(1, |offset| read_orientation_from_tiff(bytes, offset))
}

/// Rotate or flip the decoded image so it displays upright.
pub fn apply_exif_orientation(image: DynamicImage, original_bytes: &[u8]) -> DynamicImage {
    match exif_orientation(original_bytes) {
        2 => image.fliph(),
        3 => image.rotate180(),
        4 => image.flipv(),
        5 => image.rotate90().fliph(),
        6 => image.rotate90(),
        7 => image.rotate270().fliph(),
        8 => image.rotate270(),
        _ => image,
    }
}
