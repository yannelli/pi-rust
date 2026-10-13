//! TypeScript, TSX, and JSX to JavaScript with oxc, plus module format detection.
//!
//! JavaScript files are parsed only to tell ES modules from CommonJS; their text is kept as is.

mod cache;
mod sourcemap;

use std::path::Path;

use oxc_allocator::Allocator;
use oxc_codegen::{Codegen, CodegenOptions};
use oxc_diagnostics::OxcDiagnostic;
use oxc_parser::{ParseOptions, Parser, ParserReturn};
use oxc_semantic::{Scoping, SemanticBuilder};
use oxc_span::SourceType;
use oxc_transformer::{DecoratorOptions, HelperLoaderMode, JsxRuntime, TransformOptions, Transformer};
use serde::{Deserialize, Serialize};

pub use cache::TranspileCache;
pub use sourcemap::{PositionMap, SourcePosition};
pub(crate) use sourcemap::{register_source_map, remap_stack};

use crate::error::JsError;

/// How a file is evaluated: as a native ES module or inside the CommonJS wrapper.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ModuleFormat {
    Esm,
    CommonJs,
}

/// CommonJS globals an ES module reads without declaring them; the loader defines these.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FreeGlobals {
    pub require: bool,
    pub dirname: bool,
    pub filename: bool,
}

/// The result of transpiling one file.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Transpiled {
    pub code: String,
    pub format: ModuleFormat,
    pub free_globals: FreeGlobals,
    /// Generated to original positions; `None` when `code` is the original text.
    pub map: Option<PositionMap>,
}

/// Options that change transpiled output; they are part of the cache key.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TranspileOptions {
    /// Factory for JSX elements in `.tsx` and `.jsx` files (classic runtime).
    pub jsx_pragma: String,
    pub jsx_fragment: String,
}

impl Default for TranspileOptions {
    fn default() -> Self {
        Self { jsx_pragma: "React.createElement".into(), jsx_fragment: "React.Fragment".into() }
    }
}

impl TranspileOptions {
    pub(crate) fn cache_key(&self) -> String {
        format!("{}|{}", self.jsx_pragma, self.jsx_fragment)
    }
}

/// Whether a path names a file the transpiler changes (TypeScript or JSX).
pub fn needs_transform(path: &Path) -> bool {
    matches!(extension(path).as_str(), "ts" | "mts" | "cts" | "tsx" | "jsx")
}

fn extension(path: &Path) -> String {
    path.extension().and_then(|ext| ext.to_str()).unwrap_or_default().to_ascii_lowercase()
}

fn source_type_for(path: &Path) -> SourceType {
    let base = match extension(path).as_str() {
        "ts" => SourceType::ts(),
        "mts" => SourceType::ts().with_module(true),
        "cts" => SourceType::ts().with_commonjs(true),
        "tsx" => SourceType::tsx(),
        "jsx" => SourceType::jsx(),
        "mjs" => SourceType::mjs(),
        "cjs" => SourceType::cjs(),
        _ => SourceType::unambiguous(),
    };
    if path.to_string_lossy().ends_with(".d.ts") { base.with_typescript_definition(true) } else { base }
}

fn parse<'a>(allocator: &'a Allocator, source: &'a str, source_type: SourceType) -> ParserReturn<'a> {
    let options = ParseOptions { allow_return_outside_function: !source_type.is_module(), ..ParseOptions::default() };
    Parser::new(allocator, source, source_type).with_options(options).parse()
}

