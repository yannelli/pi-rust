//! The export scanner, the module tree walk, and the `modules.rs` parser.

use std::collections::BTreeSet;

use super::exports::{Source, scan};
use super::module_exports::{module_source, upstream_modules};
use super::module_tree::{Tree, Walker};

fn names(src: &str) -> Vec<String> {
    scan(src).entries.into_iter().map(|entry| entry.name).collect()
}

fn tree(files: &[(&str, &str)]) -> Tree {
    let files = files.iter().map(|(key, text)| (key.to_string(), text.to_string())).collect();
    Tree { files, ..Tree::default() }
}

fn keys(tree: &Tree, key: &str) -> BTreeSet<String> {
    Walker::new(tree).exports(key).into_keys().collect()
}

#[test]
fn type_only_exports_are_left_out() {
    let src = r#"
export type Alias = string;
export interface Shape { a: string }
export declare const ambient: number;
export type { A, B } from "./types.ts";
export { type C, value } from "./mixed.ts";
type Local = number;
interface LocalShape {}
import type { Imported } from "./types.ts";
import { type Inline, real } from "./real.ts";
export { Local, LocalShape, Imported, Inline, real };
export const kept = 1;
"#;
    assert_eq!(names(src), ["value", "kept", "real"]);
}

#[test]
fn declarations_and_aliases() {
    let src = r#"
export async function* gen() {}
export abstract class Base {}
export const enum Flags { A }
export const { first, second: renamed, ...rest } = object;
export default function () {}
function inner() { const re = /^ok(lch|hsl)\(/i; return `'${re.source.replace(/'/g, `'\\''`)}'`; }
export { inner as outer, inner as "quoted" };
export * as ns from "./ns.ts";
declare module "x" { export const hidden: number; }
"#;
    assert_eq!(names(src), ["gen", "Base", "Flags", "first", "renamed", "rest", "default", "ns", "outer", "quoted"]);
    let scanned = scan("export { a as b } from \"./a.ts\";\nexport * from \"./star.ts\";");
    assert_eq!(scanned.entries[0].source, Source::From { spec: "./a.ts".into(), imported: "a".into() });
    assert_eq!(scanned.stars, ["./star.ts"]);
}

#[test]
fn unsupported_statements_are_flagged() {
    let scanned = scan(
        "export function a() { throw err(\"ERR_PI_UNSUPPORTED\"); }\nexport function b() {}\nexport const c = unsupportedClass(\"C\");",
    );
    let flags: Vec<bool> = scanned.entries.iter().map(|entry| entry.unsupported).collect();
    assert_eq!(flags, [true, false, true]);
}

#[test]
fn wildcards_follow_relative_and_bare_paths_through_cycles() {
    let mut files = tree(&[
        ("pkg/index.ts", "export * from \"./a.js\";\nexport { renamed as alias, type T, gone } from \"./b.ts\";"),
        (
            "pkg/a.ts",
            "export * from \"./index.ts\";\nexport * from \"dep/sub\";\nexport default 1;\nexport const a = 1;",
        ),
        ("pkg/b.ts", "export const renamed = 1;\nexport type T = 1;\nexport interface gone {}"),
        ("dep/src/sub/index.ts", "export function deep() {}"),
        (
            "host/x.js",
            "export { a } from \"../pkg/a.ts\";\nexport * from \"/pkg/b.ts\";\nexport { m } from \"missing\";",
        ),
    ]);
    files.bare.push(("dep/*".into(), "dep/src/*".into()));
    assert_eq!(keys(&files, "pkg/index.ts"), ["a", "alias", "deep"].map(String::from).into());
    let mut walker = Walker::new(&files);
    let host: BTreeSet<String> = walker.exports("host/x.js").into_keys().collect();
    assert_eq!(host, ["a", "m", "renamed"].map(String::from).into());
    assert!(walker.unresolved.iter().any(|spec| spec.starts_with("missing")));
    assert_eq!(files.resolve("pkg/index.ts", "dep/nope"), None);
}

#[test]
fn modules_rs_entries_become_sources() {
    let star = module_source(r#""@x/tui", reexport("pi-tui/src/index.ts")"#).unwrap();
    assert_eq!(star, "export * from \"/pi-tui/src/index.ts\";\n");
    let json = module_source(r#""@x/core", VirtualModule::Json(serde_json::json!({ "a": 1, "b": {} }))"#).unwrap();
    assert_eq!(names(&json), ["a", "b"]);
    let format = r#""@x/all", VirtualModule::Source(format!("export {{ one, two as three }} from \"{}\";\n", path("host/ai.js")))"#;
    let source = module_source(format).unwrap();
    assert_eq!(source, "export { one, two as three } from \"/host/ai.js\";\n");
    assert!(module_source(r#""@x/native", VirtualModule::Native(build)"#).is_none());
}

#[test]
fn upstream_table_groups_module_aliases() {
    let src = r#"
import * as tui from "@earendil-works/pi-tui";
import * as agent from "../../index.ts";
export const VIRTUAL_MODULES: Record<string, unknown> = {
	"@earendil-works/pi-tui": tui,
	// a comment: tui,
	"@earendil-works/pi-coding-agent": agent,
	"@mariozechner/pi-tui": tui,
};"#;
    let groups = upstream_modules(src);
    assert_eq!(
        groups[0],
        ("@earendil-works/pi-tui".into(), vec!["@earendil-works/pi-tui".into(), "@mariozechner/pi-tui".into()])
    );
    assert_eq!(groups[1].0, "../../index.ts");
}
