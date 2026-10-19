//! node:inspector and node:inspector/promises: sessions, Runtime evaluation and remote objects,
//! properties, calls, notifications, and the errors for domains QuickJS cannot provide. Expected
//! values come from Node 22 except for those domains.

mod common;

use common::*;

const SCRIPT: &str = r##"
const inspector = require("node:inspector");
const { Session } = require("node:inspector/promises");
const out = {};
out.api = [Object.keys(inspector).sort(), inspector.url(), typeof inspector.console.log, typeof inspector.Session];
const session = new inspector.Session();
const errorOf = (fn) => { try { fn(); } catch (e) { return [e.code, e.message]; } };
out.notConnected = errorOf(() => session.post("Runtime.evaluate", {}));
session.connect();
out.twice = errorOf(() => session.connect());
out.badMethod = errorOf(() => session.post(1));
out.badParams = errorOf(() => session.post("Runtime.evaluate", 5));
const calls = [];
const post = (method, params) => {
  let reply;
  session.post(method, params, (error, result) => (reply = error ? ["error", error.code, error.message] : result));
  calls.push(method);
  return reply;
};
const evaluate = (expression, extra) => post("Runtime.evaluate", { expression, ...extra });
out.sync = (() => {
  const order = [];
  session.post("Runtime.evaluate", { expression: "1 + 1" }, (error, result) => order.push(result.result.value));
  order.push("after");
  return order;
})();
out.values = ["undefined", "null", "NaN", "-0", "Infinity", "10n", "'s'", "true", "42"].map((expr) => evaluate(expr).result);
out.objects = ["[1, 2]", "new Map([[1, 2]])", "new Set([1])", "new Uint8Array(3)", "/re/g", "Promise.resolve(1)", "({ a: 1 })", "(class Foo {}, new (class Foo {})())", "Object.create(null)", "Symbol('q')"].map((expr) => {
  const { objectId, ...rest } = evaluate(expr).result;
  return [typeof objectId, rest];
});
out.byValue = ["[1, 'a', { b: 2 }]", "({ a: 1, b: 'x' })", "undefined", "NaN", "new Map([[1, 2]])", "3"].map((expr) => evaluate(expr, { returnByValue: true }).result);
globalThis.shared = 5;
out.globals = evaluate("shared * 2").result.value;
const thrown = evaluate("throw new TypeError('bad')");
out.thrown = [thrown.result.subtype, thrown.result.className, thrown.result.description.split("\n")[0], thrown.exceptionDetails.text, typeof thrown.exceptionDetails.exceptionId, thrown.exceptionDetails.exception.className];
const syntax = evaluate("syntax error here");
out.syntax = [syntax.result.className, syntax.exceptionDetails.text];
const target = evaluate("({ x: 1, get y() { return 2; }, [Symbol('s')]: 3 })").result.objectId;
const props = post("Runtime.getProperties", { objectId: target, ownProperties: true });
out.props = [Object.keys(props).sort(), props.result.map((p) => [p.name, p.value?.type ?? null, p.value?.value ?? null, p.get?.type ?? null, p.writable ?? null, p.enumerable, p.configurable, p.isOwn, p.symbol?.description ?? null]), props.internalProperties.map((p) => [p.name, p.value.className])];
out.call = post("Runtime.callFunctionOn", { objectId: target, functionDeclaration: "function (k) { return this.x + k; }", arguments: [{ value: 5 }], returnByValue: true });
out.callNoTarget = post("Runtime.callFunctionOn", { functionDeclaration: "function () { return 1; }" });
out.release = [post("Runtime.releaseObject", { objectId: target }), post("Runtime.getProperties", { objectId: target })];
out.noExpression = post("Runtime.evaluate", {});
out.unknown = post("Foo.bar", {});
out.compileDisabled = post("Runtime.compileScript", { expression: "1 + 1", sourceURL: "x.js", persistScript: false });
out.heap = Object.keys(post("Runtime.getHeapUsage")).sort();
const notifications = [];
session.on("inspectorNotification", (message) => notifications.push(message.method));
const created = [];
session.on("Runtime.executionContextCreated", (message) => created.push([message.params.context.id, message.params.context.auxData]));
out.enable = post("Runtime.enable");
out.created = created;
out.compiled = [post("Runtime.compileScript", { expression: "1 +", sourceURL: "x.js", persistScript: false }).exceptionDetails.text, post("Runtime.compileScript", { expression: "1 + 1", sourceURL: "x.js", persistScript: false })];
const consoleEvents = [];
session.on("Runtime.consoleAPICalled", (message) => consoleEvents.push([message.params.type, message.params.args.map((arg) => arg.value ?? arg.type)]));
const original = console.log;
console.log = () => {};
try {
  inspector.console.warn("w", 1);
} finally {
  console.log = original;
}
out.console = consoleEvents;
const chunks = [];
session.on("HeapProfiler.addHeapSnapshotChunk", (message) => chunks.push(message.params.chunk));
out.snapshot = await new Promise((resolve) => session.post("HeapProfiler.takeHeapSnapshot", null, (error, result) => resolve(result)));
await new Promise((resolve) => setImmediate(resolve));
out.snapshotChunks = "snapshot" in JSON.parse(chunks.join(""));
out.awaited = await new Promise((resolve) => session.post("Runtime.evaluate", { expression: "Promise.resolve(42)", awaitPromise: true }, (error, result) => resolve(result.result)));
let pending;
session.post("Runtime.evaluate", { expression: "new Promise(() => {})", awaitPromise: true }, (error) => (pending = [error.code, error.message]));
session.disconnect();
out.closed = pending;
out.afterDisconnect = errorOf(() => session.post("Runtime.evaluate", {}));
out.wait = errorOf(() => inspector.waitForDebugger());
out.mainThread = errorOf(() => new inspector.Session().connectToMainThread());
const promised = new Session();
promised.connect();
out.promises = [(await promised.post("Runtime.evaluate", { expression: "6 * 7" })).result.value, await promised.post("Nope.nope").catch((e) => [e.code, e.message])];
promised.disconnect();
return out;
"##;

