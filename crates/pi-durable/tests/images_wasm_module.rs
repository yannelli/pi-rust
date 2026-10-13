//! Port of upstream `test/images-wasm-module.test.ts`. Upstream checks that Photon initializes from a compiled
//! WebAssembly module, as Cloudflare Workers pass it. The Rust processor has no WebAssembly module to load, so the
//! case checks the same outcome: a fresh processor decodes a 1x1 GIF and returns it as it is.

use base64::Engine;
use pi_durable::images::{DEFAULT_IMAGE_LIMITS, ImageProcessor, PreparedImage, create_image_processor};

#[tokio::test]
async fn create_image_processor_prepares_a_gif_without_initialization() {
    let data = "R0lGODlhAQABAIAAAAAAAP///yH5BAEAAAAALAAAAAABAAEAAAIBRAA7";
    let bytes = base64::engine::general_purpose::STANDARD.decode(data).unwrap();
    let prepared = create_image_processor().prepare(&bytes, "image/gif", DEFAULT_IMAGE_LIMITS).await;
    let expected =
        PreparedImage { data: data.into(), mime_type: "image/gif".into(), resized: None, converted_from: None };
    assert_eq!(prepared, Some(expected));
}
