//! Section 8: runtime exports of the modules upstream gives extensions (`virtual-modules.ts`)
//! against the bundled modules of `crates/pi-coding-agent/src/extensions/modules.rs`.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::Path;

use flate2::read::GzDecoder;
use serde_json::Value;

use super::module_tree::{Exports, Tree, Walker};
use super::report::{Item, Section};
use super::scan::{Lang, group_after, literal_at, member_names, skip_lexeme, top_level_literals};
use super::sections::{build, files_under, read_local, read_upstream};

const UPSTREAM_TABLE: &str = "packages/coding-agent/src/core/extensions/virtual-modules.ts";
const EXTENSIONS: &str = "crates/pi-coding-agent/src/extensions";
const ALIASES: &str = "crates/pi-js/src/virtual_modules.rs";

/// Module names in table order, grouped by the module instance they share.
pub fn upstream_modules(src: &str) -> Vec<(String, Vec<String>)> {
    let mut specs = BTreeMap::new();
    for line in src.lines().map(str::trim) {
        if let Some((name, spec)) = line.strip_prefix("import * as ").and_then(|rest| rest.split_once(" from ")) {
            specs.insert(name.trim().to_string(), spec.trim_end_matches(';').trim_matches('"').to_string());
        }
    }
    let Some(at) = src.find("VIRTUAL_MODULES") else { return Vec::new() };
    let body = group_after(src, at + src[at..].find('=').unwrap_or(0), b'{', Lang::Ts).unwrap_or("");
    let mut groups: Vec<(String, Vec<String>)> = Vec::new();
    for line in body.lines().map(str::trim).filter(|line| !line.starts_with("//")) {
        let Some((name, value)) = line.rsplit_once(':') else { continue };
        let Some(spec) = specs.get(value.trim().trim_end_matches(',')) else { continue };
        let name = name.trim().trim_matches('"').to_string();
        match groups.iter_mut().find(|(group_spec, _)| group_spec == spec) {
            Some((_, names)) => names.push(name),
            None => groups.push((spec.clone(), vec![name])),
        }
    }
    groups
}

fn pick(target: &Value) -> Option<&str> {
    match target {
        Value::String(text) => Some(text),
        Value::Object(map) => ["source", "import", "default"].iter().find_map(|key| pick(map.get(*key)?)),
        _ => None,
    }
}

/// Bare specifier patterns for the workspace packages, pointing at their TypeScript sources.
pub fn package_patterns(upstream: &Path) -> Vec<(String, String)> {
    let mut patterns = Vec::new();
    let Ok(dirs) = std::fs::read_dir(upstream.join("packages")) else { return patterns };
    for package in dirs.flatten().map(|entry| entry.path()) {
        let dir = package.join("package.json");
        let Ok(text) = std::fs::read_to_string(&dir) else { continue };
        let Ok(json) = serde_json::from_str::<Value>(&text) else { continue };
        let Some(name) = json.get("name").and_then(Value::as_str) else { continue };
        let rel = package.strip_prefix(upstream).map(|path| path.to_string_lossy().replace('\\', "/"));
        let Ok(rel) = rel else { continue };
        let mut entries: Vec<(String, String)> = match json.get("exports").and_then(Value::as_object) {
            Some(map) => {
                map.iter().filter_map(|(sub, target)| Some((sub.clone(), pick(target)?.to_string()))).collect()
            }
            None => Vec::new(),
        };
        if let Some(main) = json.get("main").and_then(Value::as_str).filter(|_| entries.is_empty()) {
            entries.push((".".into(), main.to_string()));
        }
        for (sub, target) in entries {
            let target = target.trim_start_matches("./");
            let source = target.strip_prefix("dist/").map(|rest| format!("src/{rest}"));
            let source = source.unwrap_or_else(|| target.to_string());
            let source = source.strip_suffix(".js").map_or(source.clone(), |stem| format!("{stem}.ts"));
            patterns.push((format!("{name}{}", sub.trim_start_matches('.')), format!("{rel}/{source}")));
        }
    }
    patterns
}

fn vendor_files(root: &Path) -> Result<BTreeMap<String, String>, String> {
    let path = root.join(EXTENSIONS).join("vendor.json.gz");
    let bytes = std::fs::read(&path).map_err(|error| format!("{}: {error}", path.display()))?;
    let mut json = Vec::new();
    GzDecoder::new(bytes.as_slice()).read_to_end(&mut json).map_err(|error| format!("vendor.json.gz: {error}"))?;
    serde_json::from_slice(&json).map_err(|error| format!("vendor.json.gz: {error}"))
}

