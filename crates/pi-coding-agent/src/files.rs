//! `@path` arguments: text files are inlined in `<file>` tags, images are attached.

use std::path::Path;

use base64::Engine;
use pi_ai::ContentBlock;

use crate::tools::detect_supported_image_mime_type;
use crate::tools::path_utils::resolve_read_path;

#[derive(Debug, Default, PartialEq)]
pub struct ProcessedFiles {
    pub text: String,
    pub images: Vec<ContentBlock>,
}

pub fn process_file_args(paths: &[String], cwd: &Path) -> anyhow::Result<ProcessedFiles> {
    let mut processed = ProcessedFiles::default();
    for input in paths {
        let path = resolve_read_path(input, cwd);
        let bytes =
            std::fs::read(&path).map_err(|error| anyhow::anyhow!("Cannot read file {}: {error}", path.display()))?;
        let name = path.display();
        if let Some(mime_type) = detect_supported_image_mime_type(&bytes) {
            processed.images.push(ContentBlock::Image {
                data: base64::engine::general_purpose::STANDARD.encode(&bytes),
                mime_type: mime_type.to_string(),
            });
            processed.text.push_str(&format!("<file name=\"{name}\"></file>\n"));
        } else if bytes.is_empty() {
            processed.text.push_str(&format!("<file name=\"{name}\"></file>\n"));
        } else {
            let content = String::from_utf8_lossy(&bytes);
            processed.text.push_str(&format!("<file name=\"{name}\">\n{content}\n</file>\n"));
        }
    }
    Ok(processed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inlines_text_and_attaches_images() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "hello").unwrap();
        let mut png = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 13];
        png.extend_from_slice(b"IHDR");
        png.extend_from_slice(&[0, 0, 0, 1, 0, 0, 0, 1, 8, 6, 0, 0, 0, 0x1f, 0x15, 0xc4, 0x89]);
        std::fs::write(dir.path().join("b.png"), png).unwrap();
        let processed = process_file_args(&["a.txt".into(), "b.png".into()], dir.path()).unwrap();
        let a = dir.path().join("a.txt");
        assert!(processed.text.starts_with(&format!("<file name=\"{}\">\nhello\n</file>\n", a.display())));
        assert_eq!(processed.images.len(), 1);
        assert!(process_file_args(&["missing".into()], dir.path()).is_err());
    }
}
