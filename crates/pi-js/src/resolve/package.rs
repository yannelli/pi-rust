//! `package.json` reading and the Node `exports` / `imports` resolution algorithm.

use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::Value;

use crate::error::JsError;

/// The `package.json` fields that module resolution reads.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct PackageJson {
    #[serde(skip)]
    pub dir: PathBuf,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub main: Option<String>,
    #[serde(default)]
    pub module: Option<String>,
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
    #[serde(default)]
    pub exports: Option<Value>,
    #[serde(default)]
    pub imports: Option<Value>,
}

impl PackageJson {
    pub fn read(dir: &Path) -> Option<Self> {
        let text = std::fs::read_to_string(dir.join("package.json")).ok()?;
        let mut package: PackageJson = serde_json::from_str(&text).ok()?;
        package.dir = dir.to_path_buf();
        Some(package)
    }

    /// Resolve `subpath` (`.` or `./x`) through `exports`. `Ok(None)` means no `exports` field.
    pub fn resolve_export(&self, subpath: &str, conditions: &[&str]) -> Result<Option<String>, JsError> {
        let Some(exports) = &self.exports else { return Ok(None) };
        let map = match exports {
            Value::Object(object) if object.keys().any(|key| key.starts_with('.')) => exports.clone(),
            other => serde_json::json!({ ".": other }),
        };
        match match_map(&map, subpath, conditions, false)? {
            Some(target) => Ok(Some(target)),
            None => Err(not_exported(self, subpath)),
        }
    }

    /// Resolve a `#name` specifier through `imports`; a result may be a bare package name.
    pub fn resolve_import(&self, specifier: &str, conditions: &[&str]) -> Result<Option<String>, JsError> {
        let Some(imports) = &self.imports else { return Ok(None) };
        match_map(imports, specifier, conditions, true)
    }
}

fn not_exported(package: &PackageJson, subpath: &str) -> JsError {
    let file = package.dir.join("package.json");
    let message = if subpath == "." {
        format!("No \"exports\" main defined in {}", file.display())
    } else {
        format!("Package subpath '{subpath}' is not defined by \"exports\" in {}", file.display())
    };
    JsError::new(message).with_code("ERR_PACKAGE_PATH_NOT_EXPORTED")
}

/// Exact keys first, then the `*` pattern with the longest prefix, then legacy `dir/` keys.
fn match_map(map: &Value, key: &str, conditions: &[&str], allow_bare: bool) -> Result<Option<String>, JsError> {
    let Value::Object(object) = map else { return Ok(None) };
    if let Some(target) = object.get(key).filter(|_| !key.contains('*')) {
        return resolve_target(target, None, conditions, allow_bare);
    }
    let mut best: Option<(&str, &Value, String)> = None;
    for (pattern, target) in object {
        let Some((prefix, suffix)) = pattern.split_once('*') else { continue };
        if key.len() < pattern.len() - 1 || !key.starts_with(prefix) || !key.ends_with(suffix) {
            continue;
        }
        let matched = key[prefix.len()..key.len() - suffix.len()].to_string();
        if best.as_ref().is_none_or(|(current, _, _)| pattern_key_compare(pattern, current)) {
            best = Some((pattern, target, matched));
        }
    }
    if let Some((_, target, matched)) = best {
        return resolve_target(target, Some(&matched), conditions, allow_bare);
    }
    let folder = object
        .iter()
        .filter(|(pattern, _)| pattern.ends_with('/') && key.starts_with(pattern.as_str()))
        .max_by_key(|(pattern, _)| pattern.len());
    if let Some((pattern, Value::String(target))) = folder {
        return Ok(Some(format!("{target}{}", &key[pattern.len()..])));
    }
    Ok(None)
}

/// Node's PATTERN_KEY_COMPARE: whether `a` is a more specific pattern than `b`.
fn pattern_key_compare(a: &str, b: &str) -> bool {
    let a_base = a.find('*').map_or(a.len(), |index| index + 1);
    let b_base = b.find('*').map_or(b.len(), |index| index + 1);
    if a_base != b_base {
        return a_base > b_base;
    }
    a.len() > b.len()
}

