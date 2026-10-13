//! Image fixtures of the image processor tests.
#![allow(dead_code)]

use base64::Engine;
use image::{ExtendedColorType, ImageEncoder, RgbaImage};

pub type Rgb = [u8; 3];

#[derive(Clone, Copy)]
pub enum Format {
    Png,
    Jpeg,
}

const R: Rgb = [255, 0, 0];
const G: Rgb = [0, 255, 0];
const B: Rgb = [0, 0, 255];
const W: Rgb = [255, 255, 255];
const K: Rgb = [0, 0, 0];
const Y: Rgb = [255, 255, 0];
pub const STORED: [[Rgb; 3]; 2] = [[R, G, B], [W, K, Y]];

/// RGBA pixels of `block`-pixel squares coloured row by row from `colors`.
pub fn blocks(colors: &[[Rgb; 3]], block: u32) -> (Vec<u8>, u32, u32) {
    let (width, height) = (3 * block, colors.len() as u32 * block);
    let mut rgba = Vec::with_capacity((width * height * 4) as usize);
    for y in 0..height {
        for x in 0..width {
            rgba.extend_from_slice(&colors[(y / block) as usize][(x / block) as usize]);
            rgba.push(255);
        }
    }
    (rgba, width, height)
}

pub fn mirror(grid: &[Vec<Rgb>]) -> Vec<Vec<Rgb>> {
    grid.iter().map(|row| row.iter().rev().copied().collect()).collect()
}

/// A quarter turn clockwise: the left column, read bottom to top, becomes the top row.
pub fn turn(grid: &[Vec<Rgb>]) -> Vec<Vec<Rgb>> {
    (0..grid[0].len()).map(|x| grid.iter().rev().map(|row| row[x]).collect()).collect()
}

/// What each orientation shows, from its EXIF definition: mirror, then turn clockwise.
pub fn upright(orientation: u16) -> Vec<Vec<Rgb>> {
    let stored: Vec<Vec<Rgb>> = STORED.iter().map(|row| row.to_vec()).collect();
    let turns = |grid: Vec<Vec<Rgb>>, count| (0..count).fold(grid, |grid, _| turn(&grid));
    match orientation {
        1 => stored,
        2 => mirror(&stored),
        3 => turns(stored, 2),
        4 => turns(mirror(&stored), 2),
        5 => turns(mirror(&stored), 3),
        6 => turns(stored, 1),
        7 => turns(mirror(&stored), 1),
        _ => turns(stored, 3),
    }
}

pub fn encode_rgba(rgba: &[u8], width: u32, height: u32, format: Format) -> Vec<u8> {
    let mut bytes = Vec::new();
    match format {
        Format::Png => image::codecs::png::PngEncoder::new(&mut bytes)
            .write_image(rgba, width, height, ExtendedColorType::Rgba8)
            .unwrap(),
        Format::Jpeg => {
            let rgb: Vec<u8> = rgba.chunks(4).flat_map(|pixel| pixel[..3].to_vec()).collect();
            image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, 95)
                .write_image(&rgb, width, height, ExtendedColorType::Rgb8)
                .unwrap();
        }
    }
    bytes
}

/// Red, with a green top-left pixel.
pub fn encode(width: u32, height: u32, format: Format) -> Vec<u8> {
    let mut rgba: Vec<u8> = (0..width * height).flat_map(|_| [255, 0, 0, 255]).collect();
    rgba[..4].copy_from_slice(&[0, 255, 0, 255]);
    encode_rgba(&rgba, width, height, format)
}

/// Noise, which neither PNG nor JPEG compresses well, so the byte limit decides.
pub fn noisy_png(width: u32, height: u32) -> Vec<u8> {
    let mut seed: u32 = 1;
    let rgba: Vec<u8> = (0..(width * height * 4) as usize)
        .map(|index| {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
            if index % 4 == 3 { 255 } else { (seed >> 24) as u8 }
        })
        .collect();
    encode_rgba(&rgba, width, height, Format::Png)
}

