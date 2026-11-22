//! Module resolution and loading for the Node backend's module hooks. Resolution matches the
//! QuickJS loader; virtual modules stand as files under the client's virtual directory.

use std::path::{Path, PathBuf};

use oxc_allocator::Allocator;
use oxc_parser::Parser;
use oxc_span::SourceType;
use serde_json::{Value, json};

use super::client::NodeClient;
use super::import_meta::lazy_import_meta;
use crate::error::JsError;
use crate::modules::namespace;
use crate::resolve::ResolveKind;
use crate::runtime::state::Shared;
use crate::transpile::ModuleFormat;
use crate::virtual_modules::VirtualModule;

const PI_HOST_SOURCE: &str = "const host = globalThis[Symbol.for(\"pi.host\")];\n\
     export default host;\nexport const { call, callAsync, has, names } = host;\n";

fn encode(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' { c.to_string() } else { format!("_{:x}_", c as u32) })
        .collect()
}

fn virtual_path(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!("{}.mjs", encode(name)))
}

/// The file standing for virtual module `name` in namespace `id`. Node keys `require()` of an ES
/// module by path, so a namespace's virtual module needs a path of its own, not a URL query.
fn namespaced_virtual_path(dir: &Path, name: &str, id: &str) -> PathBuf {
    dir.join(format!("{}.{id}.mjs", encode(name)))
}

/// The file standing for `path` in namespace `id`. Node keys modules by path, so a namespace's copy
/// of a file has a path of its own; the module's code sees its original location.
fn namespaced_file_path(dir: &Path, id: &str, path: &Path) -> Option<PathBuf> {
    let parent: String = (path.parent()?.as_os_str().as_encoded_bytes().iter())
        .map(|&b| if b.is_ascii_alphanumeric() || b == b'-' { char::from(b).to_string() } else { format!("_{b:x}_") })
        .collect();
    Some(dir.join("ns").join(id).join(parent).join(path.file_name()?))
}

fn is_alias(dir: &Path, path: &Path) -> bool {
    path.starts_with(dir.join("ns")) || namespaced_virtual(dir, path).is_some()
}

fn virtual_name(dir: &Path, path: &Path) -> Option<String> {
    if path.starts_with(dir.join("ns")) {
        return None;
    }
    let stem = path.strip_prefix(dir).ok()?.to_str()?.strip_suffix(".mjs")?;
    decode(stem)
}

/// `(namespace, name)` for a path made by `namespaced_virtual_path`.
fn namespaced_virtual(dir: &Path, path: &Path) -> Option<(String, String)> {
    let stem = path.strip_prefix(dir).ok()?.to_str()?.strip_suffix(".mjs")?;
    let (encoded, id) = stem.split_once('.')?;
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        return None;
    }
    Some((id.to_string(), decode(encoded)?))
}

fn decode(stem: &str) -> Option<String> {
    if stem.contains('.') {
        return None;
    }
    let mut name = String::new();
    let mut parts = stem.split('_');
    name.push_str(parts.next()?);
    while let (Some(code), Some(rest)) = (parts.next(), parts.next()) {
        name.push(char::from_u32(u32::from_str_radix(code, 16).ok()?)?);
        name.push_str(rest);
    }
    Some(name)
}

fn file_url(path: &Path) -> Result<String, JsError> {
    url::Url::from_file_path(path)
        .map(|url| url.to_string())
        .map_err(|()| JsError::new(format!("cannot make a file URL of {}", path.display())))
}

// Node 22's require(ESM) asserts on Windows drive paths before it calls the resolve hook.
fn file_specifiers(source: &str) -> Result<String, JsError> {
    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, source, SourceType::mjs()).parse();
    if !parsed.diagnostics.is_empty() {
        return Err(JsError::with_name("SyntaxError", parsed.diagnostics[0].message.to_string()));
    }
    let mut edits = Vec::new();
    for (specifier, requests) in &parsed.module_record.requested_modules {
        if Path::new(specifier.as_str()).is_absolute() {
            let quoted = serde_json::to_string(&file_url(Path::new(specifier.as_str()))?).unwrap();
            for request in requests {
                edits.push((request.span, quoted.clone()));
            }
        }
    }
    edits.sort_by_key(|(span, _)| std::cmp::Reverse(span.start));
    let mut source = source.to_string();
    for (span, quoted) in edits {
        source.replace_range(span.start as usize..span.end as usize, &quoted);
    }
    Ok(source)
}

