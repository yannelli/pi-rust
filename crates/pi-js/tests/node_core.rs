mod common;

use common::*;

// Expected values come from running the same script under Node, except `unsupported`:
// node:inspector exists in Node and throws the pi-rust unsupported error here.
const CORE_SCRIPT: &str = r##"
const out = {};
const order = [];
await new Promise((resolve) => {
  setTimeout(() => {
    order.push("timeout");
    setImmediate(() => order.push("immediate-from-timeout"));
    setTimeout(() => { order.push("timeout2"); resolve(); }, 5);
    process.nextTick(() => order.push("tick-from-timeout"));
    Promise.resolve().then(() => order.push("promise-from-timeout"));
  }, 0);
  Promise.resolve().then(() => { order.push("promise"); process.nextTick(() => order.push("tick-in-promise")); });
  process.nextTick(() => { order.push("tick"); Promise.resolve().then(() => order.push("promise-in-tick")); });
  queueMicrotask(() => order.push("microtask"));
});
out.order = order;
let count = 0;
await new Promise((resolve) => { const id = setInterval(() => { if (++count === 3) { clearInterval(id); resolve(); } }, 1); });
out.interval = count;
const cleared = setTimeout(() => (out.cleared = "fired"), 1);
clearTimeout(cleared);
const timer = setTimeout(() => {}, 1000);
out.timerApi = [typeof timer.ref, timer.hasRef(), timer.unref().hasRef(), typeof timer[Symbol.toPrimitive]()];
clearTimeout(timer);
out.timersPromises = await require("timers/promises").setTimeout(2, "value");
const assert = require("assert");
const check = (fn) => { try { fn(); return "pass"; } catch (e) { return [e.name, e.code, e.message.split("\n")[0]]; } };
out.assert = [
  check(() => assert.strictEqual(1, 2)), check(() => assert.deepStrictEqual({ a: [1] }, { a: [1] })),
  check(() => assert.deepEqual({ a: 1 }, { a: "1" })), check(() => assert.deepStrictEqual({ a: 1 }, { a: "1" })),
  check(() => assert.throws(() => { throw new TypeError("x"); }, TypeError)), check(() => assert.throws(() => { throw new Error("boom"); }, /boom/)),
  check(() => assert.throws(() => { throw new Error("boom"); }, { message: "boom" })), check(() => assert.throws(() => {})),
  check(() => assert.ok(0)), check(() => assert(true)), check(() => assert.match("abc", /b/)), check(() => assert.notStrictEqual(1, 1)),
  check(() => assert.equal(1, "1")), check(() => assert.strict.equal(1, "1")), check(() => assert.ifError(null)), check(() => assert.fail("nope")),
  check(() => require("assert/strict").deepEqual([1], ["1"])),
];
out.rejects = await assert.rejects(Promise.reject(new Error("r")), /r/).then(() => "ok");
const path = require("path");
out.path = [path.join("/a/b", "../c", "./d"), path.resolve("/x", "y", "..", "z"), path.relative("/a/b/c", "/a/d"), path.parse("/home/u/file.tar.gz"),
  path.format({ dir: "/d", name: "n", ext: ".e" }), path.normalize("a//b/../c/"), path.isAbsolute("./x"), path.extname("x.y.z"), path.basename("/p/q.js", ".js"),
  path.dirname("/p/q/r"), path.sep, path.delimiter, path.win32.join("C:\\a", "..", "b"), path.win32.resolve("C:\\a", "b"), path.posix === path];
const util = require("util");
out.util = [util.format("%s %d %i %j %o %%", "s", 4.5, 4.5, { j: 1 }, [1]), util.format("a", "b", 3), util.inspect({ a: { b: { c: { d: 1 } } }, s: "x", n: null, u: undefined, f() {}, arr: [1, 2], m: new Map([[1, 2]]), set: new Set([3]) }),
  util.inspect("str"), util.inspect(new Error("e")).split("\n")[0], util.types.isPromise(Promise.resolve()), util.types.isRegExp(/x/), util.isDeepStrictEqual({ a: 1 }, { a: 1 }),
  await util.promisify((v, cb) => cb(null, v + 1))(1), util.inspect(Symbol("s")), util.inspect([1, [2, [3, [4, [5]]]]]), util.inspect({ toString() { return "x"; } }), util.inspect(new Map()), util.inspect(-0)];
const EventEmitter = require("events");
const emitter = new EventEmitter();
const seen = [];
emitter.on("e", (v) => seen.push(["on", v]));
emitter.prependListener("e", (v) => seen.push(["prepend", v]));
emitter.once("e", (v) => seen.push(["once", v]));
emitter.emit("e", 1);
emitter.emit("e", 2);
out.events = [seen, emitter.listenerCount("e"), emitter.eventNames(), check(() => emitter.emit("error", new Error("unhandled"))), EventEmitter.defaultMaxListeners];
const ac = new AbortController();
const iterated = [];
setTimeout(() => { emitter.emit("tick", 1); emitter.emit("tick", 2); ac.abort(); }, 1);
try { for await (const [value] of EventEmitter.on(emitter, "tick", { signal: ac.signal })) iterated.push(value); } catch (e) { iterated.push(e.name); }
out.eventsOn = iterated;
console.log("log %s", "line", { k: 1 });
console.error("err");
out.consoleHas = ["log", "warn", "error", "info", "debug", "dir", "time", "timeEnd", "count", "group", "groupEnd", "assert", "trace"].every((k) => typeof console[k] === "function");
out.process = [typeof process.pid, process.platform === require("os").platform(), typeof process.hrtime.bigint(), process.argv.length > 0, typeof process.env.PATH, process.version.startsWith("v"), typeof process.memoryUsage().rss, process.exitCode, typeof process.uptime()];
out.module = [require("module").builtinModules.includes("fs"), typeof require("module").createRequire(__filename)("path").join, require.resolve("fs"), typeof require("node:module").isBuiltin];
out.misc = [typeof require("tty").isatty(1), require("constants").ENOENT, typeof require("diagnostics_channel").channel("x").publish, await require("stream/consumers").text(require("stream").Readable.from(["a", "b"]))];
let unsupported;
try { require("inspector"); } catch (e) { unsupported = [e.code, e.message]; }
out.unsupported = unsupported;
return out;
"##;