const EXPECTED: &str = r##"{"api":[["Network","NetworkResources","Session","close","console","open","url","waitForDebugger"],null,"function","function"],"notConnected":["ERR_INSPECTOR_NOT_CONNECTED","Session is not connected"],"twice":["ERR_INSPECTOR_ALREADY_CONNECTED","The inspector session is already connected"],"badMethod":["ERR_INVALID_ARG_TYPE","The \"method\" argument must be of type string. Received type number (1)"],"badParams":["ERR_INVALID_ARG_TYPE","The \"params\" argument must be of type object. Received type number (5)"],"sync":[2,"after"],"values":[{"type":"undefined"},{"type":"object","subtype":"null","value":null},{"type":"number","unserializableValue":"NaN","description":"NaN"},{"type":"number","unserializableValue":"-0","description":"-0"},{"type":"number","unserializableValue":"Infinity","description":"Infinity"},{"type":"bigint","unserializableValue":"10n","description":"10n"},{"type":"string","value":"s"},{"type":"boolean","value":true},{"type":"number","value":42,"description":"42"}],"objects":[["string",{"type":"object","subtype":"array","className":"Array","description":"Array(2)"}],["string",{"type":"object","subtype":"map","className":"Map","description":"Map(1)"}],["string",{"type":"object","subtype":"set","className":"Set","description":"Set(1)"}],["string",{"type":"object","subtype":"typedarray","className":"Uint8Array","description":"Uint8Array(3)"}],["string",{"type":"object","subtype":"regexp","className":"RegExp","description":"/re/g"}],["string",{"type":"object","subtype":"promise","className":"Promise","description":"Promise"}],["string",{"type":"object","className":"Object","description":"Object"}],["string",{"type":"object","className":"Foo","description":"Foo"}],["string",{"type":"object","className":"Object","description":"Object"}],["string",{"type":"symbol","description":"Symbol(q)"}]],"byValue":[{"type":"object","value":[1,"a",{"b":2}]},{"type":"object","value":{"a":1,"b":"x"}},{"type":"undefined"},{"type":"number","unserializableValue":"NaN","description":"NaN"},{"type":"object","value":{}},{"type":"number","value":3,"description":"3"}],"globals":10,"thrown":["error","TypeError","TypeError: bad","Uncaught","number","TypeError"],"syntax":["SyntaxError","Uncaught"],"props":[["internalProperties","result"],[["x","number",1,null,true,true,true,true,null],["y",null,null,"function",null,true,true,true,null],["Symbol(s)","number",3,null,true,true,true,true,"Symbol(s)"]],[["[[Prototype]]","Object"]]],"call":{"result":{"type":"number","value":6,"description":"6"}},"callNoTarget":["error","ERR_INSPECTOR_COMMAND","Inspector error -32602: Either objectId or executionContextId or uniqueContextId must be specified"],"release":[{},["error","ERR_INSPECTOR_COMMAND","Inspector error -32000: Could not find object with given id"]],"noExpression":["error","ERR_INSPECTOR_COMMAND","Inspector error -32602: Invalid parameters"],"unknown":["error","ERR_INSPECTOR_COMMAND","Inspector error -32601: 'Foo.bar' wasn't found"],"compileDisabled":["error","ERR_INSPECTOR_COMMAND","Inspector error -32000: Runtime agent is not enabled"],"heap":["totalSize","usedSize"],"enable":{},"created":[[1,{"isDefault":true}]],"compiled":["Uncaught",{}],"console":[["warning",["w",1]]],"snapshot":{},"snapshotChunks":true,"awaited":{"type":"number","value":42,"description":"42"},"closed":["ERR_INSPECTOR_COMMAND","Inspector error -32000: Execution context was destroyed."],"afterDisconnect":["ERR_INSPECTOR_NOT_CONNECTED","Session is not connected"],"wait":["ERR_INSPECTOR_NOT_ACTIVE","Inspector is not active"],"mainThread":["ERR_INSPECTOR_NOT_WORKER","Current thread is not a worker"],"promises":[42,["ERR_INSPECTOR_COMMAND","Inspector error -32601: 'Nope.nope' wasn't found"]]}"##;

