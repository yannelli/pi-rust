//! Exposes `syncedVersion` of `docs/parity/upstream.json`, the upstream pi version this port has
//! parity with, as `PI_UPSTREAM_VERSION`.

use std::path::Path;

fn main() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/parity/upstream.json");
    println!("cargo:rerun-if-changed={}", path.display());
    println!("cargo:rerun-if-changed=build.rs");
    let text = std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    let version = string_field(&text, "syncedVersion")
        .unwrap_or_else(|| panic!("{} has no string field syncedVersion", path.display()));
    assert!(
        !version.is_empty() && version.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+')),
        "syncedVersion {version:?} is not a version"
    );
    println!("cargo:rustc-env=PI_UPSTREAM_VERSION={version}");
}

/// The value of a top-level `"key": "value"` pair without escapes; a build dependency would be
/// heavier than this flat file needs.
fn string_field(text: &str, key: &str) -> Option<String> {
    let after_key = &text[text.find(&format!("\"{key}\""))? + key.len() + 2..];
    let value = after_key.trim_start().strip_prefix(':')?.trim_start().strip_prefix('"')?;
    Some(value[..value.find('"')?].to_string())
}
