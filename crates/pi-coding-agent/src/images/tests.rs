use std::sync::Arc;

use base64::Engine;
use image::{Rgba, RgbaImage};
use pi_agent::AgentTool;
use pi_ai::{ContentBlock, ImageResizeOptions, Message, ToolResultMessage, UserContent, UserMessage};
use serde_json::json;
use tokio_util::sync::CancellationToken;

use super::exif::exif_orientation;
use super::*;
use crate::tools::ReadTool;

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn unb64(data: &str) -> Vec<u8> {
    base64::engine::general_purpose::STANDARD.decode(data).unwrap()
}

fn gradient(width: u32, height: u32) -> RgbaImage {
    RgbaImage::from_fn(width, height, |x, y| Rgba([(x % 256) as u8, (y % 256) as u8, 90, 255]))
}

/// Deterministic noise, which compresses badly in PNG.
fn noise(width: u32, height: u32) -> RgbaImage {
    let mut state = 0x2545_f491_u32;
    RgbaImage::from_fn(width, height, |_, _| {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        let [r, g, b, _] = state.to_le_bytes();
        Rgba([r, g, b, 255])
    })
}

fn dims(data: &str) -> (u32, u32) {
    let image = image::load_from_memory(&unb64(data)).unwrap();
    (image.width(), image.height())
}

fn limits(max_width: u32, max_height: u32, max_bytes: u64) -> ImageResizeOptions {
    ImageResizeOptions {
        max_width: Some(max_width),
        max_height: Some(max_height),
        max_bytes: Some(max_bytes),
        jpeg_quality: None,
    }
}

#[test]
fn keeps_images_within_limits() {
    let png = encode_png(&gradient(2, 2)).unwrap();
    let result = resize_image(&png, "image/png", Some(&limits(100, 100, 1 << 20))).unwrap();
    assert!(!result.was_resized);
    assert_eq!(result.data, b64(&png));
    assert_eq!((result.original_width, result.width, result.height), (2, 2, 2));
    assert_eq!(format_dimension_note(&result), None);

    let jpeg = encode_jpeg(&gradient(2, 2), 90).unwrap();
    let result = resize_image(&jpeg, "image/jpeg", Some(&limits(100, 100, 1 << 20))).unwrap();
    assert_eq!((result.was_resized, result.mime_type.as_str()), (false, "image/jpeg"));
}

#[test]
fn resizes_to_the_default_2000_box() {
    let png = encode_png(&gradient(3000, 45)).unwrap();
    let result = resize_image(&png, "image/png", None).unwrap();
    assert!(result.was_resized);
    assert_eq!((result.width, result.height), (2000, 30));
    assert_eq!(dims(&result.data), (2000, 30));
    assert_eq!(
        format_dimension_note(&result).unwrap(),
        "[Image: original 3000x45, displayed at 2000x30. Multiply coordinates by 1.50 to map to original image.]"
    );
    let tall = encode_png(&gradient(10, 2250)).unwrap();
    let result = resize_image(&tall, "image/png", None).unwrap();
    assert_eq!((result.width, result.height), (9, 2000));
}

#[test]
fn reencodes_as_jpeg_with_decreasing_quality_to_fit_bytes() {
    let image = noise(120, 120);
    let png = encode_png(&image).unwrap();
    let jpeg_80 = encode_jpeg(&image, 80).unwrap();
    let jpeg_55 = encode_jpeg(&image, 55).unwrap();
    let encoded = |bytes: &[u8]| bytes.len().div_ceil(3) as u64 * 4;
    assert!(encoded(&png) > encoded(&jpeg_80) && encoded(&jpeg_80) > encoded(&jpeg_55));

    let result = resize_image(&png, "image/png", Some(&limits(2000, 2000, encoded(&jpeg_80) + 1))).unwrap();
    assert_eq!((result.mime_type.as_str(), result.width, result.was_resized), ("image/jpeg", 120, true));
    assert_eq!(unb64(&result.data), jpeg_80);

    let result = resize_image(&png, "image/png", Some(&limits(2000, 2000, encoded(&jpeg_55) + 1))).unwrap();
    assert_eq!(unb64(&result.data), jpeg_55);

    // A quality setting goes first; then the size shrinks by 3/4 steps.
    let options = ImageResizeOptions { jpeg_quality: Some(30), ..limits(2000, 2000, encoded(&jpeg_55) + 1) };
    let result = resize_image(&png, "image/png", Some(&options)).unwrap();
    assert_eq!(unb64(&result.data), encode_jpeg(&image, 30).unwrap());
    let result = resize_image(&png, "image/png", Some(&limits(2000, 2000, 2000))).unwrap();
    assert!(result.width < 120 && result.width == result.height);
    assert!(resize_image(&png, "image/png", Some(&limits(2000, 2000, 1))).is_none());
}