fn quote(text: &str) -> String {
    serde_json::to_string(text).unwrap_or_default()
}

/// `source` with `prefix` after its hashbang line and directive prologue, on the same line.
fn with_prefix(source: &str, prefix: &str, source_type: SourceType) -> String {
    let mut at = 0;
    if source.starts_with("#!") {
        let end = source.find(['\n', '\r', '\u{2028}', '\u{2029}']).unwrap_or(source.len());
        let rest = &source[end..];
        at = end + if rest.starts_with("\r\n") { 2 } else { rest.chars().next().map_or(0, char::len_utf8) };
    }
    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, source, source_type).parse();
    if parsed.diagnostics.is_empty()
        && let Some(directive) = parsed.program.directives.last()
    {
        at = at.max(directive.span.end as usize);
    }
    format!("{};{prefix}{}", &source[..at], &source[at..])
}

/// What sets a namespaced ES module's original location, which its alias path hides from Node.
fn esm_location(path: &Path, id: &str) -> Result<String, JsError> {
    let file = quote(&path.display().to_string());
    let dir = quote(&path.parent().unwrap_or(path).display().to_string());
    let (url, id) = (quote(&namespace::url_in(&file_url(path)?, id)), quote(id));
    Ok(format!(
        "import.meta.url = {url}; import.meta.filename = {file}; import.meta.dirname = {dir}; \
         import.meta.resolve = {NAMESPACES}.metaResolve({file}, {id}); "
    ))
}

fn cjs_location(path: &Path, id: &str) -> String {
    let file = quote(&path.display().to_string());
    let dir = quote(&path.parent().unwrap_or(path).display().to_string());
    format!("__filename = {file}; __dirname = {dir}; require = {NAMESPACES}.requireOf(module, {file}, {}); ", quote(id))
}

const NAMESPACES: &str = "globalThis[Symbol.for(\"pi.namespaces\")]";

/// `{ url, ns, path }` for a specifier imported (or required) by `parent` in `namespace`. A module of
/// a namespace has the alias path `url` and the original `path`; `ns` names the namespace.
pub(crate) fn resolve(
    shared: &Shared,
    client: &NodeClient,
    specifier: &str,
    parent: Option<&str>,
    kind: &str,
    namespace: Option<&str>,
) -> Result<Value, JsError> {
    let dir = &client.virtual_dir;
    let virtual_in = |name: &str, id: &str| -> Result<Value, JsError> {
        let path = virtual_path(dir, name).display().to_string();
        Ok(json!({ "url": file_url(&namespaced_virtual_path(dir, name, id))?, "ns": id, "path": path }))
    };
    if let Some(name) = virtual_name(dir, Path::new(specifier)) {
        return match namespace {
            Some(id) if name != "pi:host" && namespace::lookup(shared, id)?.owns(&name) => virtual_in(&name, id),
            _ => Ok(json!({ "url": file_url(Path::new(specifier))? })),
        };
    }
    let kind = if kind == "require" { ResolveKind::Require } else { ResolveKind::Import };
    let inline =
        |key: &str| client.inline_sources.read().unwrap_or_else(|poison| poison.into_inner()).contains_key(key);
    let scoped = namespace::resolve_scoped(shared, &shared.config.cwd, &inline, specifier, parent, kind, namespace)?;
    let key = scoped.key;
    Ok(match (scoped.kind, scoped.namespace.as_deref()) {
        ("builtin", _) if key == "pi:host" => json!({ "url": file_url(&virtual_path(dir, &key))? }),
        ("builtin", _) => json!({ "url": key }),
        ("virtual", Some(id)) => virtual_in(&key, id)?,
        ("virtual", None) => json!({ "url": file_url(&virtual_path(dir, &key))? }),
        // A native addon loads once per process, so it cannot be a namespace's own.
        (_, Some(id)) if !key.ends_with(".node") => {
            let alias = namespaced_file_path(dir, id, Path::new(&key))
                .ok_or_else(|| JsError::new(format!("cannot load {key} in module namespace {id}")))?;
            json!({ "url": file_url(&alias)?, "ns": id, "path": key })
        }
        _ => json!({ "url": file_url(Path::new(&key))? }),
    })
}

