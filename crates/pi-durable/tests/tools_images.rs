//! Upstream `test/tools-images.test.ts`: the image processor called directly here, and the `read` tool cases and
//! `imageDimensions` in `tools_images/read.rs`.

#[path = "support/images.rs"]
mod images;
#[path = "tools_images/read.rs"]
mod read;
#[path = "support/tools_support.rs"]
mod support;

use images::*;
use pi_durable::images::{
    DEFAULT_IMAGE_LIMITS, ImageLimits, ImageProcessor, ImageResize, ImageSize, create_image_processor,
    exif_orientation, to_base64,
};

#[tokio::test]
async fn passes_an_image_that_needs_no_change_through_untouched() {
    let png = encode(8, 6, Format::Png);
    let prepared = create_image_processor().prepare(&png, "image/png", DEFAULT_IMAGE_LIMITS).await.unwrap();
    assert_eq!((prepared.data, prepared.mime_type.as_str()), (to_base64(&png), "image/png"));
    let gif = create_image_processor().prepare(&gif(1, 1), "image/gif", DEFAULT_IMAGE_LIMITS).await.unwrap();
    assert_eq!(gif.mime_type, "image/gif");
}

#[tokio::test]
async fn applies_smaller_limits() {
    let limits = ImageLimits { max_width: 4, ..DEFAULT_IMAGE_LIMITS };
    let prepared = create_image_processor().prepare(&encode(8, 6, Format::Png), "image/png", limits).await.unwrap();
    assert_eq!(decoded(&prepared.data).dimensions(), (4, 3));
}

#[tokio::test]
async fn scales_an_image_down_to_the_maximum_size() {
    let prepared =
        create_image_processor().prepare(&encode(4000, 1000, Format::Png), "image/png", DEFAULT_IMAGE_LIMITS).await;
    let prepared = prepared.unwrap();
    assert_eq!(decoded(&prepared.data).dimensions(), (2000, 500));
    let resize =
        ImageResize { from: ImageSize { width: 4000, height: 1000 }, to: ImageSize { width: 2000, height: 500 } };
    assert_eq!(prepared.resized, Some(resize));
}

#[tokio::test]
async fn re_encodes_a_png_too_large_for_the_byte_limit_as_jpeg_and_shrinks_it_when_that_is_not_enough() {
    let processor = create_image_processor();
    let jpeg = processor.prepare(&noisy_png(1100, 1100), "image/png", DEFAULT_IMAGE_LIMITS).await.unwrap();
    assert_eq!(jpeg.mime_type, "image/jpeg");
    assert_eq!(jpeg.converted_from.as_deref(), Some("image/png"));
    assert!(jpeg.data.len() <= DEFAULT_IMAGE_LIMITS.max_bytes);
    assert_eq!(decoded(&jpeg.data).dimensions(), (1100, 1100));
    let tight = ImageLimits { max_bytes: 200_000, ..DEFAULT_IMAGE_LIMITS };
    let shrunk = processor.prepare(&noisy_png(2000, 2000), "image/png", tight).await.unwrap();
    assert!(shrunk.data.len() <= tight.max_bytes);
    let resized = shrunk.resized.unwrap();
    assert_eq!(resized.from, ImageSize { width: 2000, height: 2000 });
    assert!(resized.to.width < 2000);
}

#[tokio::test]
async fn keeps_at_least_one_pixel_on_each_side_of_an_extreme_aspect_ratio() {
    let prepared =
        create_image_processor().prepare(&encode(6000, 2, Format::Png), "image/png", DEFAULT_IMAGE_LIMITS).await;
    let resize = ImageResize { from: ImageSize { width: 6000, height: 2 }, to: ImageSize { width: 2000, height: 1 } };
    assert_eq!(prepared.unwrap().resized, Some(resize));
}