/// A JPEG with an XMP APP1 segment, then an EXIF APP1 segment carrying `orientation`.
fn jpeg_with_orientation(image: &RgbaImage, orientation: u8, big_endian: bool) -> Vec<u8> {
    let jpeg = encode_jpeg(image, 95).unwrap();
    let app1 = |payload: &[u8]| {
        let mut segment = vec![0xff, 0xe1];
        segment.extend_from_slice(&((payload.len() + 2) as u16).to_be_bytes());
        segment.extend_from_slice(payload);
        segment
    };
    let xmp = app1(b"http://ns.adobe.com/xap/1.0/\0<x:xmpmeta xmlns:x=\"adobe:ns:meta/\"/>");
    let tiff: Vec<u8> = if big_endian {
        [b"MM\0\x2a\0\0\0\x08\0\x01\x01\x12\0\x03\0\0\0\x01\0".as_slice(), &[orientation, 0, 0, 0, 0, 0, 0]].concat()
    } else {
        [b"II\x2a\0\x08\0\0\0\x01\0\x12\x01\x03\0\x01\0\0\0".as_slice(), &[orientation, 0, 0, 0, 0, 0, 0, 0]].concat()
    };
    let exif = app1(&[b"Exif\0\0".as_slice(), &tiff].concat());
    [&jpeg[..2], &xmp, &exif, &jpeg[2..]].concat()
}

fn red_left_blue_right() -> RgbaImage {
    RgbaImage::from_fn(16, 8, |x, _| if x < 8 { Rgba([255, 0, 0, 255]) } else { Rgba([0, 0, 255, 255]) })
}

fn is_red(pixel: &Rgba<u8>) -> bool {
    pixel[0] > 200 && pixel[2] < 60
}

#[test]
fn applies_exif_orientation_before_resizing() {
    let source = red_left_blue_right();
    for (orientation, size, red_at) in
        [(1, (16, 8), (2, 4)), (3, (16, 8), (13, 4)), (6, (8, 16), (4, 2)), (8, (8, 16), (4, 13))]
    {
        for big_endian in [false, true] {
            let bytes = jpeg_with_orientation(&source, orientation, big_endian);
            assert_eq!(exif_orientation(&bytes), u16::from(orientation));
            let png = convert_image_bytes_to_png(&bytes).unwrap();
            let decoded = image::load_from_memory(&png).unwrap().into_rgba8();
            assert_eq!(decoded.dimensions(), size, "orientation {orientation}");
            assert!(is_red(decoded.get_pixel(red_at.0, red_at.1)), "orientation {orientation}");
        }
    }
    // Rotated dimensions are reported even when the original bytes are kept.
    let bytes = jpeg_with_orientation(&source, 6, false);
    let result = resize_image(&bytes, "image/jpeg", None).unwrap();
    assert_eq!((result.width, result.height, result.was_resized), (8, 16, false));
    let result = resize_image(&bytes, "image/jpeg", Some(&limits(4, 100, 1 << 20))).unwrap();
    assert_eq!((result.width, result.height), (4, 8));
}

