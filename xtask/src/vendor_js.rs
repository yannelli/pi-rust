//! `cargo xtask vendor-js <upstream-root> [--out <path>]`: bundle the upstream MIT JavaScript the
//! extension host embeds (typebox, pi-tui and its dependencies, pure pi-ai utilities) as gzip JSON
//! of `path -> source`, with bare imports between bundled packages rewritten to relative paths.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use flate2::Compression;
use flate2::read::GzDecoder;
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

const SDK_AI_FILES: &[&str] = &[
    "env-api-keys.ts",
    "utils/provider-env.ts",
    "utils/abort.ts",
    "auth/helpers.ts",
    "auth/context.ts",
    "auth/credential-store.ts",
    "models-store.ts",
    "session-resources.ts",
    "api/lazy.ts",
    "utils/streaming-json.ts",
    "utils/json-parse.ts",
    "utils/identity.ts",
    "utils/version.ts",
    "utils/retry.ts",
    "utils/models-error.ts",
    "models.ts",
    "telemetry.ts",
    "auth/resolve.ts",
    "utils/model-operations.ts",
    "utils/text.ts",
    "utils/transcript.ts",
    "providers/radius-config.ts",
];

const TELEMETRY_FILES: &[&str] = &["index.ts", "memory.ts", "metrics.ts", "noop.ts", "render.ts"];

/// pi-tui files that `--update-sdk` refreshes; the rest of pi-tui stays at the `collect` version.
const TUI_UPDATE_FILES: &[&str] = &["index.ts", "keybindings.ts", "utils.ts"];

const AGENT_CORE_FILES: &[&str] = &["index.ts", "agent.ts", "agent-loop.ts", "proxy.ts", "stream-fn.ts", "types.ts"];

/// Self-contained coding-agent modules the `@earendil-works/pi-coding-agent` shim re-exports.
const CODING_AGENT_FILES: &[&str] = &[
    "core/tools/file-mutation-queue.ts",
    "core/tools/truncate.ts",
    "modes/interactive/components/bordered-loader.ts",
    "modes/interactive/components/custom-editor.ts",
    "modes/interactive/components/dynamic-border.ts",
    "modes/interactive/components/keybinding-hints.ts",
    "modes/interactive/components/visual-truncate.ts",
    "modes/interactive/components/armin.ts",
    "modes/interactive/components/assistant-message.ts",
    "modes/interactive/components/bash-execution.ts",
    "modes/interactive/components/branch-summary-message.ts",
    "modes/interactive/components/compaction-summary-message.ts",
    "modes/interactive/components/countdown-timer.ts",
    "modes/interactive/components/custom-message.ts",
    "modes/interactive/components/extension-input.ts",
    "modes/interactive/components/extension-selector.ts",
    "modes/interactive/components/markdown-transform.ts",
    "modes/interactive/components/oauth-selector.ts",
    "modes/interactive/components/show-images-selector.ts",
    "modes/interactive/components/skill-invocation-message.ts",
    "modes/interactive/components/thinking-selector.ts",
    "modes/interactive/components/user-message.ts",
    "modes/interactive/components/user-message-selector.ts",
    "utils/ansi.ts",
    "core/tools/render-utils.ts",
    "modes/interactive/components/auth-url.ts",
    "modes/interactive/components/login-dialog.ts",
    "modes/interactive/components/session-selector-search.ts",
    "modes/interactive/components/session-selector.ts",
    "modes/interactive/components/theme-selector.ts",
    "modes/interactive/components/tree-selector.ts",
    "utils/open-browser.ts",
    "utils/paths.ts",
    "utils/shell.ts",
    "core/experimental.ts",
    "core/settings-defaults.ts",
    "core/usage-totals.ts",
    "modes/interactive/components/extension-editor.ts",
    "modes/interactive/components/footer.ts",
    "modes/interactive/components/model-selector.ts",
    "modes/interactive/components/settings-selector.ts",
    "modes/interactive/components/settings-submenu.ts",
    "modes/interactive/components/tool-execution.ts",
    "modes/interactive/model-catalog-refresh.ts",
    "modes/interactive/model-search.ts",
    "utils/abort.ts",
];

/// Bare specifiers inside bundled files and the bundled file each one resolves to.
const BARE_IMPORTS: &[(&str, &str)] = &[
    ("marked", "marked/lib/marked.esm.js"),
    ("get-east-asian-width", "get-east-asian-width/index.js"),
    ("partial-json", "partial-json/dist/index.js"),
    ("@earendil-works/pi-ai", "host/ai.js"),
    ("@earendil-works/pi-telemetry", "pi-telemetry/src/index.ts"),
    ("@earendil-works/pi-ai/utils/retry", "pi-ai/src/utils/retry.ts"),
];