#[tokio::test]
async fn inspector_sessions_match_node() {
    let (_dir, root) = tempdir();
    let (runtime, _capture) = runtime(&root);
    let actual = run_cjs(&runtime, &root, SCRIPT).await;
    let expected: serde_json::Value = serde_json::from_str(EXPECTED).unwrap();
    assert_same_keys(&actual, &expected);
}

#[tokio::test]
async fn inspector_domains_without_quickjs_support() {
    let (_dir, root) = tempdir();
    let (runtime, _capture) = runtime(&root);
    let actual = run_cjs(&runtime, &root, r#"
const inspector = require("inspector");
const session = new inspector.Session();
session.connect();
const replies = [];
for (const method of ["Debugger.enable", "Profiler.start", "Schema.getDomains"]) {
  session.post(method, (error, result) => replies.push(error ? [error.code, error.message] : result.domains.map((d) => d.name)));
}
let open;
try { inspector.open(); } catch (e) { open = [e.code, e.message]; }
return { replies, open, features: process.features.inspector };
"#).await;
    let expected = serde_json::json!({
        "replies": [
            ["ERR_INSPECTOR_COMMAND", "Inspector error -32601: 'Debugger.enable' wasn't found (the Debugger domain needs V8 and is not available in pi-rust)"],
            ["ERR_INSPECTOR_COMMAND", "Inspector error -32601: 'Profiler.start' wasn't found (the Profiler domain needs V8 and is not available in pi-rust)"],
            ["Runtime", "HeapProfiler", "Schema"]
        ],
        "open": ["ERR_PI_UNSUPPORTED", "inspector.open (pi-rust has no DevTools server) is not supported in pi-rust"],
        "features": false
    });
    assert_eq!(actual, expected);
}
