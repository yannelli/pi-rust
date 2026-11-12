//! Module identity inside namespaces on QuickJS and on Node: `import.meta` read through a cycle
//! before the module body runs, and Node cache keys that user code passes back as module paths.

mod common;
mod node_common;

use std::path::Path;

use common::*;
use node_common::*;
use pi_js::{JsRuntime, ModuleNamespace, VirtualModule};
use serde_json::json;

const A: &str = r#"import { readFileSync } from "node:fs";
import { join } from "node:path";
import { early } from "./b.mjs";
import runner from "pi:runner";
export function where() {
  return {
    meta: import.meta, url: import.meta.url, filename: import.meta.filename, dirname: import.meta.dirname,
    resolved: import.meta.resolve("./note.txt"), text: readFileSync(join(import.meta.dirname, "note.txt"), "utf8"),
  };
}
import.meta.mark = "user";
const later = where();
import.meta.url = "custom:url";
(globalThis.cycles ??= {})[runner.id] = {
  early, later, same: early.meta === later.meta && later.meta === import.meta, mark: early.meta.mark, written: where().url,
};
"#;

const PROBE: &str = r#"const counter = require("./counter.cjs");
module.exports = {
  counter,
  fields: {
    filename: module.filename, id: module.id, path: module.path, paths0: module.paths[0],
    own: require.cache[__filename] === module, parentPath: counter.parentPath,
    children: module.children.length > 0 && module.children.every((child) => child.parent === module),
  },
  req(spec) { try { require(spec); return "loaded"; } catch (error) { return error.code; } },
  imp(spec) { return import(spec).then(() => "loaded", (error) => error.code); },
};
"#;

const USER: &str = r#"const runner = require("pi:runner");
(globalThis.users ??= {})[(runner.default ?? runner).id] = {
  shared: require("./shared.cjs"),
  req(spec) { try { require(spec); return "loaded"; } catch (error) { return error.code; } },
  imp(spec) { return import(spec).then(() => "loaded", (error) => error.code); },
};
"#;

fn register(runtime: &JsRuntime) {
    for id in ["a", "b"] {
        let mut namespace = ModuleNamespace::new(id).unwrap();
        namespace.insert_virtual_module("pi:runner", VirtualModule::Json(json!({ "id": id })));
        runtime.register_module_namespace(namespace).unwrap();
    }
}

fn url_of(path: &Path) -> String {
    url::Url::from_file_path(path).unwrap().to_string()
}

async fn meta_cycle(runtime: &JsRuntime, root: &Path, _node: bool) {
    write(root, "note.txt", "note");
    write(root, "a.mjs", A);
    write(root, "b.mjs", "import { where } from \"./a.mjs\";\nexport const early = where();\n");
    register(runtime);
    for id in ["a", "b"] {
        runtime.import_namespaced("./a.mjs", id).await.unwrap();
    }
    let report = eval_json(
        runtime,
        r#"const out = {};
for (const id of ["a", "b"]) {
  const { early: { meta: m1, ...early }, later: { meta: m2, ...later }, ...rest } = globalThis.cycles[id];
  out[id] = { early, later, ...rest };
}
return out;"#,
    )
    .await;
    let side = |id: &str| {
        let place = json!({
            "url": format!("{}?pi-ns={id}", url_of(&root.join("a.mjs"))),
            "filename": root.join("a.mjs").display().to_string(),
            "dirname": root.display().to_string(),
            "resolved": format!("{}?pi-ns={id}", url_of(&root.join("note.txt"))),
            "text": "note",
        });
        json!({ "early": place, "later": place, "same": true, "mark": "user", "written": "custom:url" })
    };
    assert_eq!(report, json!({ "a": side("a"), "b": side("b") }));
}

