//! `cargo xtask vendor-js <upstream-root> [--out <path>]`: bundle the upstream MIT JavaScript the
//! extension host embeds (typebox, pi-tui and its dependencies, pure pi-ai utilities) as gzip JSON
//! of `path -> source`, with bare imports between bundled packages rewritten to relative paths.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use flate2::Compression;
use flate2::write::GzEncoder;

/// Pure pi-ai utilities the `@earendil-works/pi-ai` shim re-exports.
const PI_AI_FILES: &[&str] = &[
    "utils/assistant-message-frame.ts",
    "utils/diagnostics.ts",
    "utils/estimate.ts",
    "utils/event-stream.ts",
    "utils/json-parse.ts",
    "utils/overflow.ts",
    "utils/sanitize-unicode.ts",
    "utils/text.ts",
    "utils/transcript.ts",
    "utils/typebox-helpers.ts",
    "utils/uuid.ts",
    "utils/validation.ts",
];

/// Bare specifiers inside bundled files and the bundled file each one resolves to.
const BARE_IMPORTS: &[(&str, &str)] = &[
    ("marked", "marked/lib/marked.esm.js"),
    ("get-east-asian-width", "get-east-asian-width/index.js"),
    ("partial-json", "partial-json/dist/index.js"),
];

pub fn run(args: &[String], root: &Path) -> Result<(), String> {
    let mut upstream = None;
    let mut out = None;
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--out" => out = Some(PathBuf::from(rest.next().ok_or("--out needs a path")?)),
            flag if flag.starts_with("--") => return Err(format!("vendor-js: unknown flag {flag}")),
            _ if upstream.is_none() => upstream = Some(PathBuf::from(arg)),
            _ => return Err(format!("vendor-js: unexpected argument {arg}")),
        }
    }
    let upstream = upstream.ok_or("vendor-js needs the upstream pi checkout")?;
    let out = out.unwrap_or_else(|| root.join("crates/pi-coding-agent/src/extensions/vendor.json.gz"));
    let files = collect(&upstream)?;
    let json = serde_json::to_vec(&files).map_err(|error| error.to_string())?;
    let mut encoder = GzEncoder::new(Vec::new(), Compression::best());
    encoder.write_all(&json).map_err(|error| error.to_string())?;
    let compressed = encoder.finish().map_err(|error| error.to_string())?;
    std::fs::write(&out, &compressed).map_err(|error| format!("{}: {error}", out.display()))?;
    println!("Wrote {} files ({} KiB compressed) to {}", files.len(), compressed.len() / 1024, out.display());
    Ok(())
}

fn collect(upstream: &Path) -> Result<BTreeMap<String, String>, String> {
    let modules = upstream.join("node_modules");
    let mut files = BTreeMap::new();
    let mut add_tree = |dir: &Path, prefix: &str, keep: &dyn Fn(&str) -> bool| -> Result<(), String> {
        for path in walk(dir)? {
            let relative = path.strip_prefix(dir).map_err(|error| error.to_string())?;
            let relative = relative.to_string_lossy().replace('\\', "/");
            if keep(&relative) {
                files.insert(format!("{prefix}/{relative}"), read(&path)?);
            }
        }
        Ok(())
    };
    add_tree(&modules.join("typebox/build"), "typebox/build", &|name| name.ends_with(".mjs"))?;
    add_tree(&upstream.join("packages/tui/src"), "pi-tui/src", &|name| {
        name.ends_with(".ts") && !name.ends_with(".d.ts")
    })?;
    let mut add = |source: PathBuf, key: &str| -> Result<(), String> {
        files.insert(key.to_string(), read(&source)?);
        Ok(())
    };
    add(modules.join("typebox/license"), "typebox/LICENSE")
        .or_else(|_| add(modules.join("typebox/LICENSE"), "typebox/LICENSE"))?;
    add(modules.join("marked/lib/marked.esm.js"), "marked/lib/marked.esm.js")?;
    add(modules.join("marked/LICENSE"), "marked/LICENSE")?;
    for name in ["index.js", "lookup.js", "lookup-data.js", "utilities.js", "license"] {
        add(modules.join("get-east-asian-width").join(name), &format!("get-east-asian-width/{name}"))?;
    }
    for name in ["dist/index.js", "dist/options.js", "LICENSE"] {
        add(modules.join("partial-json").join(name), &format!("partial-json/{name}"))?;
    }
    for name in PI_AI_FILES {
        add(upstream.join("packages/ai/src").join(name), &format!("pi-ai/src/{name}"))?;
    }
    add(upstream.join("LICENSE"), "pi/LICENSE")?;
    Ok(files.into_iter().map(|(key, source)| (key.clone(), rewrite_bare_imports(&key, &source))).collect())
}

fn read(path: &Path) -> Result<String, String> {
    std::fs::read_to_string(path).map_err(|error| format!("{}: {error}", path.display()))
}

fn walk(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let mut found = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(next) = pending.pop() {
        let entries = std::fs::read_dir(&next).map_err(|error| format!("{}: {error}", next.display()))?;
        for entry in entries {
            let path = entry.map_err(|error| error.to_string())?.path();
            if path.is_dir() {
                pending.push(path);
            } else {
                found.push(path);
            }
        }
    }
    found.sort();
    Ok(found)
}

/// Replace `"<bare>"` import specifiers with the bundled file's path relative to `key`.
fn rewrite_bare_imports(key: &str, source: &str) -> String {
    let up = "../".repeat(key.matches('/').count());
    let mut text = source.to_string();
    for (bare, target) in BARE_IMPORTS {
        for (open, close) in [("from \"", "\""), ("import(\"", "\")"), ("require(\"", "\")")] {
            text = text.replace(&format!("{open}{bare}{close}"), &format!("{open}{up}{target}{close}"));
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_imports_become_relative_paths() {
        let source = "import { Marked } from \"marked\";\nexport { x } from \"./x.ts\";";
        let rewritten = rewrite_bare_imports("pi-tui/src/components/markdown.ts", source);
        assert!(rewritten.contains("from \"../../../marked/lib/marked.esm.js\""), "{rewritten}");
        assert!(rewritten.contains("from \"./x.ts\""));
    }
}
