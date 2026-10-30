//! Module resolution and loading for the Node backend's module hooks. Resolution matches the
//! QuickJS loader; virtual modules stand as files under the client's virtual directory.

use std::path::{Path, PathBuf};

use oxc_allocator::Allocator;
use oxc_parser::Parser;
use oxc_span::SourceType;
use serde_json::{Value, json};

use super::client::NodeClient;
use crate::error::JsError;
use crate::resolve::ResolveKind;
use crate::runtime::state::Shared;
use crate::transpile::ModuleFormat;
use crate::virtual_modules::VirtualModule;

const PI_HOST_SOURCE: &str = "const host = globalThis[Symbol.for(\"pi.host\")];\n\
     export default host;\nexport const { call, callAsync, has, names } = host;\n";

fn virtual_path(dir: &Path, name: &str) -> PathBuf {
    let encoded: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' { c.to_string() } else { format!("_{:x}_", c as u32) })
        .collect();
    dir.join(format!("{encoded}.mjs"))
}

fn virtual_name(dir: &Path, path: &Path) -> Option<String> {
    let stem = path.strip_prefix(dir).ok()?.to_str()?.strip_suffix(".mjs")?;
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

/// `{ url }` for a specifier imported (or required) by the file `parent`.
pub(crate) fn resolve(
    shared: &Shared,
    client: &NodeClient,
    specifier: &str,
    parent: Option<&str>,
    kind: &str,
) -> Result<Value, JsError> {
    if virtual_name(&client.virtual_dir, Path::new(specifier)).is_some() {
        return Ok(json!({ "url": file_url(Path::new(specifier))? }));
    }
    let kind = if kind == "require" { ResolveKind::Require } else { ResolveKind::Import };
    let inline =
        |key: &str| client.inline_sources.read().unwrap_or_else(|poison| poison.into_inner()).contains_key(key);
    let (found, key) = crate::modules::resolve_in(shared, &shared.config.cwd, &inline, specifier, parent, kind)?;
    let url = match found {
        "builtin" if key == "pi:host" => file_url(&virtual_path(&client.virtual_dir, &key))?,
        "builtin" => key,
        "virtual" => file_url(&virtual_path(&client.virtual_dir, &key))?,
        _ => file_url(Path::new(&key))?,
    };
    Ok(json!({ "url": url }))
}

/// `{ format, source }` for files Rust provides or transpiles; `null` lets Node load the file.
pub(crate) fn load(shared: &Shared, client: &NodeClient, path: &str) -> Result<Value, JsError> {
    let path = Path::new(path);
    if let Some(name) = virtual_name(&client.virtual_dir, path) {
        return Ok(json!({ "format": "module", "source": file_specifiers(&virtual_source(shared, &name)?)? }));
    }
    let key = path.display().to_string();
    let inline = client.inline_sources.read().unwrap_or_else(|poison| poison.into_inner()).get(&key).cloned();
    let source = match inline.or_else(|| crate::modules::registered_source(shared, &key)) {
        Some(source) => source.to_string(),
        None if crate::transpile::needs_transform(path) => {
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
            if globals.require {
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
            json!({ "format": "module", "source": format!("{prefix}{}", file_specifiers(&transpiled.code)?) })
        }
        ModuleFormat::CommonJs => json!({ "format": "commonjs", "source": transpiled.code }),
    })
}

fn virtual_source(shared: &Shared, name: &str) -> Result<String, JsError> {
    if name == "pi:host" {
        return Ok(PI_HOST_SOURCE.into());
    }
    let module = shared.virtual_modules.read().unwrap_or_else(|poison| poison.into_inner()).get(name).cloned();
    match module {
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
