//! The `read` tool and `imageDimensions` cases of upstream `test/tools-images.test.ts`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use pi_ai::{ContentBlock, ImageResizeOptions, Model, ModelImageInputLimits, ModelInputLimits};
use pi_durable::DurableResult;
use pi_durable::env::SharedEnv;
use pi_durable::harness::ToolExecutionResult;
use pi_durable::images::{ImageProcessor, ImageSize, create_image_processor, to_base64};
use pi_durable::tools::image::image_dimensions;
use pi_durable::tools::{ReadToolOptions, create_coding_tools, create_read_tool, execute_read};
use serde_json::json;
use tempfile::TempDir;

use super::images::*;
use super::support::*;

#[derive(Default)]
struct Call {
    images: Option<Arc<dyn ImageProcessor>>,
    input: Option<Vec<&'static str>>,
    resize: Option<ImageResizeOptions>,
    env: Option<SharedEnv>,
}

fn processor() -> Option<Arc<dyn ImageProcessor>> {
    Some(Arc::new(create_image_processor()))
}

/// Run `read` on `bytes` written to `name`, for a conversation whose model takes `call.input`.
async fn read(dir: &TempDir, name: &str, bytes: &[u8], call: Call) -> DurableResult<ToolExecutionResult> {
    std::fs::write(dir.path().join(name), bytes).unwrap();
    let images = call.images;
    let limits = call.resize.map(|resize| ModelInputLimits {
        images: Some(ModelImageInputLimits { resize: Some(resize), ..Default::default() }),
        ..Default::default()
    });
    let input = call.input.unwrap_or(vec!["text", "image"]).into_iter().map(String::from).collect();
    let model = Model { input, input_limits: limits, ..Default::default() };
    let env = call.env.unwrap_or_else(|| Arc::new(HookedEnv::new(dir.path().to_str().unwrap())));
    let api = Arc::new(FakeApi { env: Some(env), model: Some(model), ..Default::default() });
    execute_read(&ReadToolOptions { images }, args(json!({ "path": name })), api, ctx()).await
}

/// An environment whose readers report each positional read, after it, to `on_read`.
fn read_env(dir: &TempDir, on_read: impl Fn(u64, u64) + Send + Sync + 'static) -> SharedEnv {
    let mut env = HookedEnv::new(dir.path().to_str().unwrap());
    let on_read: Arc<dyn Fn(u64, u64) + Send + Sync> = Arc::new(on_read);
    env.on_reader = Some(Arc::new(move |inner| Box::new(ObservedReader { inner, on_read: on_read.clone() })));
    Arc::new(env)
}

fn append(dir: &TempDir, name: &str, bytes: &[u8]) {
    use std::io::Write;
    std::fs::OpenOptions::new().append(true).open(dir.path().join(name)).unwrap().write_all(bytes).unwrap();
}

fn only_image(result: &ToolExecutionResult) -> (String, String) {
    match result.output.as_deref() {
        Some([ContentBlock::Image { data, mime_type }]) => (data.clone(), mime_type.clone()),
        other => panic!("expected one image, got {other:?}"),
    }
}

fn refused(result: &ToolExecutionResult) -> bool {
    result.output == Some(Vec::new()) && result.is_error == Some(true)
}

#[tokio::test]
async fn returns_a_supported_image_within_the_limits_as_it_is() {
    let dir = tempfile::tempdir().unwrap();
    let png = encode(8, 6, Format::Png);
    let result = read(&dir, "a.png", &png, Call::default()).await.unwrap();
    assert_eq!(only_image(&result), (to_base64(&png), "image/png".into()));
    assert_eq!(messages(&result), ["Read image file [image/png]."]);
    let tiny = read(&dir, "a.gif", &gif(1, 1), Call::default()).await.unwrap();
    assert_eq!(only_image(&tiny), (to_base64(&gif(1, 1)), "image/gif".into()));
}

#[tokio::test]
async fn refuses_an_image_too_large_to_send_without_reading_it() {
    let dir = tempfile::tempdir().unwrap();
    let largest = Arc::new(AtomicU64::new(0));
    let seen = largest.clone();
    let env = read_env(&dir, move |_, length| _ = seen.fetch_max(length, Ordering::SeqCst));
    let result = read(&dir, "big.png", &noisy_png(1100, 1100), Call { env: Some(env), ..Default::default() }).await;
    let result = result.unwrap();
    assert!(refused(&result));
    let message = &messages(&result)[0];
    assert!(message.starts_with("big.png is an image (image/png) of ") && message.contains(", too large to send"));
    // Only the header and the chunk walk, in blocks of at most 64 KiB.
    assert!(largest.load(Ordering::SeqCst) <= 64 * 1024);
}

