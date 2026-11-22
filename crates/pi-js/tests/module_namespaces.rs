//! Module namespaces on QuickJS and on Node: separate ES and CommonJS instances, shared keys and
//! built-ins, per-namespace virtual modules, and host functions removed by name.

mod common;
mod node_common;

use std::path::Path;

use common::*;
use node_common::*;
use pi_js::{HostFunction, JsRuntime, ModuleNamespace, VirtualModule};
use serde_json::json;

const ENTRY: &str = r#"import dep from "./dep.cjs";
import user from "./user.cjs";
import { viaRequire } from "./helper.ts";
import { box } from "./state.mjs";
import { token } from "./shared.mjs";
import { readFileSync } from "node:fs";
import runner from "pi:runner";
globalThis.entryLoads = (globalThis.entryLoads ?? 0) + 1;
const stateUrl = new URL("./state.mjs", import.meta.url).href;
(globalThis.records ??= {})[runner.id] = {
  dep, user, viaRequire, box, token, readFileSync, runner: runner.id,
  lazy: () => import("./state.mjs"),
  foreign: () => import(stateUrl + "?pi-ns=" + (runner.id === "a" ? "b" : "a")),
  meta: { url: import.meta.url, filename: import.meta.filename, dirname: import.meta.dirname, resolved: import.meta.resolve("./state.mjs") },
};
"#;

const PLAIN: &str = r#"import { box } from "./state.mjs";
import { token } from "./shared.mjs";
import dep from "./dep.cjs";
const stateUrl = new URL("./state.mjs", import.meta.url).href;
globalThis.plain = { box, token, dep, viaQuery: () => import(stateUrl + "?pi-ns=a"), viaKey: (key) => import(key) };
"#;

fn fixture(root: &Path) {
    write(root, "dep.cjs", "globalThis.depLoads = (globalThis.depLoads ?? 0) + 1;\nmodule.exports = { tag: {} };\n");
    write(
        root,
        "user.cjs",
        "const dep = require(\"./dep.cjs\");\nmodule.exports = { dep, filename: __filename, resolved: require.resolve(\"./dep.cjs\") };\n",
    );
    write(root, "helper.ts", "export const viaRequire: unknown = require(\"./dep.cjs\");\n");
    write(root, "state.mjs", "globalThis.stateLoads = (globalThis.stateLoads ?? 0) + 1;\nexport const box = {};\n");
    write(
        root,
        "shared.mjs",
        "globalThis.sharedLoads = (globalThis.sharedLoads ?? 0) + 1;\nexport const token = {};\n",
    );
    write(root, "entry.mjs", ENTRY);
    write(root, "plain.mjs", PLAIN);
}

fn namespace(root: &Path, id: &str) -> ModuleNamespace {
    let mut namespace = ModuleNamespace::new(id).unwrap();
    namespace.share(root.join("shared.mjs").display().to_string());
    namespace.insert_virtual_module("pi:runner", VirtualModule::Json(json!({ "id": id })));
    namespace
}

fn file_url(path: &Path) -> String {
    url::Url::from_file_path(path).unwrap().to_string()
}

async fn instances(runtime: &JsRuntime, root: &Path) {
    fixture(root);
    runtime.register_module_namespace(namespace(root, "a")).unwrap();
    runtime.register_module_namespace(namespace(root, "b")).unwrap();
    for id in ["a", "b", "a"] {
        runtime.import_namespaced("./entry.mjs", id).await.unwrap();
    }
    runtime.import("./plain.mjs").await.unwrap();
    let report = eval_json(
        runtime,
        r#"const { a, b } = globalThis.records;
const p = globalThis.plain;
const [lazyA, lazyB, foreignA, foreignB] = [await a.lazy(), await b.lazy(), await a.foreign(), await b.foreign()];
return {
  loads: [globalThis.entryLoads, globalThis.depLoads, globalThis.stateLoads, globalThis.sharedLoads],
  requireMatchesImport: [a, b].every((r) => r.dep === r.user.dep && r.dep === r.viaRequire),
  depApart: a.dep !== b.dep && a.dep !== p.dep && b.dep !== p.dep,
  stateApart: a.box !== b.box && a.box !== p.box && b.box !== p.box,
  dynamicStays: lazyA.box === a.box && lazyB.box === b.box,
  queryIgnored: foreignA.box === a.box && foreignB.box === b.box,
  sharedOnce: a.token === b.token && a.token === p.token,
  builtinOnce: a.readFileSync === b.readFileSync,
  runners: [a.runner, b.runner],
  meta: a.meta,
  user: [a.user.filename, a.user.resolved],
};"#,
    )
    .await;
    let entry = root.join("entry.mjs");
    assert_same_keys(
        &report,
        &json!({
            "loads": [2, 3, 3, 1],
            "requireMatchesImport": true,
            "depApart": true,
            "stateApart": true,
            "dynamicStays": true,
            "queryIgnored": true,
            "sharedOnce": true,
            "builtinOnce": true,
            "runners": ["a", "b"],
            "meta": {
                "url": format!("{}?pi-ns=a", file_url(&entry)),
                "filename": entry.display().to_string(),
                "dirname": root.display().to_string(),
                "resolved": format!("{}?pi-ns=a", file_url(&root.join("state.mjs"))),
            },
            "user": [root.join("user.cjs").display().to_string(), root.join("dep.cjs").display().to_string()],
        }),
    );
}