pub fn run(args: &[String], root: &Path) -> Result<(), String> {
    let mut upstream = None;
    let mut out = None;
    let mut update_sdk = false;
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--update-sdk" => update_sdk = true,
            "--out" => out = Some(PathBuf::from(rest.next().ok_or("--out needs a path")?)),
            flag if flag.starts_with("--") => return Err(format!("vendor-js: unknown flag {flag}")),
            _ if upstream.is_none() => upstream = Some(PathBuf::from(arg)),
            _ => return Err(format!("vendor-js: unexpected argument {arg}")),
        }
    }
    let upstream = upstream.ok_or("vendor-js needs the upstream pi checkout")?;
    let out = out.unwrap_or_else(|| root.join("crates/pi-coding-agent/src/extensions/vendor.json.gz"));
    let mut files = if update_sdk { read_archive(&out)? } else { collect(&upstream)? };
    add_sdk(&upstream, &mut files)?;
    let json = serde_json::to_vec(&files).map_err(|error| error.to_string())?;
    let mut encoder = GzEncoder::new(Vec::new(), Compression::best());
    encoder.write_all(&json).map_err(|error| error.to_string())?;
    let compressed = encoder.finish().map_err(|error| error.to_string())?;
    std::fs::write(&out, &compressed).map_err(|error| format!("{}: {error}", out.display()))?;
    println!("Wrote {} files ({} KiB compressed) to {}", files.len(), compressed.len() / 1024, out.display());
    Ok(())
}

fn read_archive(path: &Path) -> Result<BTreeMap<String, String>, String> {
    let file = std::fs::File::open(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let mut decoded = String::new();
    GzDecoder::new(file).read_to_string(&mut decoded).map_err(|error| error.to_string())?;
    serde_json::from_str(&decoded).map_err(|error| error.to_string())
}

fn add_sdk(upstream: &Path, files: &mut BTreeMap<String, String>) -> Result<(), String> {
    let packages = [
        ("ai", "pi-ai", SDK_AI_FILES),
        ("agent", "pi-agent-core", AGENT_CORE_FILES),
        ("telemetry", "pi-telemetry", TELEMETRY_FILES),
        ("tui", "pi-tui", TUI_UPDATE_FILES),
        ("coding-agent", "pi-coding-agent", CODING_AGENT_FILES),
    ];
    for (package, prefix, names) in packages {
        for name in names {
            let key = format!("{prefix}/src/{name}");
            let source = read(&upstream.join("packages").join(package).join("src").join(name))?;
            files.insert(key.clone(), rewrite_bare_imports(&key, &source));
        }
    }
    for (path, key, names) in crate::vendor_extract::GENERATED {
        let source = read(&upstream.join(path))?;
        files.insert(key.to_string(), crate::vendor_extract::extract(path, &source, names)?);
    }
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
    for name in CODING_AGENT_FILES {
        add(upstream.join("packages/coding-agent/src").join(name), &format!("pi-coding-agent/src/{name}"))?;
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

    #[test]
    fn agent_core_imports_the_host_ai_module() {
        let source = "import { EventStream } from \"@earendil-works/pi-ai\";";
        let rewritten = rewrite_bare_imports("pi-agent-core/src/agent-loop.ts", source);
        assert!(rewritten.contains("from \"../../host/ai.js\""));
    }

    #[test]
    fn the_archive_holds_the_generated_constants_modules() {
        let archive =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../crates/pi-coding-agent/src/extensions/vendor.json.gz");
        let files = read_archive(&archive).unwrap();
        for (_, key, names) in crate::vendor_extract::GENERATED {
            let module = &files[*key];
            assert!(module.starts_with("// Generated by `cargo xtask vendor-js`"), "{key}");
            assert!(!module.contains("import ") && !module.contains("undici"), "{key}");
            for name in *names {
                assert!(
                    module.contains(&format!("export const {name} "))
                        || module.contains(&format!("export function {name}(")),
                    "{key}: {name}"
                );
            }
        }
    }

    #[test]
    fn ai_telemetry_imports_the_bundled_telemetry_package() {
        let source = "import { defineTelemetrySchema } from \"@earendil-works/pi-telemetry\";";
        let rewritten = rewrite_bare_imports("pi-ai/src/telemetry.ts", source);
        assert!(rewritten.contains("from \"../../pi-telemetry/src/index.ts\""), "{rewritten}");
    }
}
