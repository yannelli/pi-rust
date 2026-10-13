mod common;

use common::*;

// Expected values come from running the same scripts under Node 22.

const URL_SCRIPT: &str = r##"
const url = require("node:url");
const qs = require("querystring");
const out = {};
const u = new URL("https://user:pa ss@Example.COM:8080/a b/../c?x=1&y=two words#frag");
out.parts = [u.href, u.origin, u.protocol, u.username, u.password, u.host, u.hostname, u.port, u.pathname, u.search, u.hash];
u.searchParams.append("z", "ä&=");
u.searchParams.delete("x");
out.afterParams = u.href;
u.pathname = "/new path";
u.port = "443";
u.hash = "";
out.afterSet = u.href;
out.rel = new URL("../x?q", "http://h/a/b/c").href;
out.canParse = [URL.canParse("nope"), URL.canParse("/p", "http://h")];
try { new URL("not a url"); } catch (e) { out.err = [e.name, e.code]; }
const p = new URLSearchParams("a=1&b=2&a=3&c=%20x+y");
out.params = [p.get("a"), p.getAll("a"), p.has("b"), p.get("c"), p.size, [...p.keys()]];
p.sort(); p.set("a", "9");
out.paramsStr = p.toString();
out.paramsObj = new URLSearchParams({ k: "v w", "é": "1" }).toString();
out.fileUrl = url.pathToFileURL("/tmp/a b#c.txt").href;
out.filePath = url.fileURLToPath("file:///tmp/a%20b.txt");
const legacy = url.parse("http://user:pw@host.com:8080/p/a/t/h?query=string&a=1#hash", true);
out.legacy = [legacy.protocol, legacy.slashes, legacy.auth, legacy.host, legacy.port, legacy.hostname, legacy.hash, legacy.search, legacy.query, legacy.pathname, legacy.path, legacy.href];
out.legacyRel = [url.parse("/a/b?c=d").pathname, url.parse("/a/b?c=d").query];
out.format = url.format({ protocol: "https", hostname: "ex.com", pathname: "/x", query: { a: "1 2", b: ["x", "y"] } });
out.resolve = [url.resolve("/one/two/three", "four"), url.resolve("http://example.com/", "/one"), url.resolve("http://example.com/one", "/two")];
out.qs = [qs.stringify({ a: [1, 2], b: "x y", c: "" }), qs.parse("a=1&a=2&b=%20&c")];
out.qsEscape = qs.escape("a b&c/é");
out.domain = url.domainToASCII("español.com");
return out;
"##;

const URL_EXPECTED: &str = r##"{"parts":["https://user:pa%20ss@example.com:8080/c?x=1&y=two%20words#frag","https://example.com:8080","https:","user","pa%20ss","example.com:8080","example.com","8080","/c","?x=1&y=two%20words","#frag"],"afterParams":"https://user:pa%20ss@example.com:8080/c?y=two+words&z=%C3%A4%26%3D#frag","afterSet":"https://user:pa%20ss@example.com/new%20path?y=two+words&z=%C3%A4%26%3D","rel":"http://h/a/x?q","canParse":[false,true],"err":["TypeError","ERR_INVALID_URL"],"params":["1",["1","3"],true," x y",4,["a","b","a","c"]],"paramsStr":"a=9&b=2&c=+x+y","paramsObj":"k=v+w&%C3%A9=1","fileUrl":"file:///tmp/a%20b%23c.txt","filePath":"/tmp/a b.txt","legacy":["http:",true,"user:pw","host.com:8080","8080","host.com","#hash","?query=string&a=1",{"query":"string","a":"1"},"/p/a/t/h","/p/a/t/h?query=string&a=1","http://user:pw@host.com:8080/p/a/t/h?query=string&a=1#hash"],"legacyRel":["/a/b","c=d"],"format":"https://ex.com/x?a=1%202&b=x&b=y","resolve":["/one/two/four","http://example.com/one","http://example.com/two"],"qs":["a=1&a=2&b=x%20y&c=",{"a":["1","2"],"b":" ","c":""}],"qsEscape":"a%20b%26c%2F%C3%A9","domain":"xn--espaol-zwa.com"}"##;