#[test]
fn reads_webp_exif_chunks() {
    let tiff = b"II\x2a\0\x08\0\0\0\x01\0\x12\x01\x03\0\x01\0\0\0\x08\0\0\0\0\0\0\0";
    let exif = [b"Exif\0\0".as_slice(), tiff].concat();
    let mut webp = b"RIFF\0\0\0\0WEBPVP8X\x0a\0\0\0".to_vec();
    webp.extend_from_slice(&[0; 10]);
    webp.extend_from_slice(b"EXIF");
    webp.extend_from_slice(&(exif.len() as u32).to_le_bytes());
    webp.extend_from_slice(&exif);
    assert_eq!(exif_orientation(&webp), 8);
    assert_eq!(exif_orientation(b"RIFF\0\0\0\0WEBPVP8 \x04\0\0\0abcd"), 1);
    assert_eq!(exif_orientation(&[0xff, 0xd8, 0xff]), 1);
}

#[test]
fn process_image_converts_unsupported_formats() {
    let mut bmp = Vec::new();
    image::DynamicImage::ImageRgba8(gradient(3, 2))
        .write_to(&mut std::io::Cursor::new(&mut bmp), image::ImageFormat::Bmp)
        .unwrap();
    let processed = process_image(&bmp, "image/bmp", &ProcessImageOptions::default()).unwrap();
    assert_eq!(processed.mime_type, "image/png");
    assert_eq!(processed.hints, ["[Image converted from image/bmp to image/png.]"]);
    assert_eq!(dims(&processed.data), (3, 2));

    let png = encode_png(&gradient(300, 10)).unwrap();
    let off = ProcessImageOptions { auto_resize: false, resize: None };
    let processed = process_image(&png, "image/png", &off).unwrap();
    assert_eq!((processed.data, processed.hints.len()), (b64(&png), 0));

    let images = pi_ai::ModelImageInputLimits { resize: Some(limits(100, 100, 1 << 20)), ..Default::default() };
    let input_limits = pi_ai::ModelInputLimits { max_request_bytes: None, images: Some(images) };
    let model = pi_ai::Model { input_limits: Some(input_limits), ..Default::default() };
    let processed = process_image(&png, "image/png", &ProcessImageOptions::for_model(&model, true)).unwrap();
    assert_eq!(dims(&processed.data), (100, 3));
    assert_eq!(
        process_image(b"BMnot really", "image/bmp", &off).unwrap_err(),
        "[Image omitted: could not be converted to a supported inline image format.]"
    );
}

#[test]
fn tool_result_images_are_normalized_once() {
    let small = ContentBlock::Image { data: b64(&encode_png(&gradient(2, 2)).unwrap()), mime_type: "image/png".into() };
    let options = ProcessImageOptions::default();
    assert_eq!(normalize_tool_result_images(&[ContentBlock::text("x"), small.clone()], &options), None);
    let broken = ContentBlock::Image { data: "AAAA".into(), mime_type: "image/png".into() };
    assert_eq!(normalize_tool_result_images(&[broken], &options), None);

    let large =
        ContentBlock::Image { data: b64(&encode_png(&gradient(2400, 12)).unwrap()), mime_type: "image/png".into() };
    let normalized =
        normalize_tool_result_images(&[ContentBlock::text("shot"), large, small.clone()], &options).unwrap();
    assert_eq!(normalized.len(), 4);
    let ContentBlock::Image { data, .. } = &normalized[1] else { panic!("expected an image") };
    assert_eq!(dims(data), (2000, 10));
    assert!(normalized[2].as_text().unwrap().starts_with("[Image: original 2400x12, displayed at 2000x10."));
    assert_eq!(normalized[3], small);
}

#[test]
fn base64_decoding_is_lenient_like_node() {
    assert_eq!(decode_base64_lenient("aGVsbG8="), b"hello");
    assert_eq!(decode_base64_lenient("aGVs\nbG8"), b"hello");
    assert_eq!(decode_base64_lenient("-_-_"), [0xfb, 0xff, 0xbf]);
    assert_eq!(decode_base64_lenient("aGk=ignored"), b"hi");
    let png = encode_png(&gradient(2, 2)).unwrap();
    let (data, mime_type) = convert_to_png(&b64(&png), "image/png").unwrap();
    assert_eq!((data, mime_type.as_str()), (b64(&png), "image/png"));
    let jpeg = encode_jpeg(&gradient(2, 2), 90).unwrap();
    let (data, mime_type) = convert_to_png(&b64(&jpeg), "image/jpeg").unwrap();
    assert_eq!((dims(&data), mime_type.as_str()), ((2, 2), "image/png"));
}