/// Replace `{}` placeholders of a `format!` string with `/<key>` of each `path("key")` argument.
fn format_source(args: &str) -> Option<String> {
    let at = args.find("format!(")? + "format!(".len();
    let start = at + args[at..].find('"')?;
    let template = literal_at(args, start, Lang::Rust)?;
    let end = skip_lexeme(args, start, Lang::Rust)?;
    let mut keys = args[end..].split("path(\"").skip(1).filter_map(|rest| rest.split('"').next());
    let template = template.replace("\\\"", "\"").replace("\\n", "\n");
    let (mut out, mut rest) = (String::new(), template.as_str());
    while let Some(at) = rest.find(['{', '}']) {
        out.push_str(&rest[..at]);
        let pair = rest.get(at..at + 2).unwrap_or("");
        match pair {
            "{{" => out.push('{'),
            "}}" => out.push('}'),
            "{}" => out.push_str(&format!("/{}", keys.next()?)),
            _ => return None,
        }
        rest = &rest[at + 2..];
    }
    out.push_str(rest);
    Some(out)
}

fn literals(text: &str) -> Vec<String> {
    let (mut found, mut i) = (Vec::new(), 0);
    while i < text.len() {
        if let Some(literal) = literal_at(text, i, Lang::Rust) {
            found.push(literal.to_string());
        }
        i = skip_lexeme(text, i, Lang::Rust).unwrap_or(i + 1);
    }
    found
}

/// The source of one `modules.insert(...)` argument list, or `None` for a module built in Rust.
pub(super) fn module_source(args: &str) -> Option<String> {
    if let Some(at) = args.find("json!(") {
        let names = member_names(group_after(args, at, b'{', Lang::Rust).unwrap_or(""));
        return Some(names.iter().map(|name| format!("export const {name} = 0;\n")).collect());
    }
    if args.contains("format!(") {
        return format_source(args);
    }
    let keys = literals(args).into_iter().skip(1);
    let keys: Vec<String> = keys.filter(|key| [".js", ".ts", ".mjs"].iter().any(|ext| key.ends_with(ext))).collect();
    (!keys.is_empty()).then(|| keys.iter().map(|key| format!("export * from \"/{key}\";\n")).collect())
}

/// Every name an extension can import from pi-rust, and the local tree behind it.
fn local_tree(root: &Path) -> Result<(Tree, BTreeSet<String>, Vec<String>), String> {
    let modules = read_local(root, &format!("{EXTENSIONS}/modules.rs"))?;
    let mut tree = Tree { files: vendor_files(root)?, ..Tree::default() };
    let host = root.join(EXTENSIONS).join("js");
    for path in files_under(&host, &["js", "mjs"]) {
        let rel = path.strip_prefix(&host).map_err(|error| error.to_string())?.to_string_lossy().replace('\\', "/");
        tree.files.insert(format!("host/{rel}"), std::fs::read_to_string(&path).map_err(|error| error.to_string())?);
    }
    let (mut names, mut opaque) = (BTreeSet::new(), Vec::new());
    for (at, _) in modules.match_indices("modules.insert(") {
        let Some(args) = group_after(&modules, at, b'(', Lang::Rust) else { continue };
        let Some(name) = top_level_literals(args, Lang::Rust).into_iter().next() else { continue };
        let key = format!("virtual/{name}.js");
        match module_source(args) {
            Some(source) => _ = tree.files.insert(key.clone(), source),
            None => opaque.push(name.clone()),
        }
        tree.bare.push((name.clone(), key));
        names.insert(name);
    }
    if names.is_empty() {
        return Err(format!("no modules.insert(...) calls in {EXTENSIONS}/modules.rs"));
    }
    let aliases = read_local(root, ALIASES)?;
    if let Some(at) = aliases.find("VIRTUAL_MODULE_ALIASES: ") {
        let body = group_after(&aliases, at + aliases[at..].find('=').unwrap_or(0), b'[', Lang::Rust).unwrap_or("");
        for pair in body.split(')').filter_map(|group| group.split_once('(').map(|(_, inner)| inner)) {
            if let [alias, canonical] = top_level_literals(pair, Lang::Rust).as_slice() {
                tree.bare.push((alias.clone(), format!("virtual/{canonical}.js")));
                names.insert(alias.clone());
            }
        }
    }
    Ok((tree, names, opaque))
}

