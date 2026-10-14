//! node:v8: serialize bytes match Node's for the same values, the Serializer and Deserializer
//! classes, and the heap statistics shapes.

mod common;

use common::*;
use serde_json::json;

async fn run(body: &str) -> serde_json::Value {
    let (_dir, root) = tempdir();
    let (runtime, _capture) = runtime(&root);
    run_cjs(&runtime, &root, body).await
}

#[tokio::test]
async fn v8_serialize_matches_node_bytes() {
    let actual = run(r#"
const v8 = require("v8");
const shared = { s: 1 };
const sparse = [1, , 3]; 
const arr = [1, "two", 3.5]; arr.extra = true;
const err = new RangeError("bad"); err.stack = "STACK";
const errc = new Error("m", { cause: 5 }); errc.stack = "S";
const values = [undefined, null, true, false, 0, -0, 1, -1, 2147483647, 2147483648, -2147483649, 1.5, NaN, Infinity, 0n, 1n, -1n, 2n ** 64n, "", "hello", "héllo", "h€llo", "😀",
  { a: 1, b: "x", 1: 2, 4294967295: 3 }, [], arr, sparse, new Array(3), [shared, shared], new Map([[1, "a"], ["b", { c: 2 }]]), new Set([1, "x"]),
  new Date(0), /ab+c/gimsuy, Object(true), Object(false), Object(5), Object("str"), Object(3n), Buffer.from([1, 2, 3]), new Uint8Array([4, 5]), new Uint16Array([1, 2]),
  new Float64Array([1.5]), new DataView(new ArrayBuffer(2)), new ArrayBuffer(3), err, errc, { nested: { deep: [1, { x: null }] } }, new Uint8Array(new ArrayBuffer(8), 2, 3)];
return values.map((v) => v8.serialize(v).toString("hex"));
"#)
    .await;
    // From Node 24's v8.serialize for the same values.
    let expected = json!([
        "ff0f5f",
        "ff0f30",
        "ff0f54",
        "ff0f46",
        "ff0f4900",
        "ff0f4e0000000000000080",
        "ff0f4902",
        "ff0f4901",
        "ff0f49feffffff0f",
        "ff0f4e000000000000e041",
        "ff0f4e000020000000e0c1",
        "ff0f4e000000000000f83f",
        "ff0f4e000000000000f87f",
        "ff0f4e000000000000f07f",
        "ff0f5a00",
        "ff0f5a100100000000000000",
        "ff0f5a110100000000000000",
        "ff0f5a2000000000000000000100000000000000",
        "ff0f2200",
        "ff0f220568656c6c6f",
        "ff0f220568e96c6c6f",
        "ff0f630a6800ac206c006c006f00",
        "ff0f63043dd800de",
        "ff0f6f490249042201614902220162220178220a3432393439363732393549067b04",
        "ff0f4100240000",
        "ff0f41034902220374776f4e0000000000000c402205657874726154240103",
        "ff0f61034900490249044906400203",
        "ff0f6103400003",
        "ff0f41026f22017349027b015e01240002",
        "ff0f3b49022201612201626f22016349047b013a04",
        "ff0f2749022201782c02",
        "ff0f440000000000000000",
        "ff0f52220461622b633f",
        "ff0f79",
        "ff0f78",
        "ff0f6e0000000000001440",
        "ff0f732203737472",
        "ff0f7a100300000000000000",
        "ff0f5c0a03010203",
        "ff0f5c01020405",
        "ff0f5c040401000200",
        "ff0f5c0808000000000000f83f",
        "ff0f5c09020000",
        "ff0f4203000000",
        "ff0f72526d2203626164732205535441434b2e",
        "ff0f726d22016d7322015363490a2e",
        "ff0f6f22066e65737465646f220464656570410249026f220178307b012400027b017b01",
        "ff0f5c0103000000"
    ]);
    let (actual, expected) = (actual.as_array().unwrap(), expected.as_array().unwrap());
    assert_eq!(actual.len(), expected.len());
    for (index, (got, want)) in actual.iter().zip(expected).enumerate() {
        assert_eq!(got, want, "value {index}");
    }
}

#[tokio::test]
async fn v8_round_trips_and_reports_heap() {
    let actual = run(r#"
const v8 = require("v8");
const shared = { s: 1 };
const value = { a: [1, , 3], m: new Map([[1, shared]]), d: shared, set: new Set(["x"]), date: new Date(5), re: /x/gi, buf: Buffer.from("hi"), u16: new Uint16Array([1, 2]), big: 10n ** 30n, text: "h€llo", boxed: Object("w"), err: new RangeError("r", { cause: 1 }) };
const back = v8.deserialize(v8.serialize(value));
const s = new v8.Serializer();
s.writeHeader(); s.writeUint32(300); s.writeUint64(1, 2); s.writeDouble(1.25); s.writeRawBytes(Buffer.from([9, 8])); s.writeValue({ q: 1 });
const bytes = s.releaseBuffer();
const d = new v8.Deserializer(bytes);
d.readHeader();
const read = [d.getWireFormatVersion(), d.readUint32(), d.readUint64(), d.readDouble(), [...d.readRawBytes(2)], d.readValue()];
const errors = [() => 1, Symbol("q"), new WeakMap(), Promise.resolve()].map((v) => { try { v8.serialize(v); } catch (e) { return e.message; } });
let version;
try { v8.deserialize(Buffer.from([0xff, 0x20])); } catch (e) { version = e.message; }
const heap = v8.getHeapStatistics();
return {
  back: [1 in back.a, back.a.length, back.m.get(1) === back.d, [...back.set], back.date.getTime(), String(back.re), Buffer.isBuffer(back.buf), back.buf.toString(), [...back.u16], String(back.big), back.text, back.boxed instanceof String, back.err instanceof RangeError, back.err.message, back.err.cause],
  bytes: bytes.toString("hex"), read, errors, version,
  heap: [Object.keys(heap).length, heap.used_heap_size > 0, heap.heap_size_limit > heap.used_heap_size],
  spaces: v8.getHeapSpaceStatistics().map((space) => space.space_name).length,
  code: Object.keys(v8.getHeapCodeStatistics()),
  tag: Number.isInteger(v8.cachedDataVersionTag()) && v8.cachedDataVersionTag() === v8.cachedDataVersionTag(),
  oneByte: [v8.isStringOneByteRepresentation("abc\xff"), v8.isStringOneByteRepresentation("€")],
  flags: v8.setFlagsFromString("--expose-gc"),
};
"#)
    .await;
    assert_eq!(
        actual["back"],
        json!([
            false,
            3,
            true,
            ["x"],
            5,
            "/x/gi",
            true,
            "hi",
            [1, 2],
            "1000000000000000000000000000000",
            "h€llo",
            true,
            true,
            "r",
            1
        ])
    );
    assert_eq!(actual["bytes"], "ff0fac028280808010000000000000f43f09086f22017149027b01");
    assert_eq!(actual["read"], json!([15, 300, [1, 2], 1.25, [9, 8], {"q": 1}]));
    assert_eq!(
        actual["errors"],
        json!([
            "() => 1 could not be cloned.",
            "Symbol(q) could not be cloned.",
            "#<WeakMap> could not be cloned.",
            "#<Promise> could not be cloned."
        ])
    );
    assert_eq!(actual["version"], "Unable to deserialize cloned data due to invalid or unsupported version.");
    assert_eq!(actual["heap"], json!([14, true, true]));
    assert_eq!(actual["spaces"], 13);
    assert_eq!(
        actual["code"],
        json!([
            "code_and_metadata_size",
            "bytecode_and_metadata_size",
            "external_script_source_size",
            "cpu_profiler_metadata_size"
        ])
    );
    assert_eq!(actual["tag"], true);
    assert_eq!(actual["oneByte"], json!([true, false]));
    assert_eq!(actual["flags"], json!(null));
}
