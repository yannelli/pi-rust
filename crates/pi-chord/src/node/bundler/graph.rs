//! The module graph of one entry, assembled into a single CommonJS file.

use std::collections::{BTreeSet, HashMap};
use std::path::{Component, Path, PathBuf};

use pi_js::resolve::{ModuleResolver, ResolveKind, Resolved};
use pi_js::transpile::{PositionMap, TranspileOptions, transpile};

use super::rewrite::{ModuleFormat, Rewritten, Target, js_string, rewrite};
use super::sourcemap::{LineIndex, SourceMapBuilder};
use crate::error::{ChordError, ChordResult};

const RUNTIME: &str = r#"var __defProp = Object.defineProperty;
var __getOwnPropNames = Object.getOwnPropertyNames;
var __hasOwnProp = Object.prototype.hasOwnProperty;
var __external = require;
var __markESM = (target) => __defProp(target, "__esModule", { value: true });
var __export = (target, all) => {
  for (let name in all) __defProp(target, name, { get: all[name], enumerable: true });
};
var __reExport = (target, mod) => {
  for (let key of __getOwnPropNames(mod))
    if (key !== "default" && !__hasOwnProp.call(target, key)) __defProp(target, key, { get: () => mod[key], enumerable: true });
};
var __toESM = (mod) => {
  if (mod != null && mod.__esModule) return mod;
  var target = {};
  if (mod != null && (typeof mod === "object" || typeof mod === "function"))
    for (let key of __getOwnPropNames(mod)) if (key !== "default") __defProp(target, key, { get: () => mod[key], enumerable: true });
  __defProp(target, "default", { value: mod, enumerable: true });
  return target;
};
var __cache = [];
var __load = (id) => {
  var cached = __cache[id];
  if (cached !== undefined) return cached.exports;
  var module = (__cache[id] = { exports: {} });
  var table = __tables[id];
  var localRequire = (specifier) => (__hasOwnProp.call(table, specifier) ? __load(table[specifier]) : __external(specifier));
  localRequire.resolve = (specifier) => __external.resolve(specifier);
  __modules[id](module.exports, module, localRequire);
  return module.exports;
};
"#;

/// Which platform's built-in modules stay external.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FacetBundlePlatform {
    #[default]
    Node,
    Browser,
    Neutral,
}

pub(crate) struct GraphOptions<'a> {
    pub working_directory: &'a Path,
    pub external: &'a [String],
    pub define: &'a [(String, String)],
    pub platform: FacetBundlePlatform,
}

struct Module {
    path: PathBuf,
    original: String,
    code: String,
    map: Option<PositionMap>,
    rewritten: Rewritten,
}

pub(crate) struct BundleOutput {
    pub code: String,
    pub external_imports: Vec<String>,
    modules: Vec<Module>,
    bases: Vec<usize>,
}

fn matches_external(pattern: &str, specifier: &str) -> bool {
    match pattern.split_once('*') {
        Some((prefix, suffix)) => {
            specifier.len() >= prefix.len() + suffix.len()
                && specifier.starts_with(prefix)
                && specifier.ends_with(suffix)
        }
        None => pattern == specifier,
    }
}

/// `to` relative to the directory `from`, with `/` separators as source maps use.
pub(crate) fn relative_path(from: &Path, to: &Path) -> String {
    let from: Vec<Component> = from.components().collect();
    let to: Vec<Component> = to.components().collect();
    let shared = from.iter().zip(&to).take_while(|(a, b)| a == b).count();
    let mut parts: Vec<String> = vec!["..".into(); from.len() - shared];
    parts.extend(to[shared..].iter().map(|part| part.as_os_str().to_string_lossy().into_owned()));
    parts.join("/")
}

fn location(path: &Path, code: &str, map: Option<&PositionMap>, offset: u32) -> String {
    let (line, column) = LineIndex::new(code).locate(offset as usize);
    let original = map.and_then(|map| map.original(line + 1, column + 1));
    let (line, column) = original.map_or((line + 1, column + 1), |position| (position.line, position.column));
    format!("{}:{line}:{column}", path.display())
}

fn load_module(path: &Path) -> ChordResult<(String, String, ModuleFormat, Option<PositionMap>)> {
    let original = std::fs::read_to_string(path).map_err(ChordError::other)?;
    if path.extension().is_some_and(|extension| extension == "json") {
        let code = format!("module.exports = {};\n", original.trim());
        return Ok((original, code, ModuleFormat::CommonJs, None));
    }
    let transpiled = transpile(path, &original, &TranspileOptions::default())
        .map_err(|error| ChordError::error(error.to_string()))?;
    let format = match transpiled.format {
        pi_js::ModuleFormat::Esm => ModuleFormat::Esm,
        pi_js::ModuleFormat::CommonJs => ModuleFormat::CommonJs,
    };
    Ok((original, transpiled.code, format, transpiled.map))
}