async fn selection(runtime: &JsRuntime, root: &Path) {
    fixture(root);
    runtime.register_module_namespace(namespace(root, "a")).unwrap();
    runtime.import_namespaced("./entry.mjs", "a").await.unwrap();
    runtime.import("./plain.mjs").await.unwrap();
    let key = format!("pi-ns:a:{}", root.join("state.mjs").display());
    let report = eval_json(
        runtime,
        &format!(
            r#"const p = globalThis.plain;
const viaQuery = await p.viaQuery();
let viaKey;
try {{ await p.viaKey({key}); viaKey = "loaded"; }} catch (error) {{ viaKey = "rejected"; }}
return {{ queryIsPlain: viaQuery.box === p.box, queryIsNotA: viaQuery.box !== globalThis.records.a.box, viaKey }};"#,
            key = json!(key)
        ),
    )
    .await;
    assert_eq!(report, json!({ "queryIsPlain": true, "queryIsNotA": true, "viaKey": "rejected" }));
    if cfg!(unix) {
        write(root, "odd/q?x.mjs", "export const meta = { url: import.meta.url, filename: import.meta.filename };\n");
        let odd = runtime.import_namespaced("./odd/q?x.mjs", "a").await.unwrap();
        let plain = runtime.import("./odd/q?x.mjs").await.unwrap();
        let path = root.join("odd/q?x.mjs");
        let url = format!("{}?pi-ns=a", file_url(&path));
        assert!(url.contains("q%3Fx.mjs?pi-ns=a"), "{url}");
        let expected = json!({ "meta": { "url": url, "filename": path.display().to_string() } });
        assert_eq!(odd.to_json().await.unwrap(), expected);
        assert_eq!(plain.to_json().await.unwrap()["meta"]["url"], json!(file_url(&path)));
    }
}

async fn registration(runtime: &JsRuntime, root: &Path) {
    fixture(root);
    runtime.register_module_namespace(namespace(root, "a")).unwrap();
    let duplicate = runtime.register_module_namespace(namespace(root, "a")).unwrap_err();
    assert_eq!(duplicate.code.as_deref(), Some("ERR_PI_MODULE_NAMESPACE"));
    let unknown = runtime.import_namespaced("./entry.mjs", "missing").await.unwrap_err();
    assert_eq!(unknown.code.as_deref(), Some("ERR_PI_MODULE_NAMESPACE"), "{}", unknown.message);
    for id in ["", "a:b", "a/b", "é", &"x".repeat(65)] {
        assert_eq!(ModuleNamespace::new(id).unwrap_err().code.as_deref(), Some("ERR_PI_MODULE_NAMESPACE"), "{id:?}");
    }
    let unresolved = runtime.import("./entry.mjs").await.unwrap_err();
    assert!(unresolved.message.contains("pi:runner"), "{}", unresolved.message);
}

async fn host_functions(runtime: &JsRuntime, root: &Path) {
    for name in ["child#1", "child#2", "parent"] {
        runtime.register_host_function(name, HostFunction::sync(move |_| Ok(json!(name).into())));
    }
    write(
        root,
        "host.mjs",
        r#"import host from "pi:host";
globalThis.probe = (names) => names.map((name) => {
  try { return { ok: host.call(name), has: host.has(name) }; } catch (error) { return { code: error.code, message: error.message }; }
});
"#,
    );
    runtime.import("./host.mjs").await.unwrap();
    assert!(runtime.unregister_host_function("child#1"));
    assert!(!runtime.unregister_host_function("child#1"));
    let probe = r#"return globalThis.probe(["child#1", "child#2", "parent", "missing"]);"#;
    assert_eq!(
        eval_json(runtime, probe).await,
        json!([
            { "code": "ERR_PI_HOST_FUNCTION_DISPOSED", "message": "The host function child#1 was unregistered" },
            { "ok": "child#2", "has": true },
            { "ok": "parent", "has": true },
            { "code": "ERR_PI_HOST_FUNCTION", "message": "Unknown host function: missing" },
        ])
    );
    runtime.register_host_function("child#1", HostFunction::sync(|_| Ok(json!("again").into())));
    assert_eq!(eval_json(runtime, probe).await[0], json!({ "ok": "again", "has": true }));
}

macro_rules! on_both_backends {
    ($scenario:ident, $quickjs:ident, $node:ident) => {
        #[tokio::test]
        async fn $quickjs() {
            let (_dir, root) = tempdir();
            let (runtime, _capture) = runtime(&root);
            $scenario(&runtime, &root).await;
        }

        #[tokio::test]
        async fn $node() {
            let (_dir, root) = tempdir();
            let Some((runtime, _capture)) = node_runtime(&root) else { return };
            $scenario(&runtime, &root).await;
            expect_node_version(&runtime).await;
        }
    };
}

on_both_backends!(instances, quickjs_namespaces_have_own_instances, node_namespaces_have_own_instances);
on_both_backends!(selection, quickjs_specifiers_cannot_select_a_namespace, node_specifiers_cannot_select_a_namespace);
on_both_backends!(registration, quickjs_namespace_registration_errors, node_namespace_registration_errors);
on_both_backends!(host_functions, quickjs_unregistered_host_functions, node_unregistered_host_functions);
