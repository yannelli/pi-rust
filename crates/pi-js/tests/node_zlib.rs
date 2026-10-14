//! node:zlib: round trips, Node's compressed vectors, options, streams, and error codes.

mod common;

use common::*;
use serde_json::json;

async fn run(body: &str) -> serde_json::Value {
    let (_dir, root) = tempdir();
    let (runtime, _capture) = runtime(&root);
    run_cjs(&runtime, &root, body).await
}

#[tokio::test]
async fn zlib_decodes_node_vectors_and_round_trips() {
    let actual = run(r#"
const zlib = require("zlib");
const text = "hello world hello world";
// Compressed by Node 24 for the same text.
const vectors = {
  gunzip: "H4sIAAAAAAAAA8tIzcnJVyjPL8pJUchAsAE7zuLqFwAAAA==",
  inflate: "eJzLSM3JyVcozy/KSVHIQLABaecI2Q==",
  inflateRaw: "y0jNyclXKM8vyklRyECwAQ==",
  brotliDecompress: "GxYA+I2Ubt5EVYaWIGxvNQsytUAGVDs=",
  zstdDecompress: "KLUv/SAXlQAAYGhlbGxvIHdvcmxkIAEAN0oS",
  unzip: "H4sIAAAAAAAAA8tIzcnJVyjPL8pJUchAsAE7zuLqFwAAAA==",
};
const decoded = {};
for (const [name, b64] of Object.entries(vectors)) decoded[name] = zlib[name + "Sync"](Buffer.from(b64, "base64")).toString();
const pairs = [["gzip", "gunzip"], ["deflate", "inflate"], ["deflateRaw", "inflateRaw"], ["brotliCompress", "brotliDecompress"], ["zstdCompress", "zstdDecompress"], ["deflate", "unzip"]];
const big = Buffer.alloc(200000, "abcdefghij");
const round = pairs.map(([c, d]) => zlib[d + "Sync"](zlib[c + "Sync"](big, { chunkSize: 1024 })).equals(big));
const promisified = await require("util").promisify(zlib.gzip)(text);
const viaCallback = await new Promise((resolve) => zlib.brotliCompress(text, (err, out) => zlib.brotliDecompress(out, (err2, back) => resolve([err, err2, back.toString()]))));
const gzip = zlib.gzipSync(text);
const levels = [0, 1, 9].map((level) => zlib.inflateSync(zlib.deflateSync(big, { level, windowBits: 9, memLevel: 9, strategy: 1 })).equals(big));
const dict = Buffer.from("hello world");
const withDict = zlib.deflateSync(text, { dictionary: dict });
return {
  decoded, round, levels,
  promisified: zlib.gunzipSync(promisified).toString(), viaCallback,
  gzipHeader: gzip.subarray(0, 4).toString("hex"),
  zstdFrame: zlib.zstdCompressSync(text).toString("base64"),
  dict: zlib.inflateSync(withDict, { dictionary: dict }).toString(),
  multiMember: zlib.gunzipSync(Buffer.concat([zlib.gzipSync("a"), zlib.gzipSync("b")])).toString(),
  crc: [zlib.crc32("hello world"), zlib.crc32("world", zlib.crc32("hello ")), zlib.crc32(Buffer.alloc(0))],
  info: zlib.deflateSync("x", { info: true }).engine instanceof zlib.Deflate,
  constants: [zlib.constants.Z_BEST_COMPRESSION, zlib.constants.BROTLI_PARAM_QUALITY, zlib.constants.ZSTD_c_compressionLevel, zlib.Z_SYNC_FLUSH, zlib.codes.Z_DATA_ERROR, zlib.codes["-3"]],
  quality: zlib.brotliDecompressSync(zlib.brotliCompressSync(big, { params: { [zlib.constants.BROTLI_PARAM_QUALITY]: 4 } })).equals(big),
  zstdLevel: zlib.zstdDecompressSync(zlib.zstdCompressSync(big, { params: { [zlib.constants.ZSTD_c_compressionLevel]: 19 } })).equals(big),
};
"#)
    .await;
    let text = "hello world hello world";
    for name in ["gunzip", "inflate", "inflateRaw", "brotliDecompress", "zstdDecompress", "unzip"] {
        assert_eq!(actual["decoded"][name], text, "{name}");
    }
    assert_eq!(actual["round"], json!([true, true, true, true, true, true]));
    assert_eq!(actual["levels"], json!([true, true, true]));
    assert_eq!(actual["promisified"], text);
    assert_eq!(actual["viaCallback"], json!([null, null, text]));
    assert_eq!(actual["gzipHeader"], "1f8b0800");
    assert_eq!(actual["zstdFrame"], "KLUv/SAXlQAAYGhlbGxvIHdvcmxkIAEAN0oS");
    assert_eq!(actual["dict"], text);
    assert_eq!(actual["multiMember"], "ab");
    assert_eq!(actual["crc"], json!([222957957u32, 222957957u32, 0]));
    assert_eq!(actual["info"], true);
    assert_eq!(actual["constants"], json!([9, 1, 100, 2, -3, "Z_DATA_ERROR"]));
    assert_eq!(actual["quality"], true);
    assert_eq!(actual["zstdLevel"], true);
}

#[tokio::test]
async fn zlib_streams_flush_and_errors_follow_node() {
    let actual = run(r#"
const zlib = require("zlib");
const err = (fn) => { try { fn(); return "no error"; } catch (e) { return [e.name, e.code, e.errno, e.message]; } };
const gzip = zlib.createGzip();
const gunzip = zlib.createGunzip();
gzip.pipe(gunzip);
const chunks = [];
gunzip.on("data", (chunk) => chunks.push(chunk));
const ended = new Promise((resolve) => gunzip.on("end", resolve));
gzip.write("abc");
await new Promise((resolve) => gzip.flush(resolve));
gzip.end("def");
await ended;
const streamError = await new Promise((resolve) => {
  const inflate = zlib.createInflate();
  inflate.on("error", (e) => resolve([e.code, e.errno, e.message]));
  inflate.end(Buffer.from("not zlib data"));
});
const { pipeline } = require("stream/promises");
const { Readable, Writable } = require("stream");
const out = [];
await pipeline(Readable.from([Buffer.from("x".repeat(5000))]), zlib.createBrotliCompress(), zlib.createBrotliDecompress(), new Writable({ write(c, e, cb) { out.push(c); cb(); } }));
const gz = zlib.gzipSync("hello");
return {
  streamed: Buffer.concat(chunks).toString(), bytesWritten: gzip.bytesWritten, streamError,
  piped: Buffer.concat(out).length,
  errors: [
    err(() => zlib.inflateSync(Buffer.from("garbage"))),
    err(() => zlib.gunzipSync(gz.subarray(0, 10))),
    err(() => zlib.inflateSync(zlib.deflateSync("x", { dictionary: Buffer.from("d") }))),
    err(() => zlib.zstdDecompressSync(Buffer.from("garbage!"))),
    err(() => zlib.deflateSync("x", { level: 10 })),
    err(() => zlib.deflateSync("x", { chunkSize: 10 })),
    err(() => zlib.deflateSync("x", { windowBits: 16 })),
    err(() => zlib.deflateSync(42)),
    err(() => zlib.createBrotliCompress({ params: { 99: 1 } })),
    err(() => zlib.gunzipSync(zlib.gzipSync("x".repeat(100)), { maxOutputLength: 10 })),
  ],
  brotliError: err(() => zlib.brotliDecompressSync(Buffer.from("garbage!"))).slice(0, 2),
};
"#)
    .await;
    assert_eq!(actual["streamed"], "abcdef");
    assert_eq!(actual["bytesWritten"], 6);
    assert_eq!(actual["streamError"], json!(["Z_DATA_ERROR", -3, "incorrect header check"]));
    assert_eq!(actual["piped"], 5000);
    assert_eq!(
        actual["errors"],
        json!([
            ["Error", "Z_DATA_ERROR", -3, "incorrect header check"],
            ["Error", "Z_BUF_ERROR", -5, "unexpected end of file"],
            ["Error", "Z_NEED_DICT", 2, "Missing dictionary"],
            ["Error", "ZSTD_error_prefix_unknown", 10, "Unknown frame descriptor"],
            [
                "RangeError",
                "ERR_OUT_OF_RANGE",
                null,
                "The value of \"options.level\" is out of range. It must be >= -1 and <= 9. Received 10"
            ],
            [
                "RangeError",
                "ERR_OUT_OF_RANGE",
                null,
                "The value of \"options.chunkSize\" is out of range. It must be >= 64. Received 10"
            ],
            [
                "RangeError",
                "ERR_OUT_OF_RANGE",
                null,
                "The value of \"options.windowBits\" is out of range. It must be >= 8 and <= 15. Received 16"
            ],
            [
                "TypeError",
                "ERR_INVALID_ARG_TYPE",
                null,
                "The \"buffer\" argument must be of type string or an instance of Buffer, TypedArray, DataView, or ArrayBuffer. Received type number (42)"
            ],
            ["RangeError", "ERR_BROTLI_INVALID_PARAM", null, "99 is not a valid Brotli parameter"],
            ["RangeError", "ERR_BUFFER_TOO_LARGE", null, "Cannot create a Buffer larger than 10 bytes"]
        ])
    );
    assert_eq!(actual["brotliError"], json!(["Error", "ERR__ERROR_FORMAT_PADDING_2"]));
}
