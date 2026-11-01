//! Generated modules for `vendor-js`: named top-level declarations of an upstream file, copied
//! verbatim with the imports they use, where the rest of the file needs packages the host does not bundle.

use std::collections::BTreeMap;

/// Upstream file, archive key, and the declarations the vendored components import from it.
pub const GENERATED: &[(&str, &str, &[&str])] = &[
    (
        "packages/coding-agent/src/core/settings-manager.ts",
        "pi-coding-agent/src/core/settings-manager.ts",
        &["CACHE_WARMING_MODES"],
    ),
    (
        "packages/coding-agent/src/core/http-dispatcher.ts",
        "pi-coding-agent/src/core/http-dispatcher.ts",
        &[
            "DEFAULT_HTTP_IDLE_TIMEOUT_MS",
            "HTTP_IDLE_TIMEOUT_CHOICES",
            "parseHttpIdleTimeoutMs",
            "formatHttpIdleTimeoutMs",
        ],
    ),
    (
        "packages/coding-agent/src/core/trust-manager.ts",
        "pi-coding-agent/src/core/trust-manager.ts",
        &["TRUST_REQUIRING_PROJECT_CONFIG_RESOURCES", "hasTrustRequiringProjectResources"],
    ),
    (
        "packages/coding-agent/src/core/footer-data-provider.ts",
        "pi-coding-agent/src/core/footer-data-provider.ts",
        &["findGitPaths"],
    ),
    (
        "packages/coding-agent/src/core/resource-loader.ts",
        "pi-coding-agent/src/core/resource-loader.ts",
        &["loadContextFileFromDir", "findShadowedContextFile", "loadProjectContextFiles"],
    ),
];

/// Node builtins that upstream imports without the `node:` prefix.
const BARE_NODE_BUILTINS: [&str; 2] = ["fs", "path"];

const DECLARATION_KINDS: [&str; 5] = ["const", "let", "var", "function", "class"];

fn is_ident(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '$'
}

fn words(text: &str) -> impl Iterator<Item = &str> {
    text.split(|c: char| !is_ident(c)).filter(|word| !word.is_empty())
}

/// The name a top-level line declares, as `(name, is_function)`.
fn declared(line: &str) -> Option<(&str, bool)> {
    let rest = line.strip_prefix("export ").unwrap_or(line);
    let rest = rest.strip_prefix("async ").unwrap_or(rest);
    let kind = DECLARATION_KINDS.iter().find(|kind| rest.starts_with(&format!("{kind} ")))?;
    let name = rest[kind.len() + 1..].split(|c: char| !is_ident(c)).next().filter(|name| !name.is_empty())?;
    Some((name, matches!(*kind, "function" | "class")))
}

/// One runtime binding of an import statement: the clause entry that names it (`x`, `* as x`, `a as x`), whether
/// it sits in braces, its local name, and the statement's index and specifier.
struct Binding {
    entry: String,
    named: bool,
    local: String,
    statement: usize,
    specifier: String,
}

/// Runtime bindings of the file's imports; `import type` and `type` entries are left out.
fn imported(source: &str) -> Vec<Binding> {
    let mut bindings = Vec::new();
    let statements = source.split(';').map(str::trim).filter(|text| text.starts_with("import "));
    for (statement, text) in statements.enumerate() {
        let Some((clause, from)) = text["import ".len()..].split_once(" from ") else { continue };
        if clause.starts_with("type ") {
            continue;
        }
        let specifier = from.trim().trim_matches(['"', '\'']).to_string();
        let (default, braces) = match clause.split_once('{') {
            Some((before, inside)) => (before, inside.trim_end_matches('}')),
            None => (clause, ""),
        };
        let defaults = default.split(',').map(|part| (part.trim(), false));
        let entries =
            braces.split(',').map(|entry| (entry.trim(), true)).filter(|(entry, _)| !entry.starts_with("type "));
        for (entry, named) in defaults.chain(entries).filter(|(entry, _)| !entry.is_empty()) {
            let Some(local) = entry.rsplit(' ').next() else { continue };
            let (entry, local, specifier) = (entry.to_string(), local.to_string(), specifier.clone());
            bindings.push(Binding { entry, named, local, statement, specifier });
        }
    }
    bindings
}

