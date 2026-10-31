//! MIT builds the repository already ships that `vendor-js` adds to the extension archive: `marked-cjk-friendly`
//! 0.1.2, the IIFE build of the HTML export, which pi-tui's `components/markdown.ts` imports.

use std::collections::BTreeMap;
use std::path::Path;

const ASSET_DIR: &str = "crates/pi-coding-agent/src/export/assets/vendor";
const CJK_FRIENDLY_SCRIPT: &str = "marked-cjk-friendly.min.js";
const CJK_FRIENDLY_LICENSE_FILE: &str = "marked-cjk-friendly.LICENSE";
/// What the IIFE build defines; the module exports it.
const CJK_FRIENDLY_GLOBAL: &str = "globalThis.markedCjkFriendly=";

pub const CJK_FRIENDLY_MODULE: &str = "marked-cjk-friendly/index.js";
pub const CJK_FRIENDLY_LICENSE: &str = "marked-cjk-friendly/LICENSE";

/// Add the repository's `marked-cjk-friendly` build as an ES module, and its license.
pub fn add_repo_assets(root: &Path, files: &mut BTreeMap<String, String>) -> Result<(), String> {
    let dir = root.join(ASSET_DIR);
    let read = |name: &str| {
        let path = dir.join(name);
        std::fs::read_to_string(&path).map_err(|error| format!("{}: {error}", path.display()))
    };
    files.insert(CJK_FRIENDLY_MODULE.into(), as_module(&read(CJK_FRIENDLY_SCRIPT)?)?);
    files.insert(CJK_FRIENDLY_LICENSE.into(), read(CJK_FRIENDLY_LICENSE_FILE)?);
    Ok(())
}

/// The IIFE build followed by a default export of the function it defines.
pub fn as_module(script: &str) -> Result<String, String> {
    if !script.contains(CJK_FRIENDLY_GLOBAL) {
        return Err(format!("{CJK_FRIENDLY_SCRIPT} no longer defines {CJK_FRIENDLY_GLOBAL}"));
    }
    Ok(format!("{}\nexport default globalThis.markedCjkFriendly;\n", script.trim_end()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> &'static Path {
        Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/.."))
    }

    fn archive() -> BTreeMap<String, String> {
        crate::vendor_js::read_archive(&root().join("crates/pi-coding-agent/src/extensions/vendor.json.gz")).unwrap()
    }

    #[test]
    fn turns_the_shipped_build_into_a_module_with_a_default_export_and_its_license() {
        let mut files = BTreeMap::new();
        add_repo_assets(root(), &mut files).unwrap();
        let script = std::fs::read_to_string(root().join(ASSET_DIR).join(CJK_FRIENDLY_SCRIPT)).unwrap();
        let module = &files[CJK_FRIENDLY_MODULE];
        assert!(module.starts_with(script.trim_end()), "the build is kept byte for byte");
        assert!(module.ends_with("\nexport default globalThis.markedCjkFriendly;\n"));
        assert!(module.contains("marked-cjk-friendly v0.1.2") && module.contains("(MIT License)"));
        assert!(files[CJK_FRIENDLY_LICENSE].contains("Permission is hereby granted, free of charge"));
        assert_eq!(files.len(), 2);
    }

    #[test]
    fn rejects_a_build_that_defines_no_global() {
        assert!(as_module("(function(){})();").is_err());
    }

    #[test]
    fn rewrites_the_markdown_import_to_the_bundled_module() {
        let source = "import markedCjkFriendly from \"marked-cjk-friendly\";\nimport { Marked } from \"marked\";";
        let rewritten = crate::vendor_js::rewrite_bare_imports("pi-tui/src/components/markdown.ts", source);
        assert!(rewritten.contains("from \"../../../marked-cjk-friendly/index.js\""), "{rewritten}");
        assert!(rewritten.contains("from \"../../../marked/lib/marked.esm.js\""), "{rewritten}");
    }

    #[test]
    fn the_archive_holds_the_module_and_resolves_every_markdown_import() {
        let files = archive();
        assert!(files[CJK_FRIENDLY_MODULE].ends_with("export default globalThis.markedCjkFriendly;\n"));
        assert!(files.contains_key(CJK_FRIENDLY_LICENSE));
        let key = "pi-tui/src/components/markdown.ts";
        let markdown = &files[key];
        assert!(markdown.contains("markedCjkFriendly().tokenizer?.emStrong"), "markdown.ts is the pinned version");
        for specifier in markdown.split("from \"").skip(1).filter_map(|rest| rest.split('"').next()) {
            assert!(specifier.starts_with('.'), "{key} keeps the bare import {specifier}");
            let resolved = resolve(key, specifier);
            assert!(files.contains_key(&resolved), "{key}: {specifier} -> {resolved}");
        }
    }

    /// `specifier` relative to the directory of archive key `key`.
    fn resolve(key: &str, specifier: &str) -> String {
        let mut parts: Vec<&str> = key.split('/').collect();
        parts.pop();
        for segment in specifier.split('/') {
            match segment {
                "." => {}
                ".." => {
                    parts.pop();
                }
                name => parts.push(name),
            }
        }
        parts.join("/")
    }
}
