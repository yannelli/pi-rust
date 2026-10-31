//! CompressionStream and DecompressionStream on QuickJS against Node 22.23 results: formats, the
//! constructor contract, vectors Node compressed, errors, cancel and abort, and backpressure.

mod common;

use common::*;
use serde_json::json;

async fn run(body: &str) -> serde_json::Value {
    let (_dir, root) = tempdir();
    let (runtime, _capture) = runtime(&root);
    run_cjs_within(&runtime, &root, &format!("{HELPERS}\n{body}"), 20).await
}

const HELPERS: &str = r#"
const zlib = require("zlib");
const web = require("node:stream/web");
const err = (e) => (e && typeof e === "object" ? { name: e.name, code: e.code, message: e.message, cause: e.cause && { code: e.cause.code, message: e.cause.message } } : String(e));
const within = (p, ms = 1000) => Promise.race([p, new Promise((r) => setTimeout(() => r("pending"), ms))]);
async function collect(readable) { const parts = []; for await (const c of readable) parts.push(Buffer.from(c)); return Buffer.concat(parts); }
async function run(format, Cls, input) {
  const s = new Cls(format); const w = s.writable.getWriter();
  const out = collect(s.readable); out.catch(() => {});
  try { for (const c of input) await w.write(c); await w.close(); } catch (e) { await out; throw e; }
  return out;
}
const attempt = async (f) => { try { return await within(f()); } catch (e) { return err(e); } };
const tryNew = (f) => { try { f(); return "ok"; } catch (e) { return err(e); } };
"#;