/// A 24-bit BMP, which providers do not take inline.
pub fn bmp(width: u32, height: u32) -> Vec<u8> {
    let row = (width * 3).div_ceil(4) * 4;
    let mut bytes = vec![0u8; (54 + row * height) as usize];
    let total = bytes.len() as u32;
    bytes[..2].copy_from_slice(b"BM");
    bytes[2..6].copy_from_slice(&total.to_le_bytes());
    bytes[10..14].copy_from_slice(&54u32.to_le_bytes());
    bytes[14..18].copy_from_slice(&40u32.to_le_bytes());
    bytes[18..22].copy_from_slice(&(width as i32).to_le_bytes());
    bytes[22..26].copy_from_slice(&(height as i32).to_le_bytes());
    bytes[26..28].copy_from_slice(&1u16.to_le_bytes());
    bytes[28..30].copy_from_slice(&24u16.to_le_bytes());
    bytes[34..38].copy_from_slice(&(row * height).to_le_bytes());
    for y in 0..height {
        for x in 0..width {
            let at = (54 + y * row + x * 3) as usize;
            bytes[at..at + 3].copy_from_slice(&[0, 0, 255]);
        }
    }
    bytes
}

/// A 1x1 GIF with its logical screen size set to `width` by `height`.
pub fn gif(width: u16, height: u16) -> Vec<u8> {
    let data = "R0lGODlhAQABAIAAAAAAAP///yH5BAEAAAAALAAAAAABAAEAAAIBRAA7";
    let mut bytes = base64::engine::general_purpose::STANDARD.decode(data).unwrap();
    bytes[6..8].copy_from_slice(&width.to_le_bytes());
    bytes[8..10].copy_from_slice(&height.to_le_bytes());
    bytes
}

/// A TIFF header whose first directory holds only Orientation = `orientation`.
pub fn tiff(orientation: u16, little: bool) -> Vec<u8> {
    let u16b = |value: u16| if little { value.to_le_bytes() } else { value.to_be_bytes() };
    let u32b = |value: u32| if little { value.to_le_bytes() } else { value.to_be_bytes() };
    let mut bytes = if little { b"II".to_vec() } else { b"MM".to_vec() };
    bytes.extend(u16b(42));
    bytes.extend(u32b(8));
    bytes.extend(u16b(1));
    bytes.extend(u16b(0x0112));
    bytes.extend(u16b(3));
    bytes.extend(u32b(1));
    bytes.extend(u16b(orientation));
    bytes.resize(26, 0);
    bytes
}

const EXIF_HEADER: [u8; 6] = [0x45, 0x78, 0x69, 0x66, 0, 0];

/// `jpeg` with an APP1 segment whose EXIF says `orientation`, after an APP0 segment when `app0`.
pub fn with_orientation(jpeg: &[u8], orientation: u16, little: bool, app0: bool) -> Vec<u8> {
    let exif: Vec<u8> = EXIF_HEADER.iter().copied().chain(tiff(orientation, little)).collect();
    let length = (exif.len() + 2) as u16;
    let mut bytes = jpeg[..2].to_vec();
    if app0 {
        bytes.extend([0xff, 0xe0, 0, 16, 0x4a, 0x46, 0x49, 0x46, 0, 1, 1, 0, 0, 1, 0, 1, 0, 0]);
    }
    bytes.extend([0xff, 0xe1]);
    bytes.extend(length.to_be_bytes());
    bytes.extend(exif);
    bytes.extend_from_slice(&jpeg[2..]);
    bytes
}

/// A RIFF WebP with a dummy `VP8 ` chunk and an `EXIF` chunk saying `orientation`, its TIFF data bare or prefixed.
pub fn webp_with_orientation(orientation: u16, prefixed: bool) -> Vec<u8> {
    let chunk = |kind: &[u8], data: &[u8]| {
        let mut bytes = kind.to_vec();
        bytes.extend((data.len() as u32).to_le_bytes());
        bytes.extend_from_slice(data);
        if data.len() % 2 == 1 {
            bytes.push(0);
        }
        bytes
    };
    let mut exif = if prefixed { EXIF_HEADER.to_vec() } else { Vec::new() };
    exif.extend(tiff(orientation, true));
    let mut body = b"WEBP".to_vec();
    body.extend(chunk(b"VP8 ", &[1, 2, 3]));
    body.extend(chunk(b"EXIF", &exif));
    let mut bytes = b"RIFF".to_vec();
    bytes.extend((body.len() as u32).to_le_bytes());
    bytes.extend(body);
    bytes
}

pub fn decoded(data: &str) -> RgbaImage {
    let bytes = base64::engine::general_purpose::STANDARD.decode(data).unwrap();
    image::load_from_memory(&bytes).unwrap().into_rgba8()
}
