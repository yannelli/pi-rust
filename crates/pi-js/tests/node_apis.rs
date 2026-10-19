//! Node APIs that once threw ERR_PI_UNSUPPORTED: util, path globs, perf_hooks, events,
//! diagnostics_channel, stream.compose, buffer, assert.CallTracker, and module.

mod common;

use common::*;
use serde_json::json;

async fn run(body: &str) -> serde_json::Value {
    let (_dir, root) = tempdir();
    let (runtime, _capture) = runtime(&root);
    run_cjs(&runtime, &root, body).await
}

#[tokio::test]
async fn util_parse_args_follows_node() {
    let actual = run(r#"
const { parseArgs } = require("util");
const err = (fn) => { try { fn(); return "no error"; } catch (e) { return [e.code, e.message]; } };
const options = { verbose: { type: "boolean", short: "v" }, name: { type: "string", short: "n" }, tag: { type: "string", multiple: true }, level: { type: "string", default: "info" } };
const basic = parseArgs({ args: ["-v", "--name", "pi", "--tag=a", "--tag", "b", "-nfoo"], options });
const group = parseArgs({ args: ["-vn", "x"], options });
const tokens = parseArgs({ args: ["-v", "pos", "--", "--name"], options, allowPositionals: true, tokens: true });
const loose = parseArgs({ args: ["--unknown", "--name=v", "p"], options, strict: false });
const negative = parseArgs({ args: ["--no-verbose"], options, allowNegative: true });
return {
  basic: { ...basic.values, proto: Object.getPrototypeOf(basic.values) === null, positionals: basic.positionals },
  group: { ...group.values },
  tokens: tokens.tokens, tokensPositionals: tokens.positionals,
  loose: { ...loose.values, positionals: loose.positionals },
  negative: { ...negative.values },
  errors: [
    err(() => parseArgs({ args: ["--nope"], options })),
    err(() => parseArgs({ args: ["--nope"], options, allowPositionals: true })),
    err(() => parseArgs({ args: ["--name"], options })),
    err(() => parseArgs({ args: ["--verbose=1"], options })),
    err(() => parseArgs({ args: ["extra"], options })),
    err(() => parseArgs({ args: ["--name", "-v"], options })),
    err(() => parseArgs({ args: "x" })),
    err(() => parseArgs({ options: { a: { type: "number" } } })),
    err(() => parseArgs({ options: { a: { type: "string", short: "ab" } } })),
  ],
};
"#)
    .await;
    assert_eq!(
        actual["basic"],
        json!({"verbose": true, "name": "foo", "tag": ["a", "b"], "level": "info", "proto": true, "positionals": []})
    );
    assert_eq!(actual["group"], json!({"verbose": true, "name": "x", "level": "info"}));
    assert_eq!(
        actual["tokens"],
        json!([
            {"kind": "option", "name": "verbose", "rawName": "-v", "index": 0},
            {"kind": "positional", "index": 1, "value": "pos"},
            {"kind": "option-terminator", "index": 2},
            {"kind": "positional", "index": 3, "value": "--name"}
        ])
    );
    assert_eq!(actual["tokensPositionals"], json!(["pos", "--name"]));
    assert_eq!(actual["loose"], json!({"unknown": true, "name": "v", "level": "info", "positionals": ["p"]}));
    assert_eq!(actual["negative"], json!({"verbose": false, "level": "info"}));
    assert_eq!(
        actual["errors"],
        json!([
            ["ERR_PARSE_ARGS_UNKNOWN_OPTION", "Unknown option '--nope'"],
            [
                "ERR_PARSE_ARGS_UNKNOWN_OPTION",
                "Unknown option '--nope'. To specify a positional argument starting with a '-', place it at the end of the command after '--', as in '-- \"--nope\""
            ],
            ["ERR_PARSE_ARGS_INVALID_OPTION_VALUE", "Option '-n, --name <value>' argument missing"],
            ["ERR_PARSE_ARGS_INVALID_OPTION_VALUE", "Option '-v, --verbose' does not take an argument"],
            [
                "ERR_PARSE_ARGS_UNEXPECTED_POSITIONAL",
                "Unexpected argument 'extra'. This command does not take positional arguments"
            ],
            [
                "ERR_PARSE_ARGS_INVALID_OPTION_VALUE",
                "Option '--name' argument is ambiguous.\nDid you forget to specify the option argument for '--name'?\nTo specify an option argument starting with a dash use '--name=-XYZ'."
            ],
            ["ERR_INVALID_ARG_TYPE", "The \"args\" argument must be an instance of Array. Received type string ('x')"],
            [
                "ERR_INVALID_ARG_TYPE",
                "The \"options.a.type\" property must be ('string|boolean'). Received type string ('number')"
            ],
            ["ERR_INVALID_ARG_VALUE", "The property 'options.a.short' must be a single character. Received 'ab'"]
        ])
    );
}

#[tokio::test]
async fn util_system_errors_proxy_and_abort_controller() {
    let actual = run(r#"
const util = require("util");
const err = (fn) => { try { fn(); return "no error"; } catch (e) { return [e.code, e.message]; } };
const map = util.getSystemErrorMap();
const linux = process.platform === "linux";
return {
  names: linux ? [util.getSystemErrorName(-2), util.getSystemErrorName(-13), util.getSystemErrorName(-98)] : null,
  eof: util.getSystemErrorName(-4095), unknown: util.getSystemErrorName(-99999),
  enoent: linux ? map.get(-2) : null, isMap: map instanceof Map, eai: map.get(-3008),
  errors: [err(() => util.getSystemErrorName(2)), err(() => util.getSystemErrorName("x"))],
  proxy: [util.types.isProxy(new Proxy({}, {})), util.types.isProxy({}), util.types.isProxy(1)],
  controller: util.transferableAbortController() instanceof AbortController,
  signal: util.transferableAbortSignal(AbortSignal.abort()).aborted,
  signalError: err(() => util.transferableAbortSignal({})),
};
"#)
    .await;
    if cfg!(target_os = "linux") {
        assert_eq!(actual["names"], json!(["ENOENT", "EACCES", "EADDRINUSE"]));
        assert_eq!(actual["enoent"], json!(["ENOENT", "no such file or directory"]));
    }
    assert_eq!(actual["eof"], "EOF");
    assert_eq!(actual["unknown"], "Unknown system error -99999");
    assert_eq!(actual["isMap"], true);
    assert_eq!(actual["eai"], json!(["EAI_NONAME", "unknown node or service"]));
    assert_eq!(
        actual["errors"],
        json!([
            ["ERR_OUT_OF_RANGE", "The value of \"err\" is out of range. It must be a negative integer. Received 2"],
            ["ERR_INVALID_ARG_TYPE", "The \"err\" argument must be of type number. Received type string ('x')"]
        ])
    );
    assert_eq!(actual["proxy"], json!([true, false, false]));
    assert_eq!(actual["controller"], true);
    assert_eq!(actual["signal"], true);
    assert_eq!(actual["signalError"][0], "ERR_INVALID_ARG_TYPE");
}

#[tokio::test]
async fn path_matches_glob() {
    let actual = run(r#"
const path = require("path");
const cases = [["/a/b/c.js", "/a/**/*.js"], ["/a/c.js", "/a/**/*.js"], ["/a/.b/c.js", "/a/**/*.js"], ["/a/.b", "/a/*"],
  ["/a/.b", "/a/.*"], ["x.ts", "*.{js,ts}"], ["x.md", "*.{js,ts}"], ["f3", "f{1..3}"], ["ab", "a?"], ["abc", "a?"],
  ["b", "[a-c]"], ["d", "[!a-c]"], ["foo.min.js", "!(*.min).js"], ["foo.js", "!(*.min).js"], ["a/b", "a/**"], ["a", "a/**"],
  ["x/y.txt", "**/*.txt"], ["y.txt", "**/*.txt"], ["a1", "a+(1|2)"], ["dir/file", "dir/*/"]];
let error;
try { path.matchesGlob(1, "x"); } catch (e) { error = e.code; }
return { posix: cases.map(([p, g]) => path.posix.matchesGlob(p, g)), win: [path.win32.matchesGlob("a\\b\\c.js", "a/**/*.js"), path.win32.matchesGlob("a\\b", "a\\*")], error };
"#)
    .await;
    assert_eq!(
        actual["posix"],
        json!([
            true, true, false, false, true, true, false, true, true, false, true, true, false, true, true, true, true,
            true, true, false
        ])
    );
    assert_eq!(actual["win"], json!([true, true]));
    assert_eq!(actual["error"], "ERR_INVALID_ARG_TYPE");
}

#[tokio::test]
async fn perf_hooks_timerify_observer_histograms_and_utilization() {
    let actual = run(r#"
const { performance, PerformanceObserver, monitorEventLoopDelay, createHistogram } = require("perf_hooks");
const seen = [];
const observer = new PerformanceObserver((list) => seen.push(...list.getEntries().map((e) => [e.name, e.entryType, e.detail])));
observer.observe({ entryTypes: ["function", "mark"] });
const histogram = createHistogram();
const add = performance.timerify(function add(a, b) { return a + b; }, { histogram });
const sum = add(2, 3);
performance.mark("m1");
await new Promise((resolve) => setImmediate(resolve));
observer.disconnect();
histogram.record(5);
histogram.record(15n);
const err = (fn) => { try { fn(); return "no error"; } catch (e) { return e.code; } };
const elu1 = performance.eventLoopUtilization();
await new Promise((resolve) => setTimeout(resolve, 30));
const elu2 = performance.eventLoopUtilization(elu1);
const delay = monitorEventLoopDelay({ resolution: 5 });
const enabled = [delay.enable(), delay.enable()];
await new Promise((resolve) => setTimeout(resolve, 40));
const disabled = [delay.disable(), delay.disable()];
const empty = createHistogram();
return {
  sum, name: add.name, length: add.length, seen, count: histogram.count, max: histogram.max >= 15,
  min: histogram.min >= 1, p100: histogram.percentile(100) === histogram.max, mean: Number.isFinite(histogram.mean),
  errors: [err(() => histogram.record(0)), err(() => histogram.record("1")), err(() => histogram.percentile(0)), err(() => performance.timerify(1)), err(() => new PerformanceObserver(1))],
  empty: [empty.count, empty.min === 9223372036854776000, empty.max, Number.isNaN(empty.mean), [...empty.percentiles]],
  elu: [Object.keys(elu1), elu2.idle > 10, elu2.utilization >= 0 && elu2.utilization <= 1],
  // A late tick shortens the next interval (as in Node), so only the first full interval bounds the samples.
  delay: [enabled, disabled, delay.count > 0, delay.min > 0 && delay.max >= 5e6, delay.percentiles instanceof Map],
  types: PerformanceObserver.supportedEntryTypes,
};
"#)
    .await;
    assert_eq!(actual["sum"], 5);
    assert_eq!(actual["name"], "timerified add");
    assert_eq!(actual["length"], 2);
    assert_eq!(actual["seen"], json!([["add", "function", [2, 3]], ["m1", "mark", null]]));
    assert_eq!(actual["count"], 3);
    for key in ["max", "min", "p100", "mean"] {
        assert_eq!(actual[key], true, "{key}");
    }
    assert_eq!(
        actual["errors"],
        json!([
            "ERR_OUT_OF_RANGE",
            "ERR_INVALID_ARG_TYPE",
            "ERR_OUT_OF_RANGE",
            "ERR_INVALID_ARG_TYPE",
            "ERR_INVALID_ARG_TYPE"
        ])
    );
    assert_eq!(actual["empty"], json!([0, true, 0, true, [[100, 0]]]));
    assert_eq!(actual["elu"], json!([["idle", "active", "utilization"], true, true]));
    assert_eq!(actual["delay"], json!([[true, false], [true, false], true, true, true]));
    assert_eq!(actual["types"], json!(["function", "mark", "measure"]));
}

#[tokio::test]
async fn events_diagnostics_channel_and_stream_compose() {
    let actual = run(r#"
const { EventEmitterAsyncResource, EventEmitter } = require("events");
class Named extends EventEmitterAsyncResource {}
const resource = new Named();
const custom = new EventEmitterAsyncResource({ name: "custom" });
const got = [];
resource.on("x", (v) => got.push(v));
resource.emit("x", 1);
const dc = require("diagnostics_channel");
const log = [];
const tc = dc.tracingChannel("work");
const handlers = { start: (c) => log.push(["start", c.arg]), end: (c) => log.push(["end", c.result]), asyncStart: (c) => log.push(["asyncStart", c.result]), asyncEnd: () => log.push(["asyncEnd"]), error: (c) => log.push(["error", c.error.message]) };
tc.subscribe(handlers);
const syncResult = tc.traceSync((a) => a * 2, { arg: 21 }, null, 21);
try { tc.traceSync(() => { throw new Error("bad"); }, { arg: 0 }); } catch {}
const promiseResult = await tc.tracePromise(async () => "done", { arg: "p" });
await new Promise((resolve) => tc.traceCallback((cb) => setImmediate(() => cb(null, "cb")), -1, { arg: "c" }, null, () => resolve()));
const unsubscribed = tc.unsubscribe(handlers);
const store = { value: undefined, run(value, fn) { const prev = this.value; this.value = value; try { return fn(); } finally { this.value = prev; } } };
const ch = dc.channel("stores");
ch.bindStore(store, (data) => ({ wrapped: data }));
const inside = ch.runStores("msg", () => store.value);
const { compose, Readable, Transform, Writable } = require("stream");
const upper = new Transform({ transform(chunk, enc, cb) { cb(null, String(chunk).toUpperCase()); } });
const composed = compose(upper, async function* (source) { for await (const chunk of source) yield `[${chunk}]`; });
const out = [];
composed.on("data", (chunk) => out.push(String(chunk)));
const finished = new Promise((resolve) => composed.on("end", resolve));
composed.write("a");
composed.end("b");
await finished;
let error;
try { compose(); } catch (e) { error = e.code; }
return {
  ids: [resource.asyncResource instanceof require("async_hooks").AsyncResource, custom.asyncResource.type, typeof resource.asyncId, resource.asyncResource.eventEmitter === resource, resource instanceof EventEmitter],
  got, syncResult, promiseResult, log, unsubscribed, hasSubscribers: tc.hasSubscribers, inside, out, error,
};
"#)
    .await;
    assert_eq!(actual["ids"], json!([true, null, "number", true, true]));
    assert_eq!(actual["got"], json!([1]));
    assert_eq!(actual["syncResult"], 42);
    assert_eq!(actual["promiseResult"], "done");
    assert_eq!(
        actual["log"],
        json!([
            ["start", 21],
            ["end", 42],
            ["start", 0],
            ["error", "bad"],
            ["end", null],
            ["start", "p"],
            ["end", null],
            ["asyncStart", "done"],
            ["asyncEnd"],
            ["start", "c"],
            ["end", null],
            ["asyncStart", "cb"],
            ["asyncEnd"]
        ])
    );
    assert_eq!(actual["unsubscribed"], true);
    assert_eq!(actual["hasSubscribers"], false);
    assert_eq!(actual["inside"], json!({"wrapped": "msg"}));
    assert_eq!(actual["out"], json!(["[A]", "[B]"]));
    assert_eq!(actual["error"], "ERR_MISSING_ARGS");
}

#[tokio::test]
async fn buffer_transcode_blob_urls_and_call_tracker() {
    let actual = run(r#"
const { transcode, resolveObjectURL, Blob } = require("buffer");
const assert = require("assert");
const err = (fn) => { try { fn(); return "no error"; } catch (e) { return [e.code, e.message]; } };
const blob = new Blob(["hello"], { type: "text/plain" });
const id = URL.createObjectURL(blob);
const resolved = resolveObjectURL(id);
const text = await resolved.text();
URL.revokeObjectURL(id);
const tracker = new assert.CallTracker();
const tracked = tracker.calls(function work(x) { return x + 1; }, 2);
const value = tracked(1);
const report = tracker.report();
let verifyError;
try { tracker.verify(); } catch (e) { verifyError = [e.code, e.message]; }
tracked(2);
const calls = tracker.getCalls(tracked).map((call) => call.arguments);
tracker.verify();
return {
  transcode: [transcode(Buffer.from("héllo €"), "utf8", "latin1").toString("latin1"), transcode(Buffer.from("hé"), "utf8", "ucs2").toString("ucs2"), transcode(Buffer.from("hé"), "utf8", "ascii").toString(), [...transcode(Buffer.from("A", "ucs2"), "ucs2", "utf8")]],
  transcodeErrors: [err(() => transcode(Buffer.from("a"), "utf8", "hex")), err(() => transcode("a", "utf8", "ascii"))[0]],
  blob: [id.startsWith("blob:nodedata:"), resolved === blob, text, resolveObjectURL(id), resolveObjectURL("blob:nodedata:missing"), err(() => URL.createObjectURL("x"))[0], err(() => URL.revokeObjectURL())[0]],
  tracker: [value, report.length, report[0].message, report[0].actual, report[0].expected, report[0].operator, verifyError, calls, err(() => tracker.getCalls(() => {}))[0], err(() => tracker.calls(() => {}, 0))[0]],
};
"#)
    .await;
    assert_eq!(actual["transcode"], json!(["héllo ?", "hé", "h?", [65]]));
    assert_eq!(
        actual["transcodeErrors"],
        json!([
            ["U_ILLEGAL_ARGUMENT_ERROR", "Unable to transcode Buffer [U_ILLEGAL_ARGUMENT_ERROR]"],
            "ERR_INVALID_ARG_TYPE"
        ])
    );
    assert_eq!(actual["blob"], json!([true, true, "hello", null, null, "ERR_INVALID_ARG_TYPE", "ERR_MISSING_ARGS"]));
    assert_eq!(
        actual["tracker"],
        json!([
            2,
            1,
            "Expected the work function to be executed 2 time(s) but was executed 1 time(s).",
            1,
            2,
            "work",
            ["ERR_ASSERTION", "Expected the work function to be executed 2 time(s) but was executed 1 time(s)."],
            [[1], [2]],
            "ERR_INVALID_ARG_VALUE",
            "ERR_OUT_OF_RANGE"
        ])
    );
}

#[tokio::test]
async fn module_register_source_maps_and_require_extensions() {
    let (_dir, root) = tempdir();
    let (runtime, _capture) = runtime(&root);
    write(
        &root,
        "hooks.mjs",
        r#"
let prefix;
export function initialize(data) { prefix = data.prefix; }
export async function resolve(specifier, context, next) {
  if (specifier.startsWith("virtual:")) return { url: specifier, shortCircuit: true };
  return next(specifier, context);
}
export async function load(url, context, next) {
  if (url.startsWith("virtual:")) return { format: "module", source: `export default ${JSON.stringify(prefix + url.slice(8))};`, shortCircuit: true };
  const result = await next(url, context);
  if (url.endsWith("plain.mjs")) return { ...result, source: String(result.source).replace("original", "hooked") };
  return result;
}
"#,
    );
    write(&root, "plain.mjs", "export default 'original';\n");
    write(&root, "data.txt", "text body\n");
    write(
        &root,
        "mapped.js",
        "console.log(1);\n//# sourceMappingURL=data:application/json;base64,eyJ2ZXJzaW9uIjozLCJzb3VyY2VzIjpbImEudHMiXSwibmFtZXMiOltdLCJtYXBwaW5ncyI6IkFBQUEsQ0FBQyJ9\n",
    );
    write(&root, "typed.ts", "const n: number = 1;\nexport default n;\n");
    let actual = run_cjs(
        &runtime,
        &root,
        r#"
const Module = require("module");
const path = require("path");
const { pathToFileURL } = require("url");
Module.register("./hooks.mjs", pathToFileURL(__filename), { data: { prefix: "v-" } });
const virtual = (await import("virtual:thing")).default;
const plain = (await import("./plain.mjs")).default;
require.extensions[".txt"] = (module, filename) => { module.exports = require("fs").readFileSync(filename, "utf8").trim(); };
const txt = require("./data.txt");
const txtBare = require("./data");
const compiled = new Module("/virtual/x.js");
compiled._compile("module.exports = 40 + 2;", "/virtual/x.js");
const map = Module.findSourceMap(path.join(__dirname, "mapped.js"));
await import("./typed.ts");
const tsMap = Module.findSourceMap(path.join(__dirname, "typed.ts"));
return {
  virtual, plain, txt, txtBare, compiled: compiled.exports, sameHandler: typeof require.extensions[".js"],
  map: map && [map.payload.sources, map.findEntry(0, 0), map.findEntry(0, 1), map.findOrigin(1, 2), map.findEntry(5, 0).originalLine],
  tsMap: tsMap instanceof Module.SourceMap, missing: Module.findSourceMap(path.join(__dirname, "data.txt")),
};
"#,
    )
    .await;
    assert_eq!(actual["virtual"], "v-thing");
    assert_eq!(actual["plain"], "hooked");
    assert_eq!(actual["txt"], "text body");
    assert_eq!(actual["txtBare"], "text body");
    assert_eq!(actual["compiled"], 42);
    assert_eq!(actual["sameHandler"], "function");
    assert_eq!(
        actual["map"],
        json!([
            ["a.ts"],
            {"generatedLine": 0, "generatedColumn": 0, "originalSource": "a.ts", "originalLine": 0, "originalColumn": 0},
            {"generatedLine": 0, "generatedColumn": 1, "originalSource": "a.ts", "originalLine": 0, "originalColumn": 1},
            {"fileName": "a.ts", "lineNumber": 1, "columnNumber": 2},
            0
        ])
    );
    assert_eq!(actual["tsMap"], true);
    assert_eq!(actual.get("missing"), None);
}
