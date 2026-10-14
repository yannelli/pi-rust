//! The WebAssembly global and node:wasi. The scripts are in fixtures/wasm, with a small binary
//! builder (there are no text format tools); the expected output there comes from Node 22.

mod common;

use common::*;

const BUILDER: &str = include_str!("fixtures/wasm/builder.js");

/// Run fixture `name` in a fresh directory and return its stdout and uncaught errors.
async fn run_fixture(name: &str, source: &str) -> (String, usize) {
    let (_dir, root) = tempdir();
    write(&root, "builder.js", BUILDER);
    write(&root, name, source);
    let (runtime, capture) = runtime(&root);
    runtime.import(root.join(name).display().to_string()).await.unwrap();
    runtime.idle().await.unwrap();
    let uncaught = capture.uncaught.lock().unwrap().len();
    (capture.stdout(), uncaught)
}

#[tokio::test]
async fn webassembly_matches_node() {
    let (stdout, uncaught) =
        run_fixture("webassembly_case.js", include_str!("fixtures/wasm/webassembly_case.js")).await;
    assert_eq!(stdout, include_str!("fixtures/wasm/webassembly.txt"));
    assert_eq!(uncaught, 0);
}

#[tokio::test]
async fn wasi_matches_node() {
    let (stdout, uncaught) = run_fixture("wasi_case.js", include_str!("fixtures/wasm/wasi_case.js")).await;
    assert_eq!(stdout, include_str!("fixtures/wasm/wasi.txt"));
    assert_eq!(uncaught, 0);
}

#[tokio::test]
async fn wasi_and_webassembly_load_everywhere() {
    let (_dir, root) = tempdir();
    write(&root, "builder.js", BUILDER);
    let (runtime, capture) = runtime(&root);
    let actual = run_cjs(&runtime, &root, r#"
const { ModuleBuilder, op } = require("./builder");
const out = {};
out.require = [typeof require("wasi").WASI, require("node:wasi") === require("wasi")];
out.esm = typeof (await import("node:wasi")).WASI;
out.builtin = require("module").builtinModules.includes("wasi");
out.global = [typeof WebAssembly, Object.prototype.toString.call(WebAssembly), "WebAssembly" in globalThis];
const m = new ModuleBuilder();
m.memory(1);
m.export("memory", "memory", 0);
m.func(["i32"], ["i32"], [op.local_get(0), op.memory_grow], { export: "grow" });
const { instance } = await WebAssembly.instantiate(m.build());
const { memory, grow } = instance.exports;
const sizes = [];
for (let i = 0; i < 20; i++) {
  const view = new Uint8Array(memory.buffer);
  view[view.length - 1] = i;
  grow(1);
  sizes.push(view.length === 0 && memory.buffer.byteLength === (i + 2) * 65536 && new Uint8Array(memory.buffer)[(i + 1) * 65536 - 1] === i);
}
out.grows = sizes.every(Boolean);
return out;
"#).await;
    let expected = serde_json::json!({
        "require": ["function", true],
        "esm": "function",
        "builtin": true,
        "global": ["object", "[object WebAssembly]", true],
        "grows": true
    });
    assert_eq!(actual, expected);
    assert!(capture.stderr().contains("ExperimentalWarning: WASI is an experimental feature"));
}
