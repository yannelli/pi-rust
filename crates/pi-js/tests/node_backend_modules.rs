//! The Node backend's module hooks: pi's resolver, virtual modules, registered sources, and
//! TypeScript through pi-js's transpiler, plus Node APIs QuickJS lacks.

mod common;
mod node_common;

use common::*;
use node_common::*;
use pi_js::VirtualModule;
use serde_json::json;

#[tokio::test]
async fn imports_esm_cjs_and_typescript_files() {
    let (_dir, root) = tempdir();
    write(&root, "lib/math.ts", "export const twice = (n: number): number => n * 2;\ninterface Unused { a: string }\n");
    write(&root, "lib/legacy.cjs", "module.exports = { name: 'legacy', dir: __dirname.endsWith('lib') };\n");
    write(&root, "lib/data.json", "{ \"answer\": 42 }\n");
    write(
        &root,
        "main.ts",
        r#"import { twice } from "./lib/math";
import legacy from "./lib/legacy.cjs";
import data from "./lib/data.json" with { type: "json" };
import { createRequire } from "node:module";
const viaRequire = require("./lib/legacy.cjs");
enum Color { Red = 1 }
export default { twice: twice(Color.Red), legacy, answer: data.answer, same: viaRequire === legacy, kind: typeof createRequire };
"#,
    );
    write(&root, "tsx/view.tsx", "const React = { createElement: (t: string) => t };\nexport default <div />;\n");
    let Some((runtime, capture)) = node_runtime(&root) else { return };
    let main = runtime.import_default("./main.ts").await.unwrap();
    assert_eq!(
        main.to_json().await.unwrap(),
        json!({ "twice": 2, "legacy": { "name": "legacy", "dir": true }, "answer": 42, "same": true, "kind": "function" })
    );
    let view = runtime.import_default(root.join("tsx/view.tsx").display().to_string()).await.unwrap();
    assert_eq!(view.to_json().await.unwrap(), json!("div"));
    assert_eq!(capture.stderr(), "");
}

#[tokio::test]
async fn virtual_modules_and_registered_sources() {
    let (_dir, root) = tempdir();
    let Some((runtime, _capture)) = node_runtime(&root) else { return };
    let vendor = root.join("vendor-not-on-disk");
    runtime.register_source_files([
        (
            vendor.join("lib/index.ts"),
            "export { greet } from \"./greet\";\nexport const kind: string = 'vendor';\n".into(),
        ),
        (vendor.join("lib/greet.ts"), "export const greet = (name: string) => `hi ${name}`;\n".into()),
    ]);
    runtime.register_virtual_module(
        "@earendil-works/pi-coding-agent",
        VirtualModule::Source(format!("export * from {};\n", json!(vendor.join("lib/index.ts")))),
    );
    runtime
        .register_virtual_module("@earendil-works/pi-agent-core", VirtualModule::Json(json!({ "a": 1, "delete": 2 })));
    write(
        &root,
        "ext.mjs",
        r#"import { greet, kind } from "@mariozechner/pi-coding-agent";
import * as direct from "@earendil-works/pi-coding-agent";
import core, { a } from "@earendil-works/pi-agent-core";
export default { greeting: greet("pi"), kind, same: direct.greet === greet, core, a };
"#,
    );
    write(
        &root,
        "ext.cjs",
        "const agent = require('@earendil-works/pi-coding-agent');\nmodule.exports = agent.greet('cjs');\n",
    );
    let ext = runtime.import_default("./ext.mjs").await.unwrap();
    assert_eq!(
        ext.to_json().await.unwrap(),
        json!({ "greeting": "hi pi", "kind": "vendor", "same": true, "core": { "a": 1, "delete": 2 }, "a": 1 })
    );
    if cfg!(windows) {
        return; // docs/parity/GAPS.md: require(esm) of a registered source fails on Windows.
    }
    let version = runtime.eval("process.versions.node").await.unwrap().into_json();
    let version = pi_js::node_backend::NodeVersion::parse(version.as_str().unwrap()).unwrap();
    if version.has_register_hooks() {
        let cjs = runtime.import_default("./ext.cjs").await.unwrap();
        assert_eq!(cjs.to_json().await.unwrap(), json!("hi cjs"));
    } else {
        let error = runtime.import_default("./ext.cjs").await.unwrap_err();
        assert!(error.message.contains("22.15"), "{error:?}");
    }
}