fn upstream_tree(upstream: &Path) -> Tree {
    Tree { disk: Some(upstream.to_path_buf()), bare: package_patterns(upstream), ..Tree::default() }
}

fn compare(section: &mut Section, module: &str, up: &Exports, local: &Exports) {
    for (name, origin) in up.iter().filter(|(name, _)| !local.contains_key(*name)) {
        let item = Item::new(format!("{module}: {name}")).detail(format!("upstream `{}`", origin.file));
        section.missing.push(Item { group: Some(module.to_string()), ..item });
    }
    for (name, origin) in local.iter().filter(|(name, _)| !up.contains_key(*name)) {
        let item = Item::new(format!("{module}: {name}")).detail(format!("pi-rust `{}`", origin.file));
        section.extra.push(Item { group: Some(module.to_string()), ..item });
    }
}

pub fn section(upstream: &Path, root: &Path) -> Section {
    build("moduleExports", "Extension module exports", |section| {
        let groups = upstream_modules(&read_upstream(upstream, UPSTREAM_TABLE)?);
        if groups.is_empty() {
            return Err(format!("no VIRTUAL_MODULES table in {UPSTREAM_TABLE}"));
        }
        let (local_tree, local_names, opaque) = local_tree(root)?;
        let canonical: BTreeSet<&String> = local_tree
            .files
            .keys()
            .filter_map(|key| key.strip_prefix("virtual/")?.strip_suffix(".js"))
            .filter_map(|name| local_names.get(name))
            .collect();
        let up_tree = upstream_tree(upstream);
        let (mut up_walk, mut local_walk) = (Walker::new(&up_tree), Walker::new(&local_tree));
        let mut unsupported = Vec::new();
        for (spec, names) in &groups {
            let module = names.iter().find(|name| canonical.contains(*name)).unwrap_or(&names[0]);
            for name in names.iter().filter(|name| !local_names.contains(*name)) {
                let item = Item::new(format!("{name}: (module)")).detail(format!("same module as `{module}` upstream"));
                section.missing.push(Item { group: Some(module.clone()), ..item });
            }
            let Some(up_key) = up_walk.tree().resolve(UPSTREAM_TABLE, spec) else {
                section.notes.push(format!("`{module}` ({spec}) does not resolve upstream, so it is not compared."));
                continue;
            };
            let Some(local_name) = names.iter().find(|name| local_names.contains(*name)) else { continue };
            if opaque.contains(local_name) {
                section.notes.push(format!("`{module}` is built in Rust, so its exports are not compared."));
                continue;
            }
            let up = up_walk.exports(&up_key);
            let local_key = local_walk.tree().resolve("", local_name).unwrap_or_default();
            let local = local_walk.exports(&local_key);
            let flagged = local.iter().filter(|(_, origin)| origin.unsupported);
            unsupported.extend(flagged.map(|(name, origin)| format!("- `{module}: {name}` (`{}`)", origin.file)));
            section.notes.push(format!(
                "`{module}` (also {}): upstream `{up_key}` has {} runtime exports; pi-rust has {}.",
                names.len() - 1,
                up.len(),
                local.len()
            ));
            compare(section, module, &up, &local);
        }
        section
            .notes
            .push("Type-only exports are left out. Exports that throw `ERR_PI_UNSUPPORTED` (or are `unsupportedFunction`/`unsupportedClass` stubs) count as present and are listed apart.".into());
        for (side, walk) in [("Upstream", &up_walk), ("pi-rust", &local_walk)] {
            if !walk.unresolved.is_empty() {
                let list: Vec<String> = walk.unresolved.iter().map(|spec| format!("`{spec}`")).collect();
                section
                    .notes
                    .push(format!("{side} imports not followed (named ones count as present): {}.", list.join(", ")));
            }
        }
        if !unsupported.is_empty() {
            section.collapsed = Some((
                format!(
                    "Present; the export statement throws ERR_PI_UNSUPPORTED in whole or in part ({})",
                    unsupported.len()
                ),
                unsupported,
            ));
        }
        Ok(())
    })
}