#[tokio::test]
async fn constructors_globals_and_node_vectors() {
    let actual = run(r#"
const cs = new CompressionStream("gzip");
const desc = Object.getOwnPropertyDescriptor(CompressionStream.prototype, "readable");
// Compressed by Node 22.23 CompressionStream from "hello world hello world".
const vectors = { gzip: "H4sIAAAAAAAAE8tIzcnJVyjPL8pJUchAsAE7zuLqFwAAAA==", deflate: "eJzLSM3JyVcozy/KSVHIQLABaecI2Q==",
  "deflate-raw": "y0jNyclXKM8vyklRyECwAQ==", brotli: "GxYA+I2Ubt5EVYaWIGxvNQsytUAGVDs=" };
const decoded = {};
for (const [format, b64] of Object.entries(vectors)) decoded[format] = (await run(format, DecompressionStream, [Buffer.from(b64, "base64")])).toString();
const big = Buffer.alloc(300000, "xyz0123456");
const chunks = [big.subarray(0, 1000), big.subarray(1000, 150000), big.subarray(150000)];
const round = {};
for (const format of Object.keys(vectors)) {
  const packed = await run(format, CompressionStream, chunks);
  round[format] = (await run(format, DecompressionStream, [packed.subarray(0, 7), packed.subarray(7)])).equals(big);
}
const gz = await run("gzip", CompressionStream, [Buffer.from("interop")]);
return {
  globals: [typeof CompressionStream, CompressionStream === web.CompressionStream, DecompressionStream === web.DecompressionStream,
    require("stream/web").DecompressionStream === DecompressionStream],
  shape: { tag: Object.prototype.toString.call(cs), dtag: Object.prototype.toString.call(new DecompressionStream("deflate")),
    own: Object.getOwnPropertyNames(cs), proto: Object.getOwnPropertyNames(CompressionStream.prototype),
    readable: cs.readable instanceof ReadableStream, writable: cs.writable instanceof WritableStream, length: CompressionStream.length,
    getter: typeof desc.get, enumerable: desc.enumerable, desired: cs.writable.getWriter().desiredSize },
  invalid: { zip: tryNew(() => new CompressionStream("zip")), none: tryNew(() => new CompressionStream()), br: tryNew(() => new DecompressionStream("br")),
    call: tryNew(() => CompressionStream("gzip")).name, obj: tryNew(() => new CompressionStream({ toString() { return "gzip"; } })),
    upper: tryNew(() => new CompressionStream("GZIP")).code, foreign: tryNew(() => desc.get.call({})).name },
  decoded, round,
  interop: [gz.subarray(0, 3).toString("hex"), zlib.gunzipSync(gz).toString()],
};
"#)
    .await;
    let invalid = |format: &str, class: &str| {
        let message = format!(
            "Failed to construct '{class}': 1st argument value '{format}' is not a valid enum value of type CompressionFormat."
        );
        json!({"name": "TypeError", "code": "ERR_INVALID_ARG_VALUE", "message": message})
    };
    let text = "hello world hello world";
    assert_eq!(actual["globals"], json!(["function", true, true, true]));
    let proto = json!(["constructor", "readable", "writable"]);
    let shape = json!({"tag": "[object CompressionStream]", "dtag": "[object DecompressionStream]", "own": [], "proto": proto,
        "readable": true, "writable": true, "length": 1, "getter": "function", "enumerable": true, "desired": 65536});
    assert_eq!(actual["shape"], shape);
    let expected = json!({"zip": invalid("zip", "CompressionStream"), "none": invalid("undefined", "CompressionStream"),
        "br": invalid("br", "DecompressionStream"), "call": "TypeError", "obj": "ok", "upper": "ERR_INVALID_ARG_VALUE", "foreign": "TypeError"});
    assert_eq!(actual["invalid"], expected);
    assert_eq!(actual["decoded"], json!({"gzip": text, "deflate": text, "deflate-raw": text, "brotli": text}));
    assert_eq!(actual["round"], json!({"gzip": true, "deflate": true, "deflate-raw": true, "brotli": true}));
    assert_eq!(actual["interop"], json!(["1f8b08", "interop"]));
}

#[tokio::test]
async fn errors_cancel_abort_and_backpressure_follow_node() {
    let actual = run(r#"
const gz = zlib.gzipSync(Buffer.alloc(100000, "abcdefghij"));
const trailing = Buffer.from("trailing");
const types = {};
for (const [name, chunk] of [["string", "abc"], ["dataview", new DataView(new ArrayBuffer(2))], ["u16", new Uint16Array([1, 2])], ["arraybuffer", new ArrayBuffer(3)], ["number", 5], ["null", null], ["array", [1, 2]]]) {
  types[name] = await attempt(async () => zlib.gunzipSync(await run("gzip", CompressionStream, [chunk])).toString("hex"));
}
const big = zlib.gzipSync(Buffer.alloc(300000, "xyz0123456"));
const incremental = await (async () => { const d = new DecompressionStream("gzip"); const w = d.writable.getWriter(); const r = d.readable.getReader();
  w.write(big.subarray(0, big.length >> 1)); const first = await within(r.read()); return first === "pending" ? first : { done: first.done, partial: first.value.length > 0, buffer: Buffer.isBuffer(first.value), tag: Object.prototype.toString.call(first.value) }; })();
const afterCancel = await (async () => { const d = new CompressionStream("deflate"); await d.readable.getReader().cancel(new Error("stop")); return attempt(() => d.writable.getWriter().write(Buffer.from("x"))); })();
const afterAbort = await (async () => { const d = new CompressionStream("deflate"); const w = d.writable.getWriter(); const r = d.readable.getReader();
  const reason = new Error("aborted here"); await w.abort(reason); try { await within(r.read()); return "read ok"; } catch (e) { return { same: e === reason, ...err(e) }; } })();
const backpressure = await (async () => { const d = new CompressionStream("gzip"); const w = d.writable.getWriter(); const sizes = [w.desiredSize];
  const p = w.write(Buffer.alloc(65536)); sizes.push(w.desiredSize); sizes.push(await within(p, 300)); sizes.push(w.desiredSize); return sizes; })();
const empty = await (async () => { const d = new CompressionStream("gzip"); const out = collect(d.readable); await d.writable.getWriter().close(); return zlib.gunzipSync(await out).length; })();
return {
  truncated: await attempt(() => run("gzip", DecompressionStream, [gz.subarray(0, gz.length - 10)])),
  garbage: await attempt(() => run("gzip", DecompressionStream, [Buffer.from("not gzip at all")])),
  trailingGzip: await attempt(() => run("gzip", DecompressionStream, [Buffer.concat([zlib.gzipSync("a"), trailing])])),
  trailingDeflate: await attempt(() => run("deflate", DecompressionStream, [zlib.deflateSync("a"), trailing])),
  trailingRaw: (await run("deflate-raw", DecompressionStream, [Buffer.concat([zlib.deflateRawSync("a"), trailing])])).toString(),
  members: (await run("gzip", DecompressionStream, [zlib.gzipSync("a"), zlib.gzipSync("b")])).toString(),
  types, incremental, afterCancel, afterAbort, backpressure, empty,
};
"#)
    .await;
    let zlib_error = |code: &str, message: &str| json!({"name": "TypeError", "code": code, "message": "", "cause": {"code": code, "message": message}});
    assert_eq!(actual["truncated"], zlib_error("Z_BUF_ERROR", "unexpected end of file"));
    assert_eq!(actual["garbage"], zlib_error("Z_DATA_ERROR", "incorrect header check"));
    assert_eq!(actual["trailingGzip"], zlib_error("Z_DATA_ERROR", "incorrect header check"));
    let junk = "Trailing junk found after the end of the compressed stream";
    let junk = json!({"name": "TypeError", "code": "ERR_TRAILING_JUNK_AFTER_STREAM_END", "message": junk});
    assert_eq!(actual["trailingDeflate"], junk);
    assert_eq!((&actual["trailingRaw"], &actual["members"]), (&json!("a"), &json!("ab")));
    let types = &actual["types"];
    assert_eq!(
        (&types["string"], &types["dataview"], &types["u16"], &types["arraybuffer"]),
        (&json!("616263"), &json!("0000"), &json!("01000200"), &json!("000000"))
    );
    assert_eq!(
        (&types["number"]["code"], &types["array"]["code"], &types["null"]["code"]),
        (&json!("ERR_INVALID_ARG_TYPE"), &json!("ERR_INVALID_ARG_TYPE"), &json!("ERR_STREAM_NULL_VALUES"))
    );
    assert_eq!(
        actual["incremental"],
        json!({"done": false, "partial": true, "buffer": false, "tag": "[object Uint8Array]"})
    );
    assert_eq!(actual["afterCancel"], json!({"name": "Error", "message": "stop"}));
    assert_eq!(actual["afterAbort"], json!({"same": true, "name": "Error", "message": "aborted here"}));
    assert_eq!(actual["backpressure"], json!([65536, 65535, null, 65536]));
    assert_eq!(actual["empty"], json!(0));
}

#[tokio::test]
async fn zlib_trailing_data_follows_node_with_and_without_the_option() {
    let actual = run(r#"
const t = (f) => { try { return f().toString(); } catch (e) { return `${e.code}: ${e.message}`; } };
const a = zlib.gzipSync("a");
const junk = Buffer.concat([zlib.deflateSync("a"), Buffer.from("x")]);
const streamed = await new Promise((resolve, reject) => { const g = zlib.createGunzip(); const parts = [];
  g.on("data", (c) => parts.push(c)).on("end", () => resolve(Buffer.concat(parts).toString())).on("error", reject);
  g.write(a); g.write(zlib.gzipSync("b")); g.end(); });
return {
  gunzipJunk: t(() => zlib.gunzipSync(Buffer.concat([a, Buffer.from("trailing")]))),
  gunzipZeros: t(() => zlib.gunzipSync(Buffer.concat([a, Buffer.alloc(4)]))),
  gunzipMagicByte: t(() => zlib.gunzipSync(Buffer.concat([a, Buffer.from([0x1f])]))),
  unzipJunk: t(() => zlib.unzipSync(Buffer.concat([a, Buffer.from("trailing")]))),
  inflateJunk: t(() => zlib.inflateSync(Buffer.concat([zlib.deflateSync("a"), Buffer.from("trailing")]))),
  inflateRejectsSync: t(() => zlib.inflateSync(junk, { rejectGarbageAfterEnd: true })),
  inflateRejectsAsync: await new Promise((resolve) => zlib.inflate(junk, { rejectGarbageAfterEnd: true }, (e, out) => resolve(e ? e.code : out.toString()))),
  inflateRejectsStream: await new Promise((resolve) => { const s = zlib.createInflate({ rejectGarbageAfterEnd: true }); const parts = [];
    s.on("data", (c) => parts.push(c)).on("error", (e) => resolve(`${Buffer.concat(parts)} ${e.name} ${e.code}`)).on("end", () => resolve("end")); s.end(junk); }),
  inflateClean: t(() => zlib.inflateSync(zlib.deflateSync("a"), { rejectGarbageAfterEnd: true })),
  members: t(() => zlib.gunzipSync(Buffer.concat([a, zlib.gzipSync("b")]))),
  streamed,
};
"#)
    .await;
    let expected = json!({
        "gunzipJunk": "Z_DATA_ERROR: incorrect header check", "gunzipZeros": "a",
        "gunzipMagicByte": "Z_BUF_ERROR: unexpected end of file", "unzipJunk": "Z_DATA_ERROR: incorrect header check",
        "inflateJunk": "a", "inflateRejectsSync": "a", "inflateRejectsAsync": "ERR_TRAILING_JUNK_AFTER_STREAM_END",
        "inflateRejectsStream": "a TypeError ERR_TRAILING_JUNK_AFTER_STREAM_END",
        "inflateClean": "a", "members": "ab", "streamed": "ab",
    });
    assert_eq!(actual, expected);
}

#[tokio::test]
async fn stream_chunks_may_be_any_array_buffer_view_as_in_node_22() {
    let actual = run(r#"
const { Writable, Readable } = require("stream");
const seen = [];
const w = new Writable({ write(chunk, _encoding, callback) { seen.push([Buffer.isBuffer(chunk), chunk.toString("hex")]); callback(); } });
w.write(new Uint16Array([1, 2]));
w.write(new DataView(new ArrayBuffer(2)));
const number = (() => { try { w.write(5); return "ok"; } catch (e) { return [e.code, e.message.split(". Received")[0]]; } })();
const r = new Readable({ read() {} });
const pushed = [];
r.on("data", (d) => pushed.push(d.toString("hex")));
r.push(new Uint16Array([3]));
await new Promise((resolve) => setImmediate(resolve));
return { seen, number, pushed };
"#)
    .await;
    let message = "The \"chunk\" argument must be of type string or an instance of Buffer, TypedArray, or DataView";
    let expected = json!({"seen": [[true, "01000200"], [true, "0000"]], "number": ["ERR_INVALID_ARG_TYPE", message], "pushed": ["0300"]});
    assert_eq!(actual, expected);
}
