mod common;

use common::*;

/// Expected values were produced by running the same script under Node 22.
const BUFFER_SCRIPT: &str = r#"
const { Buffer } = require("buffer");
const b = Buffer.from("héllo wörld");
const out = {};
out.len = b.length;
out.hex = b.toString("hex");
out.b64 = Buffer.from("hello").toString("base64");
out.b64url = Buffer.from([251, 255, 254]).toString("base64url");
out.fromB64 = Buffer.from("aGVsbG8=", "base64").toString();
out.latin1 = Buffer.from("ÿ", "latin1")[0];
out.utf16 = Buffer.from("hi", "utf16le").toJSON();
out.concat = Buffer.concat([Buffer.from("ab"), Buffer.from("cd")]).toString();
out.slice = b.slice(0, 5).toString();
out.isBuf = Buffer.isBuffer(b.subarray(1));
out.indexOf = [b.indexOf("l"), b.lastIndexOf("l"), b.includes("wör"), b.indexOf(Buffer.from("zz"))];
const n = Buffer.alloc(16);
n.writeUInt32BE(0xdeadbeef, 0); n.writeInt16LE(-2, 4); n.writeDoubleLE(1.5, 8);
out.nums = [n.readUInt32BE(0), n.readInt16LE(4), n.readDoubleLE(8), n.readUIntBE(0, 3), n.readIntLE(4, 2)];
out.big = Buffer.from([1,0,0,0,0,0,0,0]).readBigUInt64LE(0).toString();
out.fill = Buffer.alloc(5, "ab").toString();
out.compare = [Buffer.compare(Buffer.from("a"), Buffer.from("b")), Buffer.from("x").equals(Buffer.from("x"))];
out.write = (() => { const w = Buffer.alloc(4); const c = w.write("€€", 0); return [c, w.toJSON().data]; })();
out.byteLength = [Buffer.byteLength("€"), Buffer.byteLength("aGk=", "base64")];
out.inspect = require("util").inspect(Buffer.from("hi"));
out.empty = require("util").inspect(Buffer.alloc(0));
out.atob = [btoa("hi"), atob("aGk=")];
out.blob = await new Blob(["a", Buffer.from("b")], { type: "Text/Plain" }).text();
out.blobType = new Blob([], { type: "Text/Plain" }).type;
out.fromArr = Buffer.from([256, 1, -1]).toJSON().data;
out.u16 = Buffer.from(new Uint16Array([1, 256])).toJSON().data;
out.swap = Buffer.from([1,2,3,4]).swap16().toJSON().data;
out.enc = [Buffer.isEncoding("UTF-8"), Buffer.isEncoding("nope")];
out.copy = (() => { const t = Buffer.alloc(4); Buffer.from("xyz").copy(t, 1); return t.toString("hex"); })();
out.dec = new TextDecoder().decode(new Uint8Array([0xef,0xbb,0xbf,0x68,0x69]));
out.enc2 = Array.from(new TextEncoder().encode("é"));
out.encInto = new TextEncoder().encodeInto("a€b", new Uint8Array(3));
const td = new TextDecoder(); out.stream = td.decode(new Uint8Array([0xe2, 0x82]), { stream: true }) + td.decode(new Uint8Array([0xac]));
const { StringDecoder } = require("string_decoder"); const sd = new StringDecoder("utf8");
out.sd = [sd.write(Buffer.from([0xe2])), sd.write(Buffer.from([0x82, 0xac])), sd.end()];
const sd64 = new StringDecoder("base64"); out.sd64 = sd64.write(Buffer.from("ab")) + "|" + sd64.end();
return out;
"#;

const BUFFER_EXPECTED: &str = r#"{"len":13,"hex":"68c3a96c6c6f2077c3b6726c64","b64":"aGVsbG8=","b64url":"-__-","fromB64":"hello","latin1":255,"utf16":{"type":"Buffer","data":[104,0,105,0]},"concat":"abcd","slice":"héll","isBuf":true,"indexOf":[3,11,true,-1],"nums":[3735928559,-2,1.5,14593470,-2],"big":"1","fill":"ababa","compare":[-1,true],"write":[3,[226,130,172,0]],"byteLength":[3,2],"inspect":"<Buffer 68 69>","empty":"<Buffer >","atob":["aGk=","hi"],"blob":"ab","blobType":"text/plain","fromArr":[0,1,255],"u16":[1,0],"swap":[2,1,4,3],"enc":[true,false],"copy":"0078797a","dec":"hi","enc2":[195,169],"encInto":{"read":1,"written":1},"stream":"€","sd":["","€",""],"sd64":"|YWI="}"#;

#[tokio::test]
async fn buffer_and_text_encoding_match_node() {
    let (_dir, root) = tempdir();
    let (runtime, _capture) = runtime(&root);
    let actual = run_cjs(&runtime, &root, BUFFER_SCRIPT).await;
    let expected: serde_json::Value = serde_json::from_str(BUFFER_EXPECTED).unwrap();
    assert_eq!(actual, expected);
}