#[test]
fn block_images_replaces_and_dedupes_placeholders() {
    let image = || ContentBlock::Image { data: "x".into(), mime_type: "image/png".into() };
    let user = Message::User(UserMessage {
        content: UserContent::Blocks(vec![ContentBlock::text("look"), image(), image(), ContentBlock::text("end")]),
        timestamp: 1,
    });
    let result = Message::ToolResult(ToolResultMessage { content: vec![image()], ..Default::default() });
    let plain = Message::User(UserMessage { content: UserContent::Text("hi".into()), timestamp: 2 });
    let blocked = block_images(vec![user, result, plain.clone()]);
    let Message::User(user) = &blocked[0] else { panic!() };
    let placeholder = ContentBlock::text(BLOCKED_IMAGE_TEXT);
    let expected = vec![ContentBlock::text("look"), placeholder.clone(), ContentBlock::text("end")];
    assert_eq!(user.content, UserContent::Blocks(expected));
    let Message::ToolResult(result) = &blocked[1] else { panic!() };
    assert_eq!(result.content, [placeholder]);
    assert_eq!(blocked[2], plain);
}

async fn read(tool: &ReadTool, path: &str) -> pi_agent::AgentToolResult {
    tool.execute("call", json!({ "path": path }), CancellationToken::new(), Arc::new(|_| {})).await.unwrap()
}

#[tokio::test]
async fn read_tool_resizes_large_images() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("wide.png"), encode_png(&gradient(2400, 30)).unwrap()).unwrap();
    let result = read(&ReadTool::new(dir.path().to_path_buf()), "wide.png").await;
    assert_eq!(
        result.content[0].as_text().unwrap(),
        "Read image file [image/png]\n[Image: original 2400x30, displayed at 2000x25. Multiply coordinates by 1.20 to map to original image.]"
    );
    let ContentBlock::Image { data, .. } = &result.content[1] else { panic!("expected an image") };
    assert_eq!(dims(data), (2000, 25));

    let options = ProcessImageOptions { auto_resize: true, resize: Some(limits(240, 240, 1 << 20)) };
    let result = read(&ReadTool::with_image_options(dir.path().to_path_buf(), options), "wide.png").await;
    let ContentBlock::Image { data, .. } = &result.content[1] else { panic!("expected an image") };
    assert_eq!(dims(data), (240, 3));

    let images = pi_ai::ModelImageInputLimits { resize: Some(limits(600, 600, 1 << 20)), ..Default::default() };
    let input_limits = pi_ai::ModelInputLimits { max_request_bytes: None, images: Some(images) };
    let text_only = pi_ai::Model { input: vec!["text".into()], input_limits: Some(input_limits), ..Default::default() };
    let result = read(&ReadTool::for_model(dir.path().to_path_buf(), &text_only, true), "wide.png").await;
    assert!(result.content[0].as_text().unwrap().ends_with(
        "Multiply coordinates by 4.00 to map to original image.]\n[Current model does not support images. The image will be omitted from this request.]"
    ));
    let ContentBlock::Image { data, .. } = &result.content[1] else { panic!("expected an image") };
    assert_eq!(dims(data), (600, 8));

    let mut broken = encode_png(&gradient(4, 4)).unwrap();
    broken.truncate(40);
    std::fs::write(dir.path().join("broken.png"), broken).unwrap();
    let result = read(&ReadTool::new(dir.path().to_path_buf()), "broken.png").await;
    assert_eq!(result.content.len(), 1);
    assert_eq!(
        result.content[0].as_text().unwrap(),
        "Read image file [image/png]\n[Image omitted: could not be resized below the inline image size limit.]"
    );
}