#[tokio::test]
async fn url_and_querystring_match_node() {
    let (_dir, root) = tempdir();
    let (runtime, _capture) = runtime(&root);
    let actual = run_cjs(&runtime, &root, URL_SCRIPT).await;
    let expected: serde_json::Value = serde_json::from_str(URL_EXPECTED).unwrap();
    assert_eq!(actual, expected);
}

const WEB_SCRIPT: &str = r##"
const out = {};
const target = new EventTarget();
const seen = [];
target.addEventListener("ping", (e) => seen.push(["a", e.type, e.detail]));
target.addEventListener("ping", { handleEvent: (e) => seen.push(["b", e.type]) }, { once: true });
target.dispatchEvent(new CustomEvent("ping", { detail: 1 }));
target.dispatchEvent(new CustomEvent("ping", { detail: 2 }));
out.events = seen;
const ev = new Event("x", { cancelable: true });
ev.preventDefault();
out.cancel = [ev.defaultPrevented, target.dispatchEvent(new Event("none"))];
const controller = new AbortController();
const reasons = [];
controller.signal.addEventListener("abort", () => reasons.push(controller.signal.reason.name));
controller.signal.onabort = () => reasons.push("onabort");
controller.abort();
controller.abort();
out.abort = [controller.signal.aborted, reasons, AbortSignal.abort("why").reason];
try { AbortSignal.abort().throwIfAborted(); } catch (e) { out.thrown = [e.name, e.message, e.code, e instanceof DOMException, e instanceof Error]; }
const timeout = AbortSignal.timeout(5);
await new Promise((r) => setTimeout(r, 30));
out.timeout = [timeout.aborted, timeout.reason.name];
const any = AbortSignal.any([new AbortController().signal, AbortSignal.abort("first")]);
out.any = [any.aborted, any.reason];
const dom = new DOMException("msg", "NotFoundError");
out.dom = [dom.name, dom.message, dom.code, DOMException.NOT_FOUND_ERR, Object.prototype.toString.call(dom)];
const original = { d: new Date(0), m: new Map([[1, { x: [1, 2] }]]), s: new Set(["a"]), r: /x/gi, u: new Uint8Array([1, 2]), n: null, nested: { deep: true } };
original.self = original;
const copy = structuredClone(original);
out.clone = [copy !== original, copy.self === copy, copy.d instanceof Date, copy.d.getTime(), copy.m.get(1).x, [...copy.s], copy.r.flags, Array.from(copy.u), copy.nested.deep];
try { structuredClone({ f() {} }); } catch (e) { out.cloneErr = e.name; }
const err = structuredClone(new RangeError("bad"));
out.cloneError = [err instanceof RangeError, err.message];
const { EventEmitter, once } = require("events");
const emitter = new EventEmitter();
setTimeout(() => emitter.emit("ready", 1, 2), 1);
out.once = await once(emitter, "ready");
return out;
"##;

const WEB_EXPECTED: &str = r##"{"events":[["a","ping",1],["b","ping"],["a","ping",2]],"cancel":[true,true],"abort":[true,["AbortError","onabort"],"why"],"thrown":["AbortError","This operation was aborted",20,true,true],"timeout":[true,"TimeoutError"],"any":[true,"first"],"dom":["NotFoundError","msg",8,8,"[object DOMException]"],"clone":[true,true,true,0,[1,2],["a"],"gi",[1,2],true],"cloneErr":"DataCloneError","cloneError":[true,"bad"],"once":[1,2]}"##;

#[tokio::test]
async fn web_globals_match_node() {
    let (_dir, root) = tempdir();
    let (runtime, _capture) = runtime(&root);
    let actual = run_cjs(&runtime, &root, WEB_SCRIPT).await;
    let expected: serde_json::Value = serde_json::from_str(WEB_EXPECTED).unwrap();
    assert_eq!(actual, expected);
}

