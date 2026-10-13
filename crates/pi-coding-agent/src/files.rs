//! `@path` arguments: text files are inlined in `<file>` tags, images are attached
//! after auto-resize (upstream `cli/file-processor.ts`).

use std::path::Path;

use pi_ai::ContentBlock;

use crate::images::{ProcessImageOptions, process_image};
use crate::tools::detect_supported_image_mime_type;
use crate::tools::path_utils::resolve_read_path;

const IMAGE_TYPE_SNIFF_BYTES: usize = 4100;

#[derive(Debug, Default, PartialEq)]
pub struct ProcessedFiles {
    pub text: String,
    pub images: Vec<ContentBlock>,
}

/// Process `@file` arguments with the default image limits (2000x2000, 4.5MB).
pub fn process_file_args(paths: &[String], cwd: &Path) -> anyhow::Result<ProcessedFiles> {
    process_file_args_with(paths, cwd, &ProcessImageOptions::default())
}

/// `@file` arguments of the command line (upstream `prepareInitialMessage`): images stay at full
/// size, because the session resizes them for the chosen model per `images.autoResize`.
pub fn process_cli_file_args(paths: &[String], cwd: &Path) -> anyhow::Result<ProcessedFiles> {
    process_file_args_with(paths, cwd, &ProcessImageOptions { auto_resize: false, resize: None })
}

/// Process `@file` arguments; images go through [`process_image`] with `options`.
pub fn process_file_args_with(
    paths: &[String],
    cwd: &Path,
    options: &ProcessImageOptions,
) -> anyhow::Result<ProcessedFiles> {
    let mut processed = ProcessedFiles::default();
    for input in paths {
        let path = resolve_read_path(input, cwd);
        let bytes =
            std::fs::read(&path).map_err(|error| anyhow::anyhow!("Cannot read file {}: {error}", path.display()))?;
        if bytes.is_empty() {
            continue;
        }
        let name = path.display();
        let sniff = &bytes[..bytes.len().min(IMAGE_TYPE_SNIFF_BYTES)];
        if let Some(mime_type) = detect_supported_image_mime_type(sniff) {
            match process_image(&bytes, mime_type, options) {
                Ok(image) => {
                    let hints = image.hints.join("\n");
                    processed.images.push(ContentBlock::Image { data: image.data, mime_type: image.mime_type });
                    processed.text.push_str(&format!("<file name=\"{name}\">{hints}</file>\n"));
                }
                Err(message) => processed.text.push_str(&format!("<file name=\"{name}\">{message}</file>\n")),
            }
        } else {
            let content = String::from_utf8_lossy(&bytes);
            let content = content.strip_prefix('\u{feff}').unwrap_or(&content);
            processed.text.push_str(&format!("<file name=\"{name}\">\n{content}\n</file>\n"));
        }
    }
    Ok(processed)
}

#[cfg(test)]
mod tests {
    use base64::Engine;

    use super::*;

    fn png(width: u32, height: u32) -> Vec<u8> {
        let image =
            image::RgbaImage::from_fn(width, height, |x, y| image::Rgba([(x % 256) as u8, (y % 256) as u8, 7, 255]));
        crate::images::encode_png(&image).unwrap()
    }

    #[test]
    fn cli_file_images_keep_their_size() {
        let dir = tempfile::tempdir().unwrap();
        let original = png(2600, 20);
        std::fs::write(dir.path().join("wide.png"), &original).unwrap();
        let processed = process_cli_file_args(&["wide.png".into()], dir.path()).unwrap();
        let [ContentBlock::Image { data, .. }] = processed.images.as_slice() else { panic!("one image") };
        assert_eq!(base64::engine::general_purpose::STANDARD.decode(data).unwrap(), original);
        let resized = process_file_args(&["wide.png".into()], dir.path()).unwrap();
        let [ContentBlock::Image { data, .. }] = resized.images.as_slice() else { panic!("one image") };
        assert_ne!(base64::engine::general_purpose::STANDARD.decode(data).unwrap(), original);
    }

    #[test]
    fn inlines_text_and_attaches_images() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "\u{feff}hello").unwrap();
        std::fs::write(dir.path().join("b.png"), png(1, 1)).unwrap();
        std::fs::write(dir.path().join("empty.txt"), "").unwrap();
        let args = ["a.txt".into(), "b.png".into(), "empty.txt".into()];
        let processed = process_file_args(&args, dir.path()).unwrap();
        let (a, b) = (dir.path().join("a.txt"), dir.path().join("b.png"));
        let expected =
            format!("<file name=\"{}\">\nhello\n</file>\n<file name=\"{}\"></file>\n", a.display(), b.display());
        assert_eq!(processed.text, expected);
        assert_eq!(processed.images.len(), 1);
        assert!(process_file_args(&["missing".into()], dir.path()).is_err());
    }

    #[test]
    fn resizes_large_images_and_notes_the_scale() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("wide.png"), png(2400, 30)).unwrap();
        let processed = process_file_args(&["wide.png".into()], dir.path()).unwrap();
        let ContentBlock::Image { data, mime_type } = &processed.images[0] else { panic!("expected an image") };
        let bytes = base64::engine::general_purpose::STANDARD.decode(data).unwrap();
        let decoded = image::load_from_memory(&bytes).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (2000, 25));
        assert_eq!(mime_type, "image/png");
        assert!(processed.text.contains(
            "[Image: original 2400x30, displayed at 2000x25. Multiply coordinates by 1.20 to map to original image.]"
        ));

        let options = ProcessImageOptions { auto_resize: false, resize: None };
        let processed = process_file_args_with(&["wide.png".into()], dir.path(), &options).unwrap();
        let ContentBlock::Image { data, .. } = &processed.images[0] else { panic!("expected an image") };
        assert_eq!(base64::engine::general_purpose::STANDARD.decode(data).unwrap(), png(2400, 30));
    }

    #[test]
    fn notes_images_that_cannot_be_processed() {
        let dir = tempfile::tempdir().unwrap();
        let mut broken = png(4, 4);
        broken.truncate(40);
        std::fs::write(dir.path().join("broken.png"), &broken).unwrap();
        let processed = process_file_args(&["broken.png".into()], dir.path()).unwrap();
        assert!(processed.images.is_empty());
        assert!(
            processed.text.contains(">[Image omitted: could not be resized below the inline image size limit.]</file>")
        );
    }
}