/// `{ format, source }` for files Rust provides or transpiles; `null` lets Node load the file. A
/// module of `namespace`, at its original `path`, loads here and sees that path as its own.
pub(crate) fn load(
    shared: &Shared,
    client: &NodeClient,
    path: &str,
    namespace: Option<&str>,
) -> Result<Value, JsError> {
    if let Some(id) = namespace {
        namespace::lookup(shared, id)?;
    }
    let path = Path::new(path);
    let dir = &client.virtual_dir;
    if is_alias(dir, path) {
        let message = format!("Cannot find module '{}'", path.display());
        return Err(JsError::new(message).with_code("ERR_MODULE_NOT_FOUND"));
    }
    if let Some(name) = virtual_name(dir, path) {
        let source = file_specifiers(&virtual_source(shared, namespace, &name)?)?;
        let source = match namespace {
            Some(id) => lazy_import_meta(&source, &esm_location(path, id)?)?,
            None => source,
        };
        return Ok(json!({ "format": "module", "source": source }));
    }
    let key = path.display().to_string();
    let inline = client.inline_sources.read().unwrap_or_else(|poison| poison.into_inner()).get(&key).cloned();
    let source = match inline.or_else(|| crate::modules::registered_source(shared, &key)) {
        Some(source) => source.to_string(),
        None if namespace.is_some() || crate::transpile::needs_transform(path) => {
            std::fs::read_to_string(path).map_err(|error| JsError::from_io(&error, "open", Some(&key)))?
        }
        None => return Ok(Value::Null),
    };
    if path.extension().is_some_and(|ext| ext == "json") {
        return Ok(json!({ "format": "json", "source": source }));
    }
    let transpiled = shared.transpiler.get_or_transpile(path, &source)?;
    Ok(match transpiled.format {
        ModuleFormat::Esm => {
            let mut prefix = String::new();
            let globals = &transpiled.free_globals;
            if let (true, Some(id)) = (globals.require, namespace) {
                prefix.push_str(&format!(
                    "const require = {NAMESPACES}.requireIn(import.meta.filename, {}); ",
                    quote(id)
                ));
            } else if globals.require {
                prefix.push_str(
                    "import { createRequire as __pi_createRequire } from \"node:module\"; \
                     const require = __pi_createRequire(import.meta.url); ",
                );
            }
            if globals.filename {
                prefix.push_str("const __filename = import.meta.filename; ");
            }
            if globals.dirname {
                prefix.push_str("const __dirname = import.meta.dirname; ");
            }
            let source = file_specifiers(&transpiled.code)?;
            let source = match namespace {
                Some(id) => {
                    let source =
                        if prefix.is_empty() { source } else { with_prefix(&source, &prefix, SourceType::mjs()) };
                    lazy_import_meta(&source, &esm_location(path, id)?)?
                }
                None => format!("{prefix}{source}"),
            };
            json!({ "format": "module", "source": source })
        }
        ModuleFormat::CommonJs => {
            let source = match namespace {
                Some(id) => with_prefix(&transpiled.code, &cjs_location(path, id), SourceType::cjs()),
                None => transpiled.code.to_string(),
            };
            json!({ "format": "commonjs", "source": source })
        }
    })
}

fn virtual_source(shared: &Shared, namespace: Option<&str>, name: &str) -> Result<String, JsError> {
    if name == "pi:host" {
        return Ok(PI_HOST_SOURCE.into());
    }
    match namespace::virtual_module(shared, namespace, name)? {
        Some(VirtualModule::Source(source)) => Ok(source),
        Some(VirtualModule::Json(value)) => Ok(json_module(&value)),
        Some(VirtualModule::Native(_)) => Err(JsError::new(format!(
            "virtual module {name} is built in Rust for QuickJS and cannot load on the Node backend"
        ))
        .with_code("ERR_PI_NODE_BACKEND")),
        None => Err(JsError::new(format!("Cannot find module '{name}'")).with_code("ERR_MODULE_NOT_FOUND")),
    }
}