/// The import statements of `used`, each narrowed to its used bindings, in source order.
fn import_lines(used: &[&Binding]) -> String {
    let mut lines = String::new();
    let mut statements: Vec<usize> = used.iter().map(|binding| binding.statement).collect();
    statements.dedup();
    for statement in statements {
        let bindings: Vec<&&Binding> = used.iter().filter(|binding| binding.statement == statement).collect();
        let mut clause: Vec<String> =
            bindings.iter().filter(|binding| !binding.named).map(|binding| binding.entry.clone()).collect();
        let named: Vec<&str> = bindings.iter().filter(|binding| binding.named).map(|binding| &*binding.entry).collect();
        if !named.is_empty() {
            clause.push(format!("{{ {} }}", named.join(", ")));
        }
        lines.push_str(&format!("import {} from \"{}\";\n", clause.join(", "), bindings[0].specifier));
    }
    lines
}

/// End of the declaration that starts at `start`: the `;` of a variable, or the closing `}` of a function or class
/// body, at bracket depth 0 and the start of a line, so an object type in the signature does not end it.
fn declaration_end(source: &str, start: usize, is_function: bool) -> Option<usize> {
    let bytes = source.as_bytes();
    let (mut depth, mut i) = (0i32, start);
    while i < bytes.len() {
        match bytes[i] {
            b'/' if bytes.get(i + 1) == Some(&b'/') => i = source[i..].find('\n').map_or(bytes.len(), |at| i + at),
            b'/' if bytes.get(i + 1) == Some(&b'*') => i = source[i + 2..].find("*/").map(|at| i + at + 3)?,
            quote @ (b'"' | b'\'') => {
                i += 1;
                while i < bytes.len() && bytes[i] != quote {
                    i += if bytes[i] == b'\\' { 2 } else { 1 };
                }
            }
            b'`' => {
                let mut nested = 0;
                i += 1;
                while i < bytes.len() && (bytes[i] != b'`' || nested > 0) {
                    match bytes[i] {
                        b'\\' => i += 1,
                        b'{' if nested > 0 || bytes[i - 1] == b'$' => nested += 1,
                        b'}' if nested > 0 => nested -= 1,
                        _ => {}
                    }
                    i += 1;
                }
            }
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => {
                depth -= 1;
                if depth == 0 && is_function && bytes[i] == b'}' && (i == 0 || bytes[i - 1] == b'\n') {
                    return Some(i + 1);
                }
            }
            b';' if depth == 0 && !is_function => return Some(i + 1),
            _ => {}
        }
        i += 1;
    }
    None
}

/// `names` from `source` (upstream `file`), in that order, as a module with the imports they use. Fails when a
/// name has no top-level declaration, or a declaration uses an unlisted top-level declaration.
pub fn extract(file: &str, source: &str, names: &[&str]) -> Result<String, String> {
    let mut starts = Vec::new();
    let mut offset = 0;
    for line in source.split_inclusive('\n') {
        if let Some((name, is_function)) = declared(line) {
            starts.push((name, is_function, offset));
        }
        offset += line.len();
    }
    let imports = imported(source);
    let mut used = vec![false; imports.len()];
    let mut declarations = String::new();
    for name in names {
        let &(_, is_function, start) = starts
            .iter()
            .find(|(declared, ..)| declared == name)
            .ok_or_else(|| format!("{file}: no top-level declaration of {name}"))?;
        let end = declaration_end(source, start, is_function)
            .ok_or_else(|| format!("{file}: the declaration of {name} does not end"))?;
        let text = &source[start..end];
        for word in words(text) {
            for (index, binding) in imports.iter().enumerate() {
                used[index] |= binding.local == word;
            }
            if word != *name && !names.contains(&word) && starts.iter().any(|(declared, ..)| *declared == word) {
                return Err(format!("{file}: {name} uses {word}, which is not extracted"));
            }
        }
        declarations.push('\n');
        declarations.push_str(text);
        declarations.push('\n');
    }
    let used: Vec<&Binding> = imports.iter().zip(used).filter(|(_, used)| *used).map(|(binding, _)| binding).collect();
    let header = format!("// Generated by `cargo xtask vendor-js` from upstream {file}: {}.\n", names.join(", "));
    Ok(format!("{header}{}{declarations}", import_lines(&used)))
}

/// Archive keys of the upstream modules the extension host answers with its own (`SHIMS` in `modules.rs`).
pub fn shim_keys(root: &std::path::Path) -> Result<Vec<String>, String> {
    let path = root.join("crates/pi-coding-agent/src/extensions/modules.rs");
    let source = std::fs::read_to_string(&path).map_err(|error| format!("{}: {error}", path.display()))?;
    let table = source.split_once("const SHIMS").and_then(|(_, rest)| rest.split_once("\n];")).map(|(table, _)| table);
    let table = table.ok_or_else(|| format!("{}: no SHIMS table", path.display()))?;
    let literals: Vec<&str> = table.split('"').skip(1).step_by(2).collect();
    Ok(literals.into_iter().step_by(3).map(str::to_string).collect())
}

