//! Loading inside module namespaces on QuickJS and on Node: `import()` from CommonJS, one graph
//! that imports modules and the CommonJS requiring them, and each namespace's own `require.cache`.

mod common;
mod node_common;

use std::path::Path;

use common::*;
use node_common::*;
use pi_js::{JsRuntime, ModuleNamespace, VirtualModule};
use serde_json::json;

const SDK_STYLE: &str = "globalThis.sdkLoads = (globalThis.sdkLoads ?? 0) + 1;\n\
     export const token = {};\nexport default { kind: \"sdk\" };\n";

fn namespace(id: &str) -> ModuleNamespace {
    let mut namespace = ModuleNamespace::new(id).unwrap();
    namespace
        .insert_virtual_module("pi:runner", VirtualModule::Json(json!({ "id": id })))
        .insert_virtual_module("sdk-style", VirtualModule::Source(SDK_STYLE.into()))
        .insert_virtual_module("pi:data", VirtualModule::Json(json!({ "id": id, "list": [1] })))
        .insert_virtual_module("tla-style", VirtualModule::Source("await 0;\nexport const ready = true;\n".into()));
    namespace
}

fn register(runtime: &JsRuntime) {
    for id in ["a", "b"] {
        runtime.register_module_namespace(namespace(id)).unwrap();
    }
}

async fn import_in_both(runtime: &JsRuntime, specifier: &str) {
    for id in ["a", "b"] {
        runtime.import_namespaced(specifier, id).await.unwrap();
    }
}

async fn cjs_dynamic_import(runtime: &JsRuntime, root: &Path, _node: bool) {
    write(root, "state.mjs", "globalThis.stateLoads = (globalThis.stateLoads ?? 0) + 1;\nexport const box = {};\n");
    write(root, "lazy.cjs", "module.exports = { load: () => import(\"./state.mjs\") };\n");
    write(
        root,
        "entry.mjs",
        "import lazy from \"./lazy.cjs\";\nimport * as state from \"./state.mjs\";\nimport runner from \"pi:runner\";\n\
         (globalThis.records ??= {})[runner.id] = { lazy, state };\n",
    );
    write(
        root,
        "plain.mjs",
        "import lazy from \"./lazy.cjs\";\nimport * as state from \"./state.mjs\";\nglobalThis.plain = { lazy, state };\n",
    );
    register(runtime);
    import_in_both(runtime, "./entry.mjs").await;
    runtime.import("./plain.mjs").await.unwrap();
    let report = eval_json(
        runtime,
        r#"const { a, b } = globalThis.records;
const p = globalThis.plain;
const [la, lb, lp] = [await a.lazy.load(), await b.lazy.load(), await p.lazy.load()];
return {
  sameInstance: [la === a.state, lb === b.state, lp === p.state],
  apart: a.state.box !== b.state.box && a.state.box !== p.state.box && b.state.box !== p.state.box,
  loads: globalThis.stateLoads,
};"#,
    )
    .await;
    assert_eq!(report, json!({ "sameInstance": [true, true, true], "apart": true, "loads": 3 }));
}

const REQ: &str = r#"const sdk = require("sdk-style");
const data = require("pi:data");
const state = require("./state.mjs");
const text = require("node:fs").readFileSync(require("node:path").join(__dirname, "note.txt"), "utf8");
exports.sdk = sdk;
exports.data = data;
exports.state = state;
exports.where = { filename: __filename, dirname: __dirname, text, resolved: require.resolve("./state.mjs") };
"#;

const STATE: &str = r#"import { readFileSync } from "node:fs";
import { join } from "node:path";
globalThis.stateLoads = (globalThis.stateLoads ?? 0) + 1;
export const box = {};
export const where = {
  url: import.meta.url, filename: import.meta.filename, dirname: import.meta.dirname,
  text: readFileSync(join(import.meta.dirname, "note.txt"), "utf8"), resolved: import.meta.resolve("./note.txt"),
};
"#;

/// One graph root importing ES modules and the CommonJS that requires them, in either order.
fn graph_root(cjs_first: bool) -> String {
    let esm = "import { token } from \"sdk-style\";\nimport data from \"pi:data\";\nimport { box, where } from \"./state.mjs\";\n";
    let cjs = "import req, { sdk } from \"./req.cjs\";\nimport { early, fromB, partial, done } from \"./cyc-a.cjs\";\n\
               import tla from \"./tla.cjs\";\n";
    let (first, second) = if cjs_first { (cjs, esm) } else { (esm, cjs) };
    format!(
        "{first}{second}import runner from \"pi:runner\";\n(globalThis.graphs ??= {{}})[runner.id] = \
         {{ token, data, box, where, req, sdk, cycle: {{ early, fromB, partial, done }}, tla }};\n"
    )
}