#[tokio::test]
async fn refuses_an_image_wider_or_taller_than_the_limits_by_its_header() {
    let dir = tempfile::tempdir().unwrap();
    let wide = read(&dir, "wide.png", &encode(2001, 2, Format::Png), Call::default()).await.unwrap();
    assert!(refused(&wide));
    assert_eq!(
        messages(&wide),
        [
            "wide.png is an image (image/png) of 2001x2, larger than 2000x2000, and no image processor is configured to shrink it"
        ]
    );
    let tall = read(&dir, "tall.gif", &gif(1, 3000), Call::default()).await.unwrap();
    assert!(messages(&tall)[0].starts_with("tall.gif is an image (image/gif) of 1x3000, larger than 2000x2000"));
}

#[tokio::test]
async fn refuses_a_format_that_needs_converting() {
    let dir = tempfile::tempdir().unwrap();
    let result = read(&dir, "a.bmp", &bmp(4, 4), Call::default()).await.unwrap();
    assert!(refused(&result));
    assert_eq!(
        messages(&result),
        ["a.bmp is an image (image/bmp) that needs converting, and no image processor is configured"]
    );
}

#[tokio::test]
async fn says_when_the_model_sees_a_placeholder_instead_of_the_image() {
    let dir = tempfile::tempdir().unwrap();
    let call = Call { input: Some(vec!["text"]), ..Default::default() };
    let result = read(&dir, "a.png", &encode(4, 4, Format::Png), call).await.unwrap();
    assert_eq!(result.output.as_ref().map(Vec::len), Some(1));
    assert_eq!(
        messages(&result),
        ["Read image file [image/png]. The current model does not support images; it sees a placeholder instead."]
    );
}

#[tokio::test]
async fn applies_the_models_image_limits_instead_of_the_defaults() {
    let dir = tempfile::tempdir().unwrap();
    let png = encode(8, 6, Format::Png);
    let narrow = || Some(ImageResizeOptions { max_width: Some(4), ..Default::default() });
    let refused = read(&dir, "a.png", &png, Call { resize: narrow(), ..Default::default() }).await.unwrap();
    assert!(messages(&refused)[0].contains("of 8x6, larger than 4x2000,"));
    let call = Call { resize: narrow(), images: processor(), ..Default::default() };
    let resized = read(&dir, "a.png", &png, call).await.unwrap();
    assert_eq!(decoded(&only_image(&resized).0).dimensions(), (4, 3));
}

#[test]
fn lists_bmp_in_the_description_only_with_a_processor() {
    assert!(create_read_tool(ReadToolOptions::default()).tool.description.contains("(jpg, png, gif, webp)"));
    let with = create_read_tool(ReadToolOptions { images: processor() });
    assert!(with.tool.description.contains("(jpg, png, gif, webp, bmp)"));
}

#[tokio::test]
async fn reads_it_again_rather_than_return_a_cut_off_image_even_when_it_grew() {
    let dir = Arc::new(tempfile::tempdir().unwrap());
    let png = encode(8, 6, Format::Png);
    let whole_reads = Arc::new(Mutex::new(Vec::new()));
    let (seen, size, writer) = (whole_reads.clone(), png.len() as u64, dir.clone());
    // A writer appends during the first read of the whole file; the second read sees it all.
    let env = read_env(&dir, move |offset, length| {
        if offset != 0 || length < size {
            return;
        }
        let mut reads = seen.lock().unwrap();
        reads.push(length);
        if reads.len() == 1 {
            append(&writer, "growing.png", &[0; 16]);
        }
    });
    let result = read(&dir, "growing.png", &png, Call { env: Some(env), ..Default::default() }).await.unwrap();
    assert_eq!(*whole_reads.lock().unwrap(), [size, size + 16]);
    let after = [png.clone(), vec![0; 16]].concat();
    assert_eq!(only_image(&result).0, to_base64(&after));
}

#[tokio::test]
async fn fails_when_it_changes_during_the_second_read_too() {
    let dir = Arc::new(tempfile::tempdir().unwrap());
    let png = encode(8, 6, Format::Png);
    let (size, writer) = (png.len() as u64, dir.clone());
    let env = read_env(&dir, move |offset, length| {
        if offset == 0 && length >= size {
            append(&writer, "busy.png", &[0]);
        }
    });
    let error = read(&dir, "busy.png", &png, Call { env: Some(env), ..Default::default() }).await.unwrap_err();
    assert_eq!(error.to_string(), "busy.png changed while it was read");
}

#[tokio::test]
async fn with_a_processor_passes_an_image_that_needs_no_change_through_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let png = encode(8, 6, Format::Png);
    let result = read(&dir, "a.png", &png, Call { images: processor(), ..Default::default() }).await.unwrap();
    assert_eq!(only_image(&result), (to_base64(&png), "image/png".into()));
    let gif = read(&dir, "a.gif", &gif(1, 1), Call { images: processor(), ..Default::default() }).await.unwrap();
    assert_eq!(only_image(&gif).1, "image/gif");
}

