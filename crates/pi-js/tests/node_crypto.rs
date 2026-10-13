mod common;

use common::*;

// Expected values come from running the same scripts under Node 22.

const CRYPTO_SCRIPT: &str = r##"
const crypto = require("node:crypto");
const os = require("os");
const out = {};
out.hashes = ["md5", "sha1", "sha256", "sha384", "sha512"].map((a) => crypto.createHash(a).update("abc").digest("hex"));
out.b64 = crypto.createHash("sha256").update("a").update(Buffer.from("bc")).digest("base64");
const base = crypto.createHash("sha256").update("ab");
const copy = base.copy();
out.copy = [base.update("c").digest("hex") === copy.update("c").digest("hex")];
try { base.digest(); } catch (e) { out.finalized = e.code; }
out.hmac = [crypto.createHmac("sha256", "key").update("The quick brown fox jumps over the lazy dog").digest("hex"), crypto.createHmac("sha1", Buffer.from("k")).update("x").digest("base64")];
out.pbkdf2 = crypto.pbkdf2Sync("password", "salt", 2, 20, "sha256").toString("hex");
out.pbkdf2Async = await new Promise((r) => crypto.pbkdf2("pw", "s", 1, 8, "sha512", (e, k) => r(k.toString("hex"))));
const uuid = crypto.randomUUID();
out.uuid = [uuid.length, /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(uuid)];
out.random = [crypto.randomBytes(12).length, Buffer.isBuffer(crypto.randomBytes(1)), crypto.getRandomValues(new Uint32Array(4)).length];
const n = crypto.randomInt(5, 10); out.randomInt = n >= 5 && n < 10;
out.timing = [crypto.timingSafeEqual(Buffer.from("ab"), Buffer.from("ab")), crypto.timingSafeEqual(Buffer.from("ab"), Buffer.from("ac"))];
try { crypto.timingSafeEqual(Buffer.from("a"), Buffer.from("ab")); } catch (e) { out.timingErr = e.code; }
try { crypto.createHash("nope"); } catch (e) { out.badHash = e.message; }
out.oneShot = crypto.hash("sha1", "abc");
const streamed = await new Promise((resolve) => {
  const hash = crypto.createHash("md5");
  hash.on("readable", () => { const d = hash.read(); if (d) resolve(d.toString("hex")); });
  hash.write("ab"); hash.end("c");
});
out.streamed = streamed;
out.web = [typeof globalThis.crypto.randomUUID(), globalThis.crypto.getRandomValues(new Uint8Array(3)).length];
out.os = [typeof os.hostname(), os.homedir().length > 0, /[\\/]$/.test(os.tmpdir()), os.EOL === (process.platform === "win32" ? "\r\n" : "\n"), typeof os.totalmem(), os.cpus().length > 0, os.platform() === process.platform, typeof os.release(), os.loadavg().length, os.constants.signals.SIGTERM, os.constants.errno.ENOENT, typeof os.userInfo().username, os.endianness(), os.availableParallelism() > 0, os.arch() === process.arch];
return out;
"##;

const CRYPTO_EXPECTED: &str = r##"{"hashes":["900150983cd24fb0d6963f7d28e17f72","a9993e364706816aba3e25717850c26c9cd0d89d","ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad","cb00753f45a35e8bb5a03d699ac65007272c32ab0eded1631a8b605a43ff5bed8086072ba1e7cc2358baeca134c825a7","ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f"],"b64":"ungWv48Bz+pBQUDeXa4iI7ADYaOWF3qctBD/YfIAFa0=","copy":[true],"finalized":"ERR_CRYPTO_HASH_FINALIZED","hmac":["f7bc83f430538424b13298e6aa6fb143ef4d59a14946175997479dbc2d1a3cd8","ywATH87GAqj8hEE6KnAVTYwGoNI="],"pbkdf2":"ae4d0c95af6b46d32d0adff928f06dd02a303f8e","pbkdf2Async":"bf555f9834c1f2b2","uuid":[36,true],"random":[12,true,4],"randomInt":true,"timing":[true,false],"timingErr":"ERR_CRYPTO_TIMING_SAFE_EQUAL_LENGTH","badHash":"Digest method not supported","oneShot":"a9993e364706816aba3e25717850c26c9cd0d89d","streamed":"900150983cd24fb0d6963f7d28e17f72","web":["string",3],"os":["string",true,false,true,"number",true,true,"string",3,15,2,"string","LE",true,true]}"##;

#[tokio::test]
async fn crypto_and_os_match_node() {
    let (_dir, root) = tempdir();
    let (runtime, _capture) = runtime(&root);
    let actual = run_cjs(&runtime, &root, CRYPTO_SCRIPT).await;
    let expected: serde_json::Value = serde_json::from_str(CRYPTO_EXPECTED).unwrap();
    assert_eq!(actual, expected);
}