const STREAMS_SCRIPT: &str = r##"
const { Readable, Writable, Transform, PassThrough, Duplex, pipeline, finished } = require("stream");
const { pipeline: pipelineAsync } = require("stream/promises");
const out = {};
const log = [];
const readable = new Readable({ read() {} });
readable.on("data", (chunk) => log.push(["data", chunk.toString()]));
readable.on("end", () => log.push(["end"]));
readable.on("close", () => log.push(["close"]));
readable.push("a");
readable.push(Buffer.from("b"));
readable.push(null);
await new Promise((r) => readable.on("close", r));
out.flowing = log;
const chunks = [];
const writable = new Writable({
  highWaterMark: 4,
  write(chunk, encoding, callback) { chunks.push(chunk.toString()); setTimeout(callback, 1); },
});
out.writeReturns = [writable.write("ab"), writable.write("cdef")];
await new Promise((r) => writable.once("drain", r));
writable.end("!", () => chunks.push("end-callback"));
await new Promise((r) => writable.on("finish", r));
out.written = [chunks, writable.writableFinished, writable.writableEnded];
const upper = new Transform({ transform(chunk, enc, cb) { cb(null, chunk.toString().toUpperCase()); }, flush(cb) { this.push("|FLUSH"); cb(); } });
let collected = "";
await new Promise((resolve, reject) => {
  pipeline(Readable.from(["x", "y", "z"]), upper, new Writable({ write(c, e, cb) { collected += c; cb(); } }), (err) => (err ? reject(err) : resolve()));
});
out.pipeline = collected;
const objects = [];
for await (const item of Readable.from([1, 2, 3])) objects.push(item);
out.iterated = objects;
const text = [];
const enc = new PassThrough();
enc.setEncoding("utf8");
enc.on("data", (c) => text.push(typeof c));
enc.write(Buffer.from([0xe2, 0x82]));
enc.end(Buffer.from([0xac]));
await new Promise((r) => enc.on("end", r));
out.encoded = text;
let failure;
try {
  await pipelineAsync(Readable.from(["a"]), new Transform({ transform(c, e, cb) { cb(new Error("boom")); } }), new PassThrough());
} catch (e) { failure = e.message; }
out.pipelineError = failure;
const generated = [];
await pipelineAsync(async function* () { yield "p"; yield "q"; }, async function* (source) { for await (const c of source) yield c + "!"; }, async function (source) { for await (const c of source) generated.push(String(c)); });
out.generators = generated;
const paused = new Readable({ read() {} });
paused.push("12345");
paused.push(null);
const reads = [];
paused.on("readable", () => { let c; while ((c = paused.read(2)) !== null) reads.push(c.toString()); });
await new Promise((r) => paused.on("end", r));
out.readable = reads;
const errored = new Readable({ read() { this.destroy(new Error("bad read")); } });
const finishedResult = new Promise((r) => finished(errored, (err) => r(err && err.message)));
errored.resume();
out.finished = await finishedResult;
const duplex = new Duplex({ read() {}, write(c, e, cb) { this.push(String(c).repeat(2)); cb(); }, final(cb) { this.push(null); cb(); } });
const dup = [];
duplex.on("data", (c) => dup.push(String(c)));
duplex.end("ab");
await new Promise((r) => duplex.on("end", r));
out.duplex = dup;
const piped = [];
const source = Readable.from(["1", "2"]);
source.pipe(new Writable({ objectMode: true, write(c, e, cb) { piped.push(c); cb(); } })).on("finish", () => piped.push("finish"));
await new Promise((r) => setTimeout(r, 20));
out.piped = piped;
return out;
"##;

const STREAMS_EXPECTED: &str = r##"{"flowing":[["data","a"],["data","b"],["end"],["close"]],"writeReturns":[true,false],"written":[["ab","cdef","!","end-callback"],true,true],"pipeline":"XYZ|FLUSH","iterated":[1,2,3],"encoded":["string"],"pipelineError":"boom","generators":["p!","q!"],"readable":["12","34","5"],"finished":"bad read","duplex":["abab"],"piped":["1","2","finish"]}"##;

#[tokio::test]
async fn streams_match_node() {
    let (_dir, root) = tempdir();
    let (runtime, _capture) = runtime(&root);
    let actual = run_cjs(&runtime, &root, STREAMS_SCRIPT).await;
    let expected: serde_json::Value = serde_json::from_str(STREAMS_EXPECTED).unwrap();
    assert_eq!(actual, expected);
}