/// Bundle `entry` and everything it imports, except externals and platform built-ins.
pub(crate) fn bundle_entry(entry: &Path, options: &GraphOptions<'_>) -> ChordResult<BundleOutput> {
    let resolver = ModuleResolver::new();
    let entry = pi_js::resolve::canonical(entry);
    let mut ids: HashMap<PathBuf, usize> = HashMap::from([(entry.clone(), 0)]);
    let mut queue = vec![entry];
    let mut externals = BTreeSet::new();
    let mut modules = Vec::new();
    while modules.len() < queue.len() {
        let path = queue[modules.len()].clone();
        let (original, code, format, map) = load_module(&path)?;
        let mut resolve = |specifier: &str, is_require: bool, offset: u32| -> ChordResult<Target> {
            if options.external.iter().any(|pattern| matches_external(pattern, specifier)) {
                externals.insert(specifier.to_string());
                return Ok(Target::External(specifier.to_string()));
            }
            let kind = if is_require { ResolveKind::Require } else { ResolveKind::Import };
            let none = |_: &str| None;
            match resolver.resolve(specifier, Some(&path), options.working_directory, kind, &none) {
                Ok(Resolved::Builtin(_)) if options.platform == FacetBundlePlatform::Node => {
                    externals.insert(specifier.to_string());
                    Ok(Target::External(specifier.to_string()))
                }
                Ok(Resolved::File(found)) => {
                    let next = ids.len();
                    let id = *ids.entry(found.clone()).or_insert(next);
                    if id == next {
                        queue.push(found);
                    }
                    Ok(Target::Internal(id))
                }
                _ => {
                    let at = location(&path, &code, map.as_ref(), offset);
                    Err(ChordError::error(format!("{at}: Could not resolve {}", js_string(specifier))))
                }
            }
        };
        let display = path.display().to_string();
        let rewritten = if path.extension().is_some_and(|extension| extension == "json") {
            let pieces = vec![(0, 0, code.len())];
            Rewritten { code: code.clone(), pieces, require_table: Vec::new() }
        } else {
            rewrite(&display, &code, format, options.define, &mut resolve)?
        };
        modules.push(Module { path, original, code, map, rewritten });
    }
    let mut code = String::from("\"use strict\";\n");
    code.push_str(RUNTIME);
    code.push_str("var __modules = [\n");
    let mut bases = Vec::new();
    for module in &modules {
        let name = relative_path(options.working_directory, &module.path);
        code.push_str(&format!("// {name}\nfunction (exports, module, require) {{\n"));
        bases.push(code.len());
        code.push_str(&module.rewritten.code);
        code.push_str("\n},\n");
    }
    code.push_str("];\nvar __tables = [\n");
    for module in &modules {
        let entries: Vec<String> = module
            .rewritten
            .require_table
            .iter()
            .map(|(specifier, id)| format!("{}: {id}", js_string(specifier)))
            .collect();
        code.push_str(&format!("  {{ {} }},\n", entries.join(", ")));
    }
    code.push_str("];\nmodule.exports = __load(0);\n");
    Ok(BundleOutput { code, external_imports: externals.into_iter().collect(), modules, bases })
}

impl BundleOutput {
    /// A source map for `code`, with sources relative to `map_directory`.
    pub fn source_map(&self, map_directory: &Path) -> String {
        let mut builder = SourceMapBuilder::default();
        let bundle = LineIndex::new(&self.code);
        for (module, base) in self.modules.iter().zip(&self.bases) {
            let source = builder.add_source(relative_path(map_directory, &module.path), module.original.clone());
            let transpiled = LineIndex::new(&module.code);
            for (output, start, length) in module.rewritten.pieces.iter().copied() {
                let text = &module.rewritten.code[output..output + length];
                let breaks = text.match_indices('\n').map(|(index, _)| index + 1).filter(|index| *index < length);
                for delta in std::iter::once(0).chain(breaks) {
                    let generated = bundle.locate(base + output + delta);
                    let (line, column) = transpiled.locate(start + delta);
                    let original = match &module.map {
                        Some(map) => map.original(line + 1, column + 1).map(|at| (at.line - 1, at.column - 1)),
                        None => Some((line, column)),
                    };
                    if let Some(original) = original {
                        builder.add(generated, source, original);
                    }
                }
            }
        }
        builder.finish()
    }
}