const CORE_EXPECTED: &str = r##"{"order": ["tick", "promise", "microtask", "promise-in-tick", "tick-in-promise", "timeout", "tick-from-timeout", "promise-from-timeout", "immediate-from-timeout", "timeout2"], "interval": 3, "timerApi": ["function", true, false, "number"], "timersPromises": "value", "assert": [["AssertionError", "ERR_ASSERTION", "Expected values to be strictly equal:"], "pass", "pass", ["AssertionError", "ERR_ASSERTION", "Expected values to be strictly deep-equal:"], "pass", "pass", "pass", ["AssertionError", "ERR_ASSERTION", "Missing expected exception."], ["AssertionError", "ERR_ASSERTION", "The expression evaluated to a falsy value:"], "pass", "pass", ["AssertionError", "ERR_ASSERTION", "Expected \"actual\" to be strictly unequal to: 1"], "pass", ["AssertionError", "ERR_ASSERTION", "Expected values to be strictly equal:"], "pass", ["AssertionError", "ERR_ASSERTION", "nope"], ["AssertionError", "ERR_ASSERTION", "Expected values to be strictly deep-equal:"]], "rejects": "ok", "path": ["/a/c/d", "/x/z", "../../d", {"root": "/", "dir": "/home/u", "base": "file.tar.gz", "ext": ".gz", "name": "file.tar"}, "/d/n.e", "a/c/", false, ".z", "q", "/p/q", "/", ":", "C:\\b", "C:\\a\\b", true], "util": ["s 4.5 4 {\"j\":1} [ 1, [length]: 1 ] %", "a b 3", "{\n  a: { b: { c: [Object] } },\n  s: 'x',\n  n: null,\n  u: undefined,\n  f: [Function: f],\n  arr: [ 1, 2 ],\n  m: Map(1) { 1 => 2 },\n  set: Set(1) { 3 }\n}", "'str'", "Error: e", true, true, true, 2, "Symbol(s)", "[ 1, [ 2, [ 3, [Array] ] ] ]", "{ toString: [Function: toString] }", "Map(0) {}", "-0"], "events": [[["prepend", 1], ["on", 1], ["once", 1], ["prepend", 2], ["on", 2]], 2, ["e"], ["Error", null, "unhandled"], 10], "eventsOn": [1, 2, "AbortError"], "consoleHas": true, "process": ["number", true, "bigint", true, "string", true, "number", null, "number"], "module": [true, "function", "fs", "function"], "misc": ["boolean", 2, "function", "ab"], "unsupported": ["ERR_PI_UNSUPPORTED", "Node module 'node:inspector' is not supported in pi-rust"]}"##;

#[tokio::test]
#[cfg_attr(windows, ignore = "expects POSIX paths")]
async fn timers_assert_path_util_events_and_process_match_node() {
    let (_dir, root) = tempdir();
    let (runtime, capture) = runtime(&root);
    let actual = run_cjs(&runtime, &root, CORE_SCRIPT).await;
    let expected: serde_json::Value = serde_json::from_str(CORE_EXPECTED).unwrap();
    assert_same_keys(&actual, &expected);
    assert_eq!(capture.stdout(), "log line { k: 1 }\n");
    assert_eq!(capture.stderr(), "err\n");
}

const MIME_SCRIPT: &str = r##"
const { MIMEType, MIMEParams } = require("node:util");
const mime = new MIMEType("text/ecmascript;key=value");
const before = [mime.type, mime.subtype, mime.essence, mime.params.get("key"), String(mime)];
mime.type = "Application";
const quoted = new MIMEType(' Text/Plain ; charset="utf-8"; Foo=Bar; foo=dup; empty=; "x" ');
quoted.params.set("q", "a b\"c");
const params = new MIMEParams();
params.set("a", "1");
let error;
try { new MIMEType("text/plain/x"); } catch (e) { error = [e.code, e.message, e instanceof TypeError]; }
return {
  before,
  after: [mime.toString(), mime.essence],
  quoted: [quoted.toString(), [...quoted.params.keys()], quoted.params.has("empty"), JSON.stringify({ m: quoted })],
  params: [params.toString(), [...params], params.get("b")],
  error,
};
"##;

#[tokio::test]
async fn util_mime_type_parses_like_node() {
    let (_dir, root) = tempdir();
    let (runtime, _capture) = runtime(&root);
    let actual = run_cjs(&runtime, &root, MIME_SCRIPT).await;
    let expected = serde_json::json!({
        "before": ["text", "ecmascript", "text/ecmascript", "value", "text/ecmascript;key=value"],
        "after": ["application/ecmascript;key=value", "application/ecmascript"],
        "quoted": [
            "text/plain;charset=utf-8;foo=Bar;q=\"a b\\\"c\"",
            ["charset", "foo", "q"],
            false,
            "{\"m\":\"text/plain;charset=utf-8;foo=Bar;q=\\\"a b\\\\\\\"c\\\"\"}"
        ],
        "params": ["a=1", [["a", "1"]], null],
        "error": ["ERR_INVALID_MIME_SYNTAX", "The MIME syntax for a subtype in \"text/plain/x\" is invalid at 5", true]
    });
    assert_same_keys(&actual, &expected);
}