async fn mixed_graph(runtime: &JsRuntime, root: &Path, node: bool) {
    write(root, "note.txt", "note");
    write(root, "state.mjs", STATE);
    write(root, "req.cjs", REQ);
    write(
        root,
        "cyc-a.cjs",
        "exports.early = \"a\";\nconst b = require(\"./cyc-b.cjs\");\nexports.fromB = b.seen;\nexports.partial = b.partial;\nexports.done = true;\n",
    );
    write(
        root,
        "cyc-b.cjs",
        "const a = require(\"./cyc-a.cjs\");\nexports.seen = a.early;\nexports.partial = a.done === undefined;\n",
    );
    write(
        root,
        "tla.cjs",
        "let outcome;\ntry { require(\"tla-style\"); outcome = \"loaded\"; } catch (error) { outcome = error.code; }\n\
         module.exports = outcome;\n",
    );
    write(root, "esm-first.mjs", &graph_root(false));
    write(root, "cjs-first.mjs", &graph_root(true));
    register(runtime);
    runtime.import_namespaced("./esm-first.mjs", "a").await.unwrap();
    runtime.import_namespaced("./cjs-first.mjs", "b").await.unwrap();
    let body = r#"const out = {};
for (const id of ["a", "b"]) {
  const g = globalThis.graphs[id];
  out[id] = {
    id: g.data.id,
    sameToken: g.req.sdk.token === g.token && g.sdk === g.req.sdk,
    sameState: g.req.state.box === g.box,
    requireIsValue: g.req.data === g.data,
    requireIsNamespace: g.req.data.default === g.data,
    cjs: g.req.where, esm: g.where, cycle: g.cycle,
  };
  TLA
}
const { a, b } = globalThis.graphs;
return { ...out, apart: a.token !== b.token && a.box !== b.box, loads: [globalThis.sdkLoads, globalThis.stateLoads] };"#;
    let report = eval_json(runtime, &body.replace("TLA", if node { "out[id].tla = g.tla;" } else { "" })).await;
    let path = |name: &str| root.join(name).display().to_string();
    let side = |id: &str| {
        let mut side = json!({
            "id": id, "sameToken": true, "sameState": true, "requireIsValue": !node, "requireIsNamespace": node,
            "cjs": { "filename": path("req.cjs"), "dirname": root.display().to_string(), "text": "note", "resolved": path("state.mjs") },
            "esm": {
                "url": format!("{}?pi-ns={id}", url_of(&root.join("state.mjs"))), "filename": path("state.mjs"),
                "dirname": root.display().to_string(), "text": "note",
                "resolved": format!("{}?pi-ns={id}", url_of(&root.join("note.txt"))),
            },
            "cycle": { "early": "a", "fromB": "a", "partial": true, "done": true },
        });
        if node {
            side["tla"] = json!("ERR_REQUIRE_ASYNC_MODULE");
        }
        side
    };
    assert_eq!(report, json!({ "a": side("a"), "b": side("b"), "apart": true, "loads": [2, 2] }));
}

fn url_of(path: &Path) -> String {
    url::Url::from_file_path(path).unwrap().to_string()
}

async fn require_cache(runtime: &JsRuntime, root: &Path, _node: bool) {
    write(root, "counter.cjs", "globalThis.counterLoads = (globalThis.counterLoads ?? 0) + 1;\nmodule.exports = {};\n");
    write(
        root,
        "cachey.cjs",
        r#"const key = require.resolve("./counter.cjs");
const first = require("./counter.cjs");
const own = require.cache[__filename] === module;
const listed = require.cache[key].exports === first;
delete require.cache[key];
const second = require("./counter.cjs");
module.exports = { own, listed, reloaded: first !== second, relisted: require.cache[key].exports === second };
"#,
    );
    write(
        root,
        "entry.mjs",
        "import cachey from \"./cachey.cjs\";\nimport runner from \"pi:runner\";\n\
         (globalThis.cacheRecords ??= {})[runner.id] = cachey;\n",
    );
    register(runtime);
    import_in_both(runtime, "./entry.mjs").await;
    let originals =
        json!([root.join("counter.cjs").display().to_string(), root.join("cachey.cjs").display().to_string()]);
    let forged = json!(format!("{}?pi-ns=a", url_of(&root.join("cachey.cjs"))));
    let report = eval_json(
        runtime,
        &format!(
            r#"const {{ default: Module }} = await import("node:module");
const names = Object.keys(Module._cache).filter((key) => {originals}.includes(key));
const loads = globalThis.counterLoads;
Module.createRequire({forged})("./counter.cjs");
const forged = [globalThis.counterLoads - loads, {originals}[0] in Module._cache];
return {{ a: globalThis.cacheRecords.a, b: globalThis.cacheRecords.b, loads, global: names, forged }};"#
        ),
    )
    .await;
    let side = json!({ "own": true, "listed": true, "reloaded": true, "relisted": true });
    assert_eq!(report, json!({ "a": side, "b": side, "loads": 4, "global": [], "forged": [1, true] }));
}

macro_rules! on_both_backends {
    ($scenario:ident, $quickjs:ident, $node:ident) => {
        #[tokio::test]
        async fn $quickjs() {
            let (_dir, root) = tempdir();
            let (runtime, _capture) = runtime(&root);
            $scenario(&runtime, &root, false).await;
        }

        #[tokio::test]
        async fn $node() {
            let (_dir, root) = tempdir();
            let Some((runtime, _capture)) = node_runtime(&root) else { return };
            $scenario(&runtime, &root, true).await;
            expect_node_version(&runtime).await;
        }
    };
}

on_both_backends!(
    cjs_dynamic_import,
    quickjs_cjs_dynamic_import_stays_in_namespace,
    node_cjs_dynamic_import_stays_in_namespace
);
on_both_backends!(
    mixed_graph,
    quickjs_one_graph_imports_and_requires_each_namespace_module_once,
    node_one_graph_imports_and_requires_each_namespace_module_once
);
on_both_backends!(
    require_cache,
    quickjs_namespaced_require_cache_is_the_namespace_cache,
    node_namespaced_require_cache_is_the_namespace_cache
);
