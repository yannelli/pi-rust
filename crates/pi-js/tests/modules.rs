mod common;

use common::*;
use pi_js::{HostValue, VirtualModule};
use serde_json::json;

#[tokio::test]
async fn runs_typescript_modules_with_imports() {
    let (_dir, root) = tempdir();
    write(
        &root,
        "ext/util.ts",
        "export enum Mode { A = 'a', B = 'b' }\nexport function double(n: number): number { return n * 2; }\n",
    );
    write(
        &root,
        "ext/index.ts",
        "import { double, Mode } from './util.js';\nimport type { Foo } from './types';\ninterface Shape { size: number }\nexport default function (shape: Shape): string { return `${double(shape.size)}:${Mode.B}`; }\n",
    );
    let (runtime, _capture) = runtime(&root);
    let factory = runtime.import_default(root.join("ext/index.ts").display().to_string()).await.unwrap();
    let result = factory.call(vec![HostValue::Json(json!({ "size": 21 }))]).await.unwrap();
    assert_eq!(result.into_json(), json!("42:b"));
}

#[tokio::test]
async fn interoperates_between_commonjs_and_es_modules() {
    let (_dir, root) = tempdir();
    write(&root, "cjs.js", "exports.name = 'cjs'; exports.add = (a, b) => a + b; module.exports.self = __filename;");
    write(
        &root,
        "babel.cjs",
        "Object.defineProperty(exports, '__esModule', { value: true }); exports.default = 'dflt'; exports.extra = 1;",
    );
    write(&root, "replaced.js", "module.exports = function main() { return 'called'; }; module.exports.flag = true;");
    write(&root, "esm.mjs", "export default 'esm-default'; export const named = 'esm-named';");
    write(&root, "data.json", "{\"answer\": 42, \"list\": [1, 2]}");
    write(
        &root,
        "main.ts",
        r#"
import cjs, { name, add } from "./cjs.js";
import babel, { extra } from "./babel.cjs";
import replaced from "./replaced";
import data from "./data.json";
const required = require("./esm.mjs");
const viaRequire = require("./cjs");
const json = require("./data.json");
export const result = {
  name, sum: add(2, 3), sameObject: cjs === viaRequire, babel, extra,
  replaced: replaced(), flag: replaced.flag, answer: data.answer, jsonSame: json.answer,
  esmDefault: required.default, esmNamed: required.named, esModule: required.__esModule,
  dirname: __dirname.length > 0, file: import.meta.filename.endsWith("main.ts"),
};
"#,
    );
    let (runtime, _capture) = runtime(&root);
    let namespace = runtime.import("./main.ts").await.unwrap();
    let result = namespace.get("result").await.unwrap().into_json();
    assert_eq!(
        result,
        json!({
            "name": "cjs", "sum": 5, "sameObject": true, "babel": "dflt", "extra": 1,
            "replaced": "called", "flag": true, "answer": 42, "jsonSame": 42,
            "esmDefault": "esm-default", "esmNamed": "esm-named", "esModule": true,
            "dirname": true, "file": true
        })
    );
}