fn json_module(value: &Value) -> String {
    let mut source = format!("const value = {value};\nexport default value;\n");
    if let Value::Object(map) = value {
        for (index, key) in map.keys().filter(|key| key.as_str() != "default").enumerate() {
            let quoted = serde_json::to_string(key).unwrap_or_default();
            source.push_str(&format!("const e{index} = value[{quoted}];\nexport {{ e{index} as {quoted} }};\n"));
        }
    }
    source
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn virtual_names_round_trip_through_paths() {
        let dir = PathBuf::from("/tmp/pi-virtual");
        for name in ["pi:host", "@earendil-works/pi-ai/compat", "typebox", "a_b"] {
            assert_eq!(virtual_name(&dir, &virtual_path(&dir, name)).as_deref(), Some(name));
        }
    }

    #[test]
    fn namespaced_virtual_paths_stay_apart_from_plain_ones() {
        let dir = PathBuf::from("/tmp/pi-virtual");
        for name in ["pi:runner", "@earendil-works/pi-coding-agent", "a.b", "a_b"] {
            let path = namespaced_virtual_path(&dir, name, "child-1_x");
            assert_eq!(namespaced_virtual(&dir, &path), Some(("child-1_x".to_string(), name.to_string())));
            assert_eq!(virtual_name(&dir, &path), None);
            assert_eq!(namespaced_virtual(&dir, &virtual_path(&dir, name)), None);
        }
        assert_eq!(namespaced_virtual(&dir, &dir.join("pi_3a_runner.a.b.mjs")), None);
    }

    #[test]
    fn namespaced_files_have_paths_of_their_own() {
        let dir = PathBuf::from("/tmp/pi-virtual");
        let alias = namespaced_file_path(&dir, "a", Path::new("/w/x_y/m.cjs")).unwrap();
        assert_eq!(alias, dir.join("ns").join("a").join("_2f_w_2f_x_5f_y").join("m.cjs"));
        assert_ne!(alias, namespaced_file_path(&dir, "a", Path::new("/w/x/y/m.cjs")).unwrap());
        assert_ne!(alias, namespaced_file_path(&dir, "b", Path::new("/w/x_y/m.cjs")).unwrap());
        assert!(is_alias(&dir, &alias) && virtual_name(&dir, &alias).is_none());
        assert!(!is_alias(&dir, &virtual_path(&dir, "pi:runner")));
    }

    #[test]
    fn prefixes_follow_the_hashbang_and_directives() {
        let cjs = SourceType::cjs();
        assert_eq!(with_prefix("f();", "p;", cjs), ";p;f();");
        assert_eq!(with_prefix("#!/bin/node\r\n'use strict'\nf();", "p;", cjs), "#!/bin/node\r\n'use strict';p;\nf();");
        assert_eq!(with_prefix("\"use strict\"; \"x\";\nf();", "p;", cjs), "\"use strict\"; \"x\";;p;\nf();");
        assert_eq!(with_prefix("#!/bin/node\r\nf();", "p;", cjs), "#!/bin/node\r\n;p;f();");
        assert_eq!(with_prefix("#!/bin/node\nimport 'a';", "p;", SourceType::mjs()), "#!/bin/node\n;p;import 'a';");
        assert_eq!(with_prefix("'use strict' +;", "p;", cjs), ";p;'use strict' +;");
    }

    #[test]
    fn absolute_module_specifiers_use_file_urls() {
        let path = std::env::current_dir().unwrap().join("vendor space/#entry.ts");
        let quoted_path = serde_json::to_string(&path).unwrap();
        let quoted_url = serde_json::to_string(&file_url(&path).unwrap()).unwrap();
        let source = format!(
            "import {{ value }} from {quoted_path};\nexport {{ value }} from {quoted_path};\nexport * from {quoted_path};\nimport {quoted_path};\nconst text = {quoted_path};\nconst load = () => import({quoted_path});\nexport * from './relative.js';\nexport * from 'node:fs';\nexport * from {quoted_url};\n"
        );
        let expected = source.replacen(&quoted_path, &quoted_url, 4);
        assert_eq!(file_specifiers(&source).unwrap(), expected);
        assert_eq!(file_specifiers(&expected).unwrap(), expected);
    }
}