#[tokio::test]
async fn scales_an_image_down_to_the_maximum_size_and_says_how_to_map_coordinates_back() {
    let dir = tempfile::tempdir().unwrap();
    let call = Call { images: processor(), ..Default::default() };
    let result = read(&dir, "wide.png", &encode(4000, 1000, Format::Png), call).await.unwrap();
    let (data, mime_type) = only_image(&result);
    assert_eq!(decoded(&data).dimensions(), (2000, 500));
    assert_eq!(
        messages(&result),
        [format!(
            "Read image file [{mime_type}]. Resized from 4000x1000 to 2000x500. Multiply coordinates by 2.00 to map them to the original."
        )]
    );
}

#[tokio::test]
async fn re_encodes_a_png_too_large_for_the_byte_limit_as_jpeg_at_its_size() {
    let dir = tempfile::tempdir().unwrap();
    let call = Call { images: processor(), ..Default::default() };
    let result = read(&dir, "big.png", &noisy_png(1100, 1100), call).await.unwrap();
    let (data, mime_type) = only_image(&result);
    assert_eq!(mime_type, "image/jpeg");
    assert_eq!(messages(&result), ["Read image file [image/jpeg]. Converted from image/png to image/jpeg."]);
    assert!(data.len() <= pi_durable::images::DEFAULT_IMAGE_LIMITS.max_bytes);
    assert_eq!(decoded(&data).dimensions(), (1100, 1100));
}

#[tokio::test]
async fn converts_a_format_providers_do_not_take_inline_to_png() {
    let dir = tempfile::tempdir().unwrap();
    let result = read(&dir, "a.bmp", &bmp(4, 4), Call { images: processor(), ..Default::default() }).await.unwrap();
    assert_eq!(only_image(&result).1, "image/png");
    assert_eq!(messages(&result), ["Read image file [image/png]. Converted from image/bmp to image/png."]);
}

#[tokio::test]
async fn reports_bytes_it_cannot_decode_as_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let broken = encode(4, 4, Format::Png)[..40].to_vec();
    let result = read(&dir, "broken.png", &broken, Call { images: processor(), ..Default::default() }).await.unwrap();
    assert!(refused(&result));
    assert_eq!(messages(&result), ["broken.png is an image (image/png) that cannot be prepared for the model"]);
}

// Upstream runs the `read` that `createCodingTools({ images })` installs. A registered tool needs a Harness api, so
// this checks that the installed `read` carries the processor, which only its description shows.
#[test]
fn is_what_create_coding_tools_with_images_gives_read() {
    let extension = create_coding_tools(ReadToolOptions { images: processor() });
    let read = extension.tools.iter().find(|tool| tool.name() == "read").unwrap();
    assert!(read.tool.description.contains("(jpg, png, gif, webp, bmp)"));
}

#[test]
fn reads_the_size_each_format_declares_in_its_header() {
    let size = |width, height| Some(ImageSize { width, height });
    assert_eq!(image_dimensions(&encode(7, 5, Format::Png), "image/png"), size(7, 5));
    assert_eq!(image_dimensions(&encode(7, 5, Format::Jpeg), "image/jpeg"), size(7, 5));
    let oriented = with_orientation(&encode(7, 5, Format::Jpeg), 6, false, true);
    assert_eq!(image_dimensions(&oriented, "image/jpeg"), size(7, 5));
    assert_eq!(image_dimensions(&gif(300, 200), "image/gif"), size(300, 200));
    let webp = |kind: &str, data: &[u8]| [b"RIFF\0\0\0\0WEBP", kind.as_bytes(), &[0; 4], data, &[0; 16]].concat();
    // VP8: frame tag, start code, then 14-bit width and height.
    let vp8 = webp("VP8 ", &[0, 0, 0, 0x9d, 0x01, 0x2a, 0x2c, 0x01, 0xc8, 0x00]);
    assert_eq!(image_dimensions(&vp8, "image/webp"), size(300, 200));
    // VP8L: signature, then width - 1 and height - 1 in 14 bits each.
    let bits: u32 = 299 | (199 << 14);
    let vp8l = webp("VP8L", &[[0x2f].as_slice(), &bits.to_le_bytes()].concat());
    assert_eq!(image_dimensions(&vp8l, "image/webp"), size(300, 200));
    // VP8X: flags, then width - 1 and height - 1 in 24 bits each.
    let vp8x = webp("VP8X", &[0, 0, 0, 0, 0x2b, 0x01, 0, 0xc7, 0, 0]);
    assert_eq!(image_dimensions(&vp8x, "image/webp"), size(300, 200));
    assert_eq!(image_dimensions(&[0xff, 0xd8, 0xff], "image/jpeg"), None);
}