fn resolve_target(
    target: &Value,
    matched: Option<&str>,
    conditions: &[&str],
    allow_bare: bool,
) -> Result<Option<String>, JsError> {
    match target {
        Value::String(text) => {
            if !text.starts_with("./") && !(allow_bare && !text.starts_with('/') && !text.starts_with("../")) {
                return Err(
                    JsError::new(format!("Invalid package target \"{text}\"")).with_code("ERR_INVALID_PACKAGE_TARGET")
                );
            }
            Ok(Some(match matched {
                Some(matched) => text.replace('*', matched),
                None => text.clone(),
            }))
        }
        Value::Array(items) => {
            let mut last_error = None;
            for item in items {
                match resolve_target(item, matched, conditions, allow_bare) {
                    Ok(Some(found)) => return Ok(Some(found)),
                    Ok(None) => {}
                    Err(error) => last_error = Some(error),
                }
            }
            last_error.map_or(Ok(None), Err)
        }
        Value::Object(object) => {
            for (condition, value) in object {
                if condition == "default" || conditions.contains(&condition.as_str()) {
                    if let Some(found) = resolve_target(value, matched, conditions, allow_bare)? {
                        return Ok(Some(found));
                    }
                }
            }
            Ok(None)
        }
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn package(exports: Value) -> PackageJson {
        PackageJson { dir: PathBuf::from("/pkg"), exports: Some(exports), ..PackageJson::default() }
    }

    const IMPORT: &[&str] = &["node", "import"];
    const REQUIRE: &[&str] = &["node", "require"];

    #[test]
    fn resolves_conditional_and_sugar_exports() {
        let sugar = package(json!("./main.js"));
        assert_eq!(sugar.resolve_export(".", IMPORT).unwrap().as_deref(), Some("./main.js"));
        let conditional = package(json!({ ".": { "import": "./esm/index.mjs", "require": "./cjs/index.cjs" } }));
        assert_eq!(conditional.resolve_export(".", IMPORT).unwrap().as_deref(), Some("./esm/index.mjs"));
        assert_eq!(conditional.resolve_export(".", REQUIRE).unwrap().as_deref(), Some("./cjs/index.cjs"));
        let nested = package(json!({ "import": { "types": "./x.d.ts", "default": "./x.js" } }));
        assert_eq!(nested.resolve_export(".", IMPORT).unwrap().as_deref(), Some("./x.js"));
    }

    #[test]
    fn resolves_subpath_patterns_by_specificity() {
        let exports = package(json!({
            "./*": "./dist/*.js",
            "./features/*": { "require": "./cjs/features/*.cjs", "default": "./features/*.js" },
            "./features/private/*": null,
            "./utils": ["./missing-fallback.js"]
        }));
        assert_eq!(exports.resolve_export("./a", IMPORT).unwrap().as_deref(), Some("./dist/a.js"));
        assert_eq!(
            exports.resolve_export("./features/x/y", REQUIRE).unwrap().as_deref(),
            Some("./cjs/features/x/y.cjs")
        );
        assert!(exports.resolve_export("./features/private/z", IMPORT).is_err());
        assert_eq!(exports.resolve_export("./utils", IMPORT).unwrap().as_deref(), Some("./missing-fallback.js"));
        let error = package(json!({ ".": "./i.js" })).resolve_export("./other", IMPORT).unwrap_err();
        assert_eq!(error.code.as_deref(), Some("ERR_PACKAGE_PATH_NOT_EXPORTED"));
    }

    #[test]
    fn resolves_imports_including_bare_targets() {
        let package = PackageJson {
            imports: Some(
                json!({ "#dep": { "node": "dep-node", "default": "./dep.js" }, "#internal/*": "./src/*.js" }),
            ),
            ..PackageJson::default()
        };
        assert_eq!(package.resolve_import("#dep", IMPORT).unwrap().as_deref(), Some("dep-node"));
        assert_eq!(package.resolve_import("#internal/a", IMPORT).unwrap().as_deref(), Some("./src/a.js"));
        assert_eq!(package.resolve_import("#nope", IMPORT).unwrap(), None);
    }
}