#[tokio::test]
async fn resolves_node_modules_with_exports_maps() {
    let (_dir, root) = tempdir();
    write(
        &root,
        "node_modules/dual/package.json",
        r#"{"name":"dual","exports":{".":{"import":"./esm.mjs","require":"./cjs.cjs"},"./feature":"./feature.js"}}"#,
    );
    write(&root, "node_modules/dual/esm.mjs", "export const kind = 'esm';");
    write(&root, "node_modules/dual/cjs.cjs", "exports.kind = 'cjs';");
    write(&root, "node_modules/dual/feature.js", "module.exports = { feature: true };");
    write(&root, "node_modules/ms/package.json", r#"{"name":"ms","main":"./index"}"#);
    write(&root, "node_modules/ms/index.js", "module.exports = (text) => parseInt(text, 10) * 1000;");
    write(
        &root,
        "src/deep/entry.ts",
        "import { kind } from 'dual';\nimport { feature } from 'dual/feature';\nimport ms from 'ms';\nconst viaRequire = require('dual');\nexport default { kind, feature, cjs: viaRequire.kind, ms: ms('2s') };\n",
    );
    let (runtime, _capture) = runtime(&root);
    let value = runtime.import_default("./src/deep/entry.ts").await.unwrap();
    assert_eq!(value.to_json().await.unwrap(), json!({ "kind": "esm", "feature": true, "cjs": "cjs", "ms": 2000 }));
    let error = runtime.import("./src/missing.ts").await.unwrap_err();
    assert_eq!(error.code.as_deref(), Some("ERR_MODULE_NOT_FOUND"));
    let hidden = runtime.import("dual/esm.mjs").await.unwrap_err();
    assert_eq!(hidden.code.as_deref(), Some("ERR_PACKAGE_PATH_NOT_EXPORTED"));
}

#[tokio::test]
async fn supports_dynamic_import_and_top_level_await() {
    let (_dir, root) = tempdir();
    write(&root, "lazy.ts", "export const value: number = 7;");
    write(&root, "main.js", "const mod = await import('./lazy.ts');\nexport const value = mod.value * 6;\n");
    write(&root, "script.cjs", "module.exports = () => import('./lazy.ts').then((m) => m.value);");
    let (runtime, _capture) = runtime(&root);
    let namespace = runtime.import("./main.js").await.unwrap();
    assert_eq!(namespace.get("value").await.unwrap().into_json(), json!(42));
    let lazy = runtime.import_default("./script.cjs").await.unwrap();
    assert_eq!(lazy.call(vec![]).await.unwrap().into_json(), json!(7));
}

#[tokio::test]
async fn loads_virtual_modules_and_aliases() {
    let (_dir, root) = tempdir();
    let (runtime, _capture) = runtime(&root);
    runtime.register_virtual_module(
        "@earendil-works/pi-tui",
        VirtualModule::Source("export class Text { constructor(t) { this.t = t; } }\nexport default 'tui';".into()),
    );
    runtime.register_virtual_module("typebox", VirtualModule::Json(json!({ "Kind": "kind-symbol" })));
    runtime.register_virtual_module(
        "@earendil-works/pi-coding-agent",
        VirtualModule::native(|scope| {
            let object = pi_js::rquickjs::Object::new(scope.ctx().clone()).map_err(|error| scope.caught(error))?;
            object.set("VERSION", "1.2.3").map_err(|error| scope.caught(error))?;
            Ok(object)
        }),
    );
    write(
        &root,
        "ext.ts",
        r#"
import { Text } from "@mariozechner/pi-tui";
import tui from "@earendil-works/pi-tui";
import { Kind } from "@sinclair/typebox";
import { VERSION } from "@mariozechner/pi-coding-agent";
const again = require("@earendil-works/pi-coding-agent");
const tuiRequired = require("@earendil-works/pi-tui");
export default { text: new Text("x").t, tui, kind: Kind, version: VERSION, same: again.VERSION, required: tuiRequired.default };
"#,
    );
    let value = runtime.import_default("./ext.ts").await.unwrap();
    assert_eq!(
        value.to_json().await.unwrap(),
        json!({ "text": "x", "tui": "tui", "kind": "kind-symbol", "version": "1.2.3", "same": "1.2.3", "required": "tui" })
    );
}

#[tokio::test]
async fn errors_carry_typescript_positions() {
    let (_dir, root) = tempdir();
    write(
        &root,
        "boom.ts",
        "interface A {\n  a: number;\n}\ntype B = A;\n\nexport function boom(): never {\n  throw new TypeError('bad thing');\n}\n",
    );
    write(&root, "bad.ts", "const x: = 1;\n");
    let (runtime, _capture) = runtime(&root);
    let boom = runtime.import("./boom.ts").await.unwrap();
    let error = boom.call_method("boom", vec![]).await.unwrap_err();
    assert_eq!(error.name, "TypeError");
    assert_eq!(error.message, "bad thing");
    let stack = error.stack.unwrap();
    assert!(stack.contains("boom.ts:7:"), "{stack}");
    let syntax = runtime.import("./bad.ts").await.unwrap_err();
    assert_eq!(syntax.name, "SyntaxError");
    assert!(syntax.message.contains("bad.ts:1:"), "{}", syntax.message);
}

#[tokio::test]
async fn evaluates_inline_modules() {
    let (_dir, root) = tempdir();
    write(&root, "helper.ts", "export const twice = (n: number) => n * 2;");
    let (runtime, _capture) = runtime(&root);
    let module = runtime
        .eval_module("inline.ts", "import { twice } from './helper.ts';\nexport const out: number = twice(21);")
        .await
        .unwrap();
    assert_eq!(module.get("out").await.unwrap().into_json(), json!(42));
}

#[tokio::test]
async fn registered_source_files_load_with_relative_imports() {
    let (_dir, root) = tempdir();
    let (runtime, _capture) = runtime(&root);
    let base = std::env::temp_dir().join("pi-source-files-test").join("pkg");
    runtime.register_source_files([
        (base.join("index.mjs"), "export * from './lib/math.mjs';\nexport default 'pkg';\n".to_string()),
        (base.join("lib/math.mjs"), "export const triple = (n) => n * 3;\n".to_string()),
    ]);
    let entry = serde_json::to_string(&base.join("index.mjs").display().to_string()).unwrap();
    runtime.register_virtual_module(
        "virtual-pkg",
        VirtualModule::Source(format!("export * from {entry};\nexport {{ default }} from {entry};\n")),
    );
    write(&root, "use.ts", "import name, { triple } from 'virtual-pkg';\nexport const out = `${name}:${triple(4)}`;\n");
    let namespace = runtime.import("./use.ts").await.unwrap();
    assert_eq!(namespace.get("out").await.unwrap().into_json(), json!("pkg:12"));
    let required = run_cjs(&runtime, &root, &format!("return require({entry}).triple(2);")).await;
    assert_eq!(required, json!(6));
}