/// Archive keys of the host modules (`HOST_FILES` in `modules.rs`), such as `host/context_color.js`.
pub fn host_keys(root: &std::path::Path) -> Result<Vec<String>, String> {
    let path = root.join("crates/pi-coding-agent/src/extensions/modules.rs");
    let source = std::fs::read_to_string(&path).map_err(|error| format!("{}: {error}", path.display()))?;
    let table =
        source.split_once("const HOST_FILES").and_then(|(_, rest)| rest.split_once("\n];")).map(|(table, _)| table);
    let table = table.ok_or_else(|| format!("{}: no HOST_FILES table", path.display()))?;
    let names = table.lines().filter_map(|line| line.trim().strip_prefix("(\"")?.split_once('"').map(|(name, _)| name));
    Ok(names.map(|name| format!("host/{name}")).collect())
}

/// Check that every import of generated module `key` resolves: a Node builtin, an archive file, a host shim, or a
/// host module. Run after `rewrite_bare_imports`, so a bare specifier left over is one the archive does not bundle.
pub fn check_imports(key: &str, files: &BTreeMap<String, String>, shims: &[String]) -> Result<(), String> {
    let module = files.get(key).ok_or_else(|| format!("{key}: not generated"))?;
    for line in module.lines().filter(|line| line.starts_with("import ")) {
        let specifier = line.rsplit_once(" from ").map(|(_, from)| from.trim_end_matches(';').trim_matches('"'));
        let specifier = specifier.ok_or_else(|| format!("{key}: cannot read {line}"))?;
        if specifier.starts_with("node:") || BARE_NODE_BUILTINS.contains(&specifier) {
            continue;
        }
        if !specifier.starts_with('.') {
            return Err(format!("{key}: imports {specifier}, which the archive does not bundle"));
        }
        let resolved = resolve(key, specifier);
        if !files.contains_key(&resolved) && !shims.contains(&resolved) {
            return Err(format!("{key}: imports {specifier}, which is neither an archive file nor a host shim"));
        }
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    const SOURCE: &str = r#"import { EventEmitter, once as onceEvent, type Listener } from "node:events";
import * as undici from "undici";
import type { Thing } from "./thing.ts";

export const LIMIT = 5;
export const CHOICES = [
	{ label: "a;b", ms: 1 },
] as const;

function helper(): number {
	return LIMIT;
}

export function format(ms: number): string {
	const choice = CHOICES.find((item) => item.ms === ms);
	if (choice) {
		return choice.label;
	}
	return `${ms / 1000} sec { }`;
}

export function agent() {
	return new undici.Agent();
}

export function viaHelper() {
	return helper();
}

export function waitOnce(emitter: unknown): Promise<Listener> {
	return onceEvent(emitter, "done");
}
"#;

    #[test]
    fn copies_named_declarations_verbatim() {
        let module = extract("x.ts", SOURCE, &["LIMIT", "CHOICES", "format"]).unwrap();
        let expected = "// Generated by `cargo xtask vendor-js` from upstream x.ts: LIMIT, CHOICES, format.\n\nexport const LIMIT = 5;\n\nexport const CHOICES = [\n\t{ label: \"a;b\", ms: 1 },\n] as const;\n\nexport function format(ms: number): string {\n\tconst choice = CHOICES.find((item) => item.ms === ms);\n\tif (choice) {\n\t\treturn choice.label;\n\t}\n\treturn `${ms / 1000} sec { }`;\n}\n";
        assert_eq!(module, expected);
    }

    #[test]
    fn rejects_missing_and_dependent_declarations() {
        assert_eq!(extract("x.ts", SOURCE, &["NOPE"]).unwrap_err(), "x.ts: no top-level declaration of NOPE");
        let error = extract("x.ts", SOURCE, &["viaHelper"]).unwrap_err();
        assert_eq!(error, "x.ts: viaHelper uses helper, which is not extracted");
        assert_eq!(
            extract("x.ts", SOURCE, &["format"]).unwrap_err(),
            "x.ts: format uses CHOICES, which is not extracted"
        );
    }

    #[test]
    fn keeps_only_the_runtime_bindings_the_declarations_use_in_import_order() {
        let module = extract("x.ts", SOURCE, &["agent", "waitOnce"]).unwrap();
        let imports: Vec<&str> = module.lines().filter(|line| line.starts_with("import ")).collect();
        assert_eq!(
            imports,
            ["import { once as onceEvent } from \"node:events\";", "import * as undici from \"undici\";"]
        );
        assert!(module.contains("\nexport function agent() {") && module.contains("\nexport function waitOnce("));
        let plain = extract("x.ts", SOURCE, &["LIMIT"]).unwrap();
        assert!(!plain.contains("import "), "{plain}");
    }

    #[test]
    fn keeps_a_used_default_import_with_its_used_named_aliases() {
        let source = r#"import Default, { a as aliased, b, type T } from "./mixed.ts";
import * as ns from "./ns.ts";
import Unused from "./unused.ts";

export function usesAll(): T {
	return [Default, aliased, ns.x];
}
"#;
        let module = extract("x.ts", source, &["usesAll"]).unwrap();
        let imports: Vec<&str> = module.lines().filter(|line| line.starts_with("import ")).collect();
        assert_eq!(
            imports,
            ["import Default, { a as aliased } from \"./mixed.ts\";", "import * as ns from \"./ns.ts\";"]
        );
    }

    #[test]
    fn checks_that_every_kept_import_resolves() {
        let module = |imports: &str| format!("// Generated\n{imports}\nexport const A = 1;\n");
        let mut files = BTreeMap::new();
        files.insert("p/src/c.ts".to_string(), String::new());
        let shims = vec!["p/src/config.ts".to_string()];
        let ok = "import { a } from \"node:fs\";\nimport { b } from \"../c.ts\";\nimport { C } from \"../config.ts\";";
        files.insert("p/src/core/x.ts".to_string(), module(ok));
        assert_eq!(check_imports("p/src/core/x.ts", &files, &shims), Ok(()));
        files.insert("p/src/core/x.ts".to_string(), module("import * as undici from \"undici\";"));
        let error = check_imports("p/src/core/x.ts", &files, &shims).unwrap_err();
        assert_eq!(error, "p/src/core/x.ts: imports undici, which the archive does not bundle");
        files.insert("p/src/core/x.ts".to_string(), module("import { m } from \"./missing.ts\";"));
        let error = check_imports("p/src/core/x.ts", &files, &shims).unwrap_err();
        assert_eq!(error, "p/src/core/x.ts: imports ./missing.ts, which is neither an archive file nor a host shim");
    }

    #[test]
    fn an_object_type_in_the_signature_does_not_end_a_function() {
        let source = "export function load(options: {\n\tcwd: string;\n}): { path: string } | null {\n\tif (options) {\n\t\treturn { path: \"\" };\n\t}\n\treturn null;\n}\n\nexport const AFTER = 1;\n";
        let module = extract("x.ts", source, &["load"]).unwrap();
        assert!(module.ends_with("\treturn null;\n}\n"), "{module}");
        assert!(!module.contains("AFTER"), "{module}");
    }

    #[test]
    fn accepts_bare_node_builtins_and_host_modules() {
        let mut files = BTreeMap::new();
        let imports = "import { existsSync } from \"fs\";\nimport { join } from \"path\";\nimport chalk from \"../../../host/context_color.js\";";
        files.insert("p/src/core/x.ts".to_string(), format!("// Generated\n{imports}\n"));
        let known = vec!["host/context_color.js".to_string()];
        assert_eq!(check_imports("p/src/core/x.ts", &files, &known), Ok(()));
        files.insert("p/src/core/x.ts".to_string(), "// Generated\nimport { x } from \"os\";\n".to_string());
        let error = check_imports("p/src/core/x.ts", &files, &known).unwrap_err();
        assert_eq!(error, "p/src/core/x.ts: imports os, which the archive does not bundle");
    }

    #[test]
    fn reads_the_host_module_keys_from_modules_rs() {
        let keys = host_keys(std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/.."))).unwrap();
        assert!(keys.iter().any(|key| key == "host/context_color.js"), "{keys:?}");
        assert!(keys.iter().any(|key| key == "host/coding_agent.js"), "{keys:?}");
        assert!(keys.iter().all(|key| key.starts_with("host/") && key.ends_with(".js")), "{keys:?}");
    }

    #[test]
    fn reads_the_host_shim_keys_from_modules_rs() {
        let keys = shim_keys(std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/.."))).unwrap();
        assert!(keys.iter().any(|key| key == "pi-coding-agent/src/config.ts"), "{keys:?}");
        assert!(keys.iter().any(|key| key == "pi-ai/src/providers/images/register-builtins.ts"), "{keys:?}");
        assert!(keys.iter().all(|key| key.ends_with(".ts")), "{keys:?}");
    }
}
