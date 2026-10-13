use std::path::Path;
use std::sync::Arc;

use super::*;

fn run(path: &str, source: &str) -> Transpiled {
    transpile(Path::new(path), source, &TranspileOptions::default()).unwrap()
}

#[test]
fn strips_types_and_keeps_runtime_constructs() {
    let source = r#"
import type { Foo } from "./types";
import { used, onlyType } from "./mod";
interface Shape { size: number }
type Alias = string | number;
enum Color { Red, Green = 5 }
namespace NS { export const x = 1; }
class Point { constructor(private x: number, public readonly y: number) {} }
export function area<T extends Shape>(shape: T, extra?: onlyType): number {
    return (shape.size as number) satisfies number;
}
export default used(Color.Green, NS.x, new Point(1, 2));
"#;
    let out = run("/ext/index.ts", source);
    assert_eq!(out.format, ModuleFormat::Esm);
    assert!(!out.code.contains("interface"));
    assert!(!out.code.contains("onlyType"), "type-only import is elided: {}", out.code);
    assert!(!out.code.contains("./types"));
    assert!(out.code.contains("Color[Color[\"Red\"] = 0] = \"Red\""), "{}", out.code);
    assert!(out.code.contains("this.x = x"));
    assert!(out.map.is_some());
}

#[test]
fn lowers_legacy_decorators_to_external_helpers() {
    let source = "function dec(t: any, k: string) {}\nexport class A { @dec method(): void {} }\n";
    let out = run("/ext/a.ts", source);
    assert!(out.code.contains("babelHelpers.decorate"), "{}", out.code);
}

#[test]
fn compiles_tsx_with_the_classic_runtime() {
    let out = run("/ext/view.tsx", "export const view = (n: number) => <Box size={n}><>hi</></Box>;\n");
    assert!(out.code.contains("React.createElement(Box"), "{}", out.code);
    assert!(out.code.contains("React.Fragment"));
    let options = TranspileOptions { jsx_pragma: "h".into(), jsx_fragment: "Fragment".into() };
    let custom = transpile(Path::new("/ext/v.jsx"), "export default <a/>;", &options).unwrap();
    assert!(custom.code.contains("h(\"a\""), "{}", custom.code);
}

#[test]
fn detects_module_formats() {
    assert_eq!(run("/a.js", "module.exports = 1;").format, ModuleFormat::CommonJs);
    assert_eq!(run("/a.js", "export const a = 1;").format, ModuleFormat::Esm);
    assert_eq!(run("/a.js", "const x = import.meta.url;").format, ModuleFormat::Esm);
    assert_eq!(run("/a.js", "await Promise.resolve(1);").format, ModuleFormat::Esm);
    assert_eq!(run("/a.cjs", "if (true) return; exports.a = 1;").format, ModuleFormat::CommonJs);
    assert_eq!(run("/a.mts", "const a: number = 1;").format, ModuleFormat::Esm);
    assert_eq!(run("/a.ts", "const fs = require('fs');").format, ModuleFormat::CommonJs);
    let js = run("/a.js", "exports.a = 1;");
    assert_eq!(js.code, "exports.a = 1;");
    assert!(js.map.is_none());
}

#[test]
fn reports_cjs_globals_used_by_es_modules() {
    let used = run("/a.ts", "export const here: string = __dirname + require('x');");
    assert_eq!(used.free_globals, FreeGlobals { require: true, dirname: true, filename: false });
    let declared = run("/a.js", "const __dirname = '.'; export default __dirname;");
    assert_eq!(declared.free_globals, FreeGlobals::default());
}

#[test]
fn syntax_errors_point_at_the_original_source() {
    let error =
        transpile(Path::new("/ext/bad.ts"), "const a = 1;\nconst b: = 2;\n", &TranspileOptions::default()).unwrap_err();
    assert_eq!(error.name, "SyntaxError");
    assert!(error.message.contains("/ext/bad.ts:2:"), "{}", error.message);
    assert_eq!(error.data.as_ref().unwrap()["line"], 2);
}

#[test]
fn source_maps_lead_back_to_typescript_lines() {
    let source =
        "interface A {\n  a: number;\n}\n\ntype B = A;\nexport function boom(): never {\n  throw new Error('x');\n}\n";
    let out = run("/ext/boom.ts", source);
    let line = out.code.lines().position(|line| line.contains("throw new Error")).unwrap() as u32 + 1;
    let column = out.code.lines().nth(line as usize - 1).unwrap().find("throw").unwrap() as u32 + 1;
    let original = out.map.unwrap().original(line, column).unwrap();
    assert_eq!(original.line, 7);
}

#[test]
fn cache_reuses_entries_by_content() {
    let dir = tempfile::tempdir().unwrap();
    let cache = TranspileCache::new(TranspileOptions::default(), Some(dir.path().to_path_buf()));
    let a = cache.get_or_transpile(Path::new("/a.ts"), "export const a: number = 1;").unwrap();
    let b = cache.get_or_transpile(Path::new("/b.ts"), "export const a: number = 1;").unwrap();
    assert!(Arc::ptr_eq(&a, &b));
    assert_eq!(cache.len(), 1);
    let fresh = TranspileCache::new(TranspileOptions::default(), Some(dir.path().to_path_buf()));
    let c = fresh.get_or_transpile(Path::new("/c.ts"), "export const a: number = 1;").unwrap();
    assert_eq!(*a, *c);
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
}