#[tokio::test]
async fn gives_up_when_not_even_one_pixel_fits_the_byte_limit() {
    let limits = ImageLimits { max_bytes: 8, ..DEFAULT_IMAGE_LIMITS };
    assert_eq!(create_image_processor().prepare(&encode(4, 4, Format::Png), "image/png", limits).await, None);
}

#[tokio::test]
async fn keeps_a_jpeg_that_must_be_re_encoded_a_jpeg() {
    let jpeg = with_orientation(&encode(40, 30, Format::Jpeg), 3, false, false);
    let prepared = create_image_processor().prepare(&jpeg, "image/jpeg", DEFAULT_IMAGE_LIMITS).await.unwrap();
    assert_eq!(prepared.mime_type, "image/jpeg");
}

#[tokio::test]
async fn converts_a_format_providers_do_not_take_inline_to_png() {
    let prepared = create_image_processor().prepare(&bmp(4, 4), "image/bmp", DEFAULT_IMAGE_LIMITS).await.unwrap();
    assert_eq!(prepared.mime_type, "image/png");
    assert_eq!(prepared.converted_from.as_deref(), Some("image/bmp"));
}

#[tokio::test]
async fn reports_bytes_it_cannot_decode_as_none() {
    let broken = encode(4, 4, Format::Png)[..40].to_vec();
    assert_eq!(create_image_processor().prepare(&broken, "image/png", DEFAULT_IMAGE_LIMITS).await, None);
}

#[tokio::test]
async fn turns_every_orientation_upright() {
    for orientation in 1..=8 {
        let (rgba, width, height) = blocks(&STORED, 16);
        let jpeg = with_orientation(&encode_rgba(&rgba, width, height, Format::Jpeg), orientation, false, false);
        let prepared = create_image_processor().prepare(&jpeg, "image/jpeg", DEFAULT_IMAGE_LIMITS).await.unwrap();
        let image = decoded(&prepared.data);
        let expected = upright(orientation);
        let size = (expected[0].len() as u32 * 16, expected.len() as u32 * 16);
        assert_eq!(image.dimensions(), size, "orientation {orientation}");
        // The colour at the centre of each block, rounded to full channels against JPEG noise.
        for (by, row) in expected.iter().enumerate() {
            for (bx, color) in row.iter().enumerate() {
                let pixel = image.get_pixel(bx as u32 * 16 + 8, by as u32 * 16 + 8);
                let seen = [0, 1, 2].map(|channel| if pixel[channel] > 127 { 255 } else { 0 });
                assert_eq!(&seen, color, "orientation {orientation} block {bx},{by}");
            }
        }
    }
}

#[test]
fn reads_big_and_little_endian_jpeg_exif_after_an_app0_segment_and_1_when_absent_or_out_of_range() {
    let jpeg = encode(2, 2, Format::Jpeg);
    assert_eq!(exif_orientation(&with_orientation(&jpeg, 6, false, false)), 6);
    assert_eq!(exif_orientation(&with_orientation(&jpeg, 8, true, false)), 8);
    assert_eq!(exif_orientation(&with_orientation(&jpeg, 5, false, true)), 5);
    assert_eq!(exif_orientation(&with_orientation(&jpeg, 9, false, false)), 1);
    assert_eq!(exif_orientation(&jpeg), 1);
    assert_eq!(exif_orientation(&encode(2, 2, Format::Png)), 1);
}

#[test]
fn reads_webp_exif_chunks_with_and_without_the_exif_prefix() {
    assert_eq!(exif_orientation(&webp_with_orientation(6, false)), 6);
    assert_eq!(exif_orientation(&webp_with_orientation(3, true)), 3);
}

#[test]
fn to_base64_matches_standard_base64_for_every_remainder_length() {
    use base64::Engine;
    for length in [0usize, 1, 2, 3, 4, 5, 10_000] {
        let bytes: Vec<u8> = (0..length).map(|index| ((index * 37) % 256) as u8).collect();
        assert_eq!(to_base64(&bytes), base64::engine::general_purpose::STANDARD.encode(&bytes));
    }
}