async fn exposed_aliases(runtime: &JsRuntime, root: &Path, node: bool) {
    write(
        root,
        "counter.cjs",
        "globalThis.counterLoads = (globalThis.counterLoads ?? 0) + 1;\nmodule.exports = { parentPath: module.parent?.path };\n",
    );
    write(root, "probe.cjs", PROBE);
    write(
        root,
        "entry.mjs",
        "import probe from \"./probe.cjs\";\nimport runner from \"pi:runner\";\n(globalThis.probes ??= {})[runner.id] = probe;\n",
    );
    register(runtime);
    for id in ["a", "b"] {
        runtime.import_namespaced("./entry.mjs", id).await.unwrap();
    }
    let plain = json!(root.join("counter.cjs").display().to_string());
    let report = eval_json(
        runtime,
        &format!(
            r#"const {{ default: Module, createRequire }} = await import("node:module");
const {{ pathToFileURL }} = await import("node:url");
const path = await import("node:path");
const plain = {plain};
const exposed = Object.keys(Module._cache).filter((key) => key !== plain && path.basename(key) === "counter.cjs");
const shared = Module.createRequire(plain)("./counter.cjs");
const loads = globalThis.counterLoads;
const viaAlias = exposed.flatMap((key) => [key, pathToFileURL(key), pathToFileURL(key).href, path.dirname(key) + path.sep]
  .map((target) => createRequire(target)("./counter.cjs") === shared));
const {{ a, b }} = globalThis.probes;
const direct = [];
for (const key of exposed) {{
  const url = pathToFileURL(key).href;
  direct.push(a.req(key), b.req(key), await a.imp(url), await b.imp(url));
}}
return {{
  exposed: exposed.length, viaAlias, extraLoads: globalThis.counterLoads - loads, direct,
  namedIsModule: createRequire === Module.createRequire,
  apart: a.counter !== b.counter && a.counter !== shared && b.counter !== shared,
  fields: [a.fields, b.fields],
}};"#
        ),
    )
    .await;
    let aliases = if node { 2 } else { 0 };
    let direct = ["MODULE_NOT_FOUND", "MODULE_NOT_FOUND", "ERR_MODULE_NOT_FOUND", "ERR_MODULE_NOT_FOUND"];
    let fields = json!({
        "filename": root.join("probe.cjs").display().to_string(),
        "id": root.join("probe.cjs").display().to_string(),
        "path": root.display().to_string(),
        "paths0": root.join("node_modules").display().to_string(),
        "own": true,
        "parentPath": root.display().to_string(),
        "children": true,
    });
    assert_eq!(
        report,
        json!({
            "exposed": aliases,
            "viaAlias": vec![true; aliases * 4],
            "extraLoads": 0,
            "direct": direct.repeat(aliases),
            "namedIsModule": true,
            "apart": true,
            "fields": [fields, fields],
        })
    );
}

async fn virtual_aliases(runtime: &JsRuntime, root: &Path, node: bool) {
    write(root, "shared.cjs", "globalThis.sharedLoads = (globalThis.sharedLoads ?? 0) + 1;\n");
    write(root, "user.cjs", USER);
    write(root, "entry.mjs", "import \"./user.cjs\";\n");
    register(runtime);
    for id in ["a", "b"] {
        runtime.import_namespaced("./entry.mjs", id).await.unwrap();
    }
    let plain = json!(root.join("shared.cjs").display().to_string());
    let report = eval_json(
        runtime,
        &format!(
            r#"const {{ default: Module, createRequire }} = await import("node:module");
const {{ pathToFileURL }} = await import("node:url");
const path = await import("node:path");
const plain = {plain};
const keys = Object.keys(Module._cache);
const files = keys.filter((key) => key !== plain && path.basename(key) === "shared.cjs");
const virtuals = keys.filter((key) => key.endsWith(".a.mjs") || key.endsWith(".b.mjs"));
const shared = Module.createRequire(plain)(plain);
const loads = globalThis.sharedLoads;
const forms = (key) => {{
  const href = pathToFileURL(key).href;
  const dir = path.dirname(key);
  return [key, pathToFileURL(key), href, dir + path.sep, "FILE:" + href.slice(5), " \t" + href,
    dir + path.sep + "." + path.sep + path.basename(key)];
}};
const all = [...files, ...virtuals];
const via = all.flatMap((key) => forms(key).map((target) => createRequire(target)(plain) === shared));
const later = all.map((key) => createRequire(path.join(path.dirname(key), "later.cjs"))(plain) === shared);
const again = Module.createRequire(plain)("./shared.cjs") === shared;
const {{ a, b }} = globalThis.users;
const direct = [];
for (const key of virtuals) {{
  const url = pathToFileURL(key).href;
  direct.push(a.req(key), b.req(key), await a.imp(url), await b.imp(url));
}}
const loaded = all.map((key) => {{
  try {{ Module._load(key); return "loaded"; }} catch (error) {{ return error.code; }}
}});
return {{
  files: files.length, virtuals: virtuals.length, via, later, again, direct, loaded,
  extraLoads: globalThis.sharedLoads - loads,
  apart: a.shared !== b.shared && a.shared !== shared && b.shared !== shared,
}};"#
        ),
    )
    .await;
    let n = if node { 2 } else { 0 };
    let direct = ["MODULE_NOT_FOUND", "MODULE_NOT_FOUND", "ERR_MODULE_NOT_FOUND", "ERR_MODULE_NOT_FOUND"];
    assert_eq!(
        report,
        json!({
            "files": n,
            "virtuals": n,
            "via": vec![true; n * 2 * 7],
            "later": vec![true; n * 2],
            "again": true,
            "direct": direct.repeat(n),
            "loaded": vec!["MODULE_NOT_FOUND"; n * 2],
            "extraLoads": 0,
            "apart": true,
        })
    );
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
    meta_cycle,
    quickjs_import_meta_read_through_a_cycle_has_the_original_location,
    node_import_meta_read_through_a_cycle_has_the_original_location
);
on_both_backends!(
    exposed_aliases,
    quickjs_cache_keys_given_back_stay_out_of_namespaces,
    node_cache_keys_given_back_stay_out_of_namespaces
);
on_both_backends!(
    virtual_aliases,
    quickjs_virtual_module_cache_keys_stay_out_of_namespaces,
    node_virtual_module_cache_keys_stay_out_of_namespaces
);