/// Transpile one file. Syntax and transform errors carry `path:line:col` of the original text.
pub fn transpile(path: &Path, source: &str, options: &TranspileOptions) -> Result<Transpiled, JsError> {
    let allocator = Allocator::default();
    let source_type = source_type_for(path);
    let mut parsed = parse(&allocator, source, source_type);
    if !parsed.diagnostics.is_empty() && source_type.is_unambiguous() {
        // Top-level await without import or export only parses as a module.
        let retry = parse(&allocator, source, source_type.with_module(true));
        if retry.diagnostics.is_empty() {
            parsed = retry;
        }
    }
    if parsed.fatal_error || !parsed.diagnostics.is_empty() {
        return Err(diagnostics_error(path, source, &parsed.diagnostics, "SyntaxError"));
    }
    let format = if parsed.program.source_type.is_module() { ModuleFormat::Esm } else { ModuleFormat::CommonJs };
    let mut program = parsed.program;

    if !needs_transform(path) {
        let free_globals = if format == ModuleFormat::Esm && mentions_cjs_globals(source) {
            free_globals(&SemanticBuilder::new().build(&program).semantic.into_scoping())
        } else {
            FreeGlobals::default()
        };
        return Ok(Transpiled { code: source.to_string(), format, free_globals, map: None });
    }

    let semantic = SemanticBuilder::new().with_excess_capacity(2.0).with_enum_eval(true).build(&program);
    let scoping = semantic.semantic.into_scoping();
    let transform_options = transform_options(options);
    let transformed = Transformer::new(&allocator, path, &transform_options).build_with_scoping(scoping, &mut program);
    if !transformed.diagnostics.is_empty() {
        return Err(diagnostics_error(path, source, &transformed.diagnostics, "TransformError"));
    }
    let free_globals = free_globals(&transformed.scoping);
    let codegen_options = CodegenOptions { source_map_path: Some(path.to_path_buf()), ..CodegenOptions::default() };
    let generated = Codegen::new().with_options(codegen_options).build(&program);
    let map = generated.map.as_ref().map(PositionMap::from_source_map);
    Ok(Transpiled { code: generated.code, format, free_globals, map })
}

fn transform_options(options: &TranspileOptions) -> TransformOptions {
    let mut transform = TransformOptions {
        decorator: DecoratorOptions { legacy: true, emit_decorator_metadata: true, strict_null_checks: true },
        ..TransformOptions::default()
    };
    transform.helper_loader.mode = HelperLoaderMode::External;
    transform.jsx.runtime = JsxRuntime::Classic;
    transform.jsx.pragma = Some(options.jsx_pragma.clone());
    transform.jsx.pragma_frag = Some(options.jsx_fragment.clone());
    transform.typescript.jsx_pragma = options.jsx_pragma.clone().into();
    transform.typescript.jsx_pragma_frag = options.jsx_fragment.clone().into();
    transform
}

fn mentions_cjs_globals(source: &str) -> bool {
    source.contains("require") || source.contains("__dirname") || source.contains("__filename")
}

fn free_globals(scoping: &Scoping) -> FreeGlobals {
    let unresolved = scoping.root_unresolved_references();
    let has = |name: &str| unresolved.keys().any(|key| key.as_str() == name);
    FreeGlobals { require: has("require"), dirname: has("__dirname"), filename: has("__filename") }
}

fn diagnostics_error(path: &Path, source: &str, diagnostics: &[OxcDiagnostic], name: &str) -> JsError {
    let file = path.display().to_string();
    let mut lines = Vec::new();
    let mut first = None;
    for diagnostic in diagnostics.iter().take(5) {
        let offset = diagnostic.labels.first().map(|label| label.offset()).unwrap_or(0);
        let (line, column) = line_column(source, offset as usize);
        first.get_or_insert((diagnostic.message.to_string(), line, column));
        let mut text = format!("{file}:{line}:{column}: {}", diagnostic.message);
        if let Some(help) = &diagnostic.help {
            text.push_str(&format!(" ({help})"));
        }
        lines.push(text);
    }
    let (message, line, column) = first.unwrap_or_else(|| ("invalid source".into(), 1, 1));
    JsError::with_name(name, format!("{message} ({file}:{line}:{column})"))
        .with_stack(lines.iter().map(|line| format!("    at {line}")).collect::<Vec<_>>().join("\n"))
        .with_data("file", file)
        .with_data("line", line)
        .with_data("column", column)
}

/// 1-based line and column (in characters) of a byte offset.
fn line_column(source: &str, offset: usize) -> (usize, usize) {
    let offset = offset.min(source.len());
    let before = source.get(..offset).unwrap_or(source);
    let line = before.matches('\n').count() + 1;
    let line_start = before.rfind('\n').map(|index| index + 1).unwrap_or(0);
    (line, before[line_start..].chars().count() + 1)
}

#[cfg(test)]
mod tests;
