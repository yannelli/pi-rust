//! Upstream MIT JavaScript embedded for extensions (built by `cargo xtask vendor-js`): typebox,
//! pi-tui with marked and get-east-asian-width, partial-json, and pure pi-ai utilities. The files
//! load from memory under [`vendor_root`], so relative imports between them resolve.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use flate2::read::GzDecoder;

static ARCHIVE: &[u8] = include_bytes!("vendor.json.gz");

/// The bundled files by archive path, decompressed once.
fn files() -> &'static BTreeMap<String, String> {
    static FILES: OnceLock<BTreeMap<String, String>> = OnceLock::new();
    FILES.get_or_init(|| {
        let mut json = Vec::new();
        let decoded = GzDecoder::new(ARCHIVE).read_to_end(&mut json);
        decoded.ok().and_then(|_| serde_json::from_slice(&json).ok()).unwrap_or_default()
    })
}

/// The absolute directory the bundled files appear under. Nothing is written there.
pub fn vendor_root() -> PathBuf {
    std::env::temp_dir().join("pi-extension-host")
}

/// Every bundled file as an absolute path under `root` and its source.
pub fn source_files(root: &Path) -> Vec<(PathBuf, String)> {
    files().iter().map(|(key, source)| (root.join(key), source.clone())).collect()
}

/// The absolute path of a bundled file, as an import specifier.
pub fn specifier(root: &Path, key: &str) -> String {
    root.join(key).display().to_string().replace('\\', "/")
}

/// Whether the archive decoded; false only for a corrupt build.
pub fn is_available() -> bool {
    !files().is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archive_holds_the_bundled_packages_and_their_licenses() {
        assert!(is_available());
        let keys = files();
        for key in ["typebox/build/index.mjs", "pi-tui/src/index.ts", "marked/LICENSE", "pi-ai/src/utils/uuid.ts"] {
            assert!(keys.contains_key(key), "{key}");
        }
        let markdown = &keys["pi-tui/src/components/markdown.ts"];
        assert!(markdown.contains("\"../../../marked/lib/marked.esm.js\""));
    }
}