#[tokio::test]
async fn native_virtual_modules_fail_clearly() {
    let (_dir, root) = tempdir();
    let Some((runtime, _capture)) = node_runtime(&root) else { return };
    runtime.register_virtual_module(
        "native-thing",
        VirtualModule::native(|scope| {
            pi_js::rquickjs::Object::new(scope.ctx().clone()).map_err(|error| scope.caught(error))
        }),
    );
    let error = runtime.import("native-thing").await.unwrap_err();
    assert!(error.message.contains("Node backend"), "{error:?}");
}

#[tokio::test]
async fn child_process_fork_runs_on_real_node() {
    let (_dir, root) = tempdir();
    write(
        &root,
        "child.cjs",
        "process.on('message', (m) => { process.send({ echo: m, exec: process.execPath }); process.exit(0); });\n",
    );
    write(
        &root,
        "parent.cjs",
        r#"const { fork } = require("node:child_process");
module.exports = new Promise((resolve, reject) => {
  const child = fork(require("node:path").join(__dirname, "child.cjs"));
  child.on("message", (message) => resolve({ echo: message.echo, node: message.exec === process.execPath }));
  child.on("error", reject);
  child.send("ping");
});
"#,
    );
    let Some((runtime, _capture)) = node_runtime(&root) else { return };
    let module = runtime.import("./parent.cjs").await.unwrap();
    let outcome = module.get("default").await.unwrap().into_json();
    assert_eq!(outcome, json!({ "echo": "ping", "node": true }));
}

#[test]
fn require_rewrite_skips_strings_comments_and_regexes() {
    let Some(node) = node() else { return };
    let module = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/node_backend/js/require_rewrite.mjs");
    let cases = json!([
        ["const a = require(\"x\");", "const a = require.call(void 0, \"x\");"],
        [
            "s = \"require(\" + 'require(' + `require(${require(\"y\")})`;",
            "s = \"require(\" + 'require(' + `require(${require.call(void 0, \"y\")})`;"
        ],
        ["// require(a)\n/* require(b) */ x", "// require(a)\n/* require(b) */ x"],
        ["s.replace(/'/g, \"\\\\'\"); require ( 'z')", "s.replace(/'/g, \"\\\\'\"); require.call(void 0,  'z')"],
        ["a / b / require('w')", "a / b / require.call(void 0, 'w')"],
        [
            "obj.require('q'); o?.require(1); function require(n) {}",
            "obj.require('q'); o?.require(1); function require(n) {}"
        ],
        [
            "f(...require('s'), { k: require('t') })",
            "f(...require.call(void 0, 's'), { k: require.call(void 0, 't') })"
        ],
        ["#!/usr/bin/env node\nrequire('h')", "#!/usr/bin/env node\nrequire.call(void 0, 'h')"],
    ]);
    let script = format!(
        "import {{ hideRequireCalls }} from {url};\nconst cases = {cases};\nprocess.stdout.write(JSON.stringify(cases.map(([input]) => hideRequireCalls(input))));",
        url = json!(url::Url::from_file_path(&module).unwrap().to_string()),
    );
    let output = std::process::Command::new(node).args(["--input-type=module", "-e", &script]).output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let actual: Vec<String> = serde_json::from_slice(&output.stdout).unwrap();
    for (case, actual) in cases.as_array().unwrap().iter().zip(actual) {
        assert_eq!(actual, case[1].as_str().unwrap(), "input: {}", case[0]);
    }
}
