use crate::common;

use common::*;

// Expected values come from running the same scripts under Node 22.23.5. KEYS holds PKCS#8 keys
// (base64 DER) that Node generated. `sha` compares long outputs by a SHA-256 prefix.

const KEYS: &str = r##"
const KEYS = {
  rsa: "MIICdgIBADANBgkqhkiG9w0BAQEFAASCAmAwggJcAgEAAoGBAMg4iFeiF50NtKdnXCVW/mMgbb5kfMgtqTGmke+aLE94ql8S" +
    "t3lMZU5DClB8DEphvFwplMVWPZae69yBHx7JjNYh4FndOguZdOYZOR+Lmzs4zb2029P+8lnHISC34E2qvqfeiFsi737JmNp/" +
    "2nG/cNJNI1PUrPyxiWNS3HbGHsSjAgMBAAECgYAX1fMVS6y3+7u3+iwUHekm7+NguSHJyvXLfWL+PUJtK5ZMvzOscxXUXVLL" +
    "BhmIeV4tMp4NaBjo5354v75usdvcJDDlgmqaCoWsF0J73NNVwnBeTy1pt8Ijh09vuPLmRZXUky7NM20UwHIo0N40CWhI6yP5" +
    "xGCzO5JbuetEKwtNwQJBAO1yjk3lDPXfARoQWuVb7A7pPQKBobtV+9nj3dJovTltwMZyERys2y0gVbZdHH1MUAUpIr3btQaK" +
    "0KFhedAK1ZMCQQDX3V26PyB21tek+QXFcfOw4Js5SOhmAKoA1EtZZ5PW1VO8CWoKpdtoRQcmDUhop0mI+0cPx8b4UCq8Rp3+" +
    "Kr6xAkBbqiGtiIMcV97BBRdR+86BYyEeWpTpESSM1R7JH44NIYWOFnLssv5ZyWXNZNyeslsh4uN6j9quj+BCrLKt0cDpAkBZ" +
    "+sY0+ldX8hoVhE/pi6Re2GEbsc5dpm02x0dZTxmK9YVf3jS7l4MrpYpjTCogpYq7dLKcW1QO8au+q1j0N3EhAkEA4QEZrsIS" +
    "PH6V8f9ONtx/k81ASzL69TVS96I6DK3Y8reAUWF+RSfcUJBv/hGQk+rBWnyCxLuqm7H5eLiVmYUt8Q==",
  p256: "MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgyetOGWcCiO/nlUQabhIsxy678dsSYvCIoHzDvDrrCTKhRANC" +
    "AAQMUqKYb8mLlTVqaVOebVxOHAalaCOGDGNdbttS96Ty2vYb4PAhI3/P3DLUkl4IofnyvJzdz3ut9uIttbE/SfCf",
  p256b: "MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgpsnwPERMb2VfQPrlpuXuyRflaifEpzEfHPr+gRls4lmhRANC" +
    "AAQjCK3oEG8cAke5yW36C2kkovI+SEZxQqGhOKPFUwXu1EK52rZ2u34/ENvd9T+OsH7bia3tdRrJ1uciVQj1BMhg",
  p384: "MIG2AgEAMBAGByqGSM49AgEGBSuBBAAiBIGeMIGbAgEBBDBGw/IaiZpGXceSOvbBIvgC2EVhHfOwZqroHnbpXxTu4YbyJJoW" +
    "mYVLMKxBkgMgqmahZANiAAQOwN6sBaFKVxcLzWssvE3kXQFi0EzyVmAT1sYclhyu0zKagh05oPca93Z0Uw9sVmZaXWzB8gGt" +
    "IX6txEmzYuUIzuar7pJp0P9XcYCZkj3XleezyK/8B0TewBYEk5oUUaM=",
  p521: "MIHuAgEAMBAGByqGSM49AgEGBSuBBAAjBIHWMIHTAgEBBEIAUd2uR+/9gIacKmzqY/PJFVq8th4h8pY1wzW72PIGNdO3pwlW" +
    "JRAbRlvK8PSZomdWurwlnPlakHQuVfaR5Imghi+hgYkDgYYABAFydZtD8OyGpIB7ngEAYQru7Q26gxGUcFSitJdXJeTTfxW/" +
    "TkWzLV+MGB/IBJT4Sra8AiHsfpBNMqjS8VlQhUBCfgFYGYYYDiSsrDR48w/r2KxG5Va2wnWjl+oJ740W/hq1pho8Nye7jmYk" +
    "aMb5/s9sv3sEXjkozbHanOePhdD+U86jfA==",
  ed25519: "MC4CAQAwBQYDK2VwBCIEINOGLKAv+nrd/snweOYx2ijwGFeRUpbLtkNtwaTg3CDT",
  x25519: "MC4CAQAwBQYDK2VuBCIEIJD3ZnJQM3/qe1G2Z8hCFNGpRA1Nc9Y4ecSZvDk7sgFo",
  x25519b: "MC4CAQAwBQYDK2VuBCIEIKBLa1wfu7VImq7hwVN/qSlI1Y4OxJL0S3vIoEZDdPJ7",
  ed448: "MEcCAQAwBQYDK2VxBDsEOWhAemSiz4oTFpxHYxQdg9AsykUosysE2W7Pj4pouxIGSgCfKsYqX7PZ76ygytJRB7FhAXJ5skdMtg==",
  x448: "MEYCAQAwBQYDK2VvBDoEONRRq0/POZ+/+U1xk0JoSE0oeqw61RXsFvR7kQl56SoJqbm/VB8JTtmrnu9wlzd3HQhXEHu1VDWy",
  x448b: "MEYCAQAwBQYDK2VvBDoEOHRqYzgbVfndelQyA8FNxOJ9rnh1UnTWtJpmsLng4g7T7BZLiEiUO0PKH005hr/xtOU3wx5aFI3Q",
};
"##;

const PRELUDE: &str = r##"
const crypto = require("node:crypto");
const { subtle } = globalThis.crypto;
const hex = (b) => Buffer.from(b).toString("hex");
const sha = (b) => crypto.createHash("sha256").update(typeof b === "string" ? b : Buffer.from(b)).digest("hex").slice(0, 24);
const der = (name) => Buffer.from(KEYS[name], "base64");
const short = (jwk) => Object.fromEntries(Object.entries(jwk).map(([k, v]) => [k, typeof v === "string" && v.length > 48 ? sha(v) : v]));
const out = {};
"##;

pub(crate) async fn run(script: &str) -> serde_json::Value {
    let (_dir, root) = tempdir();
    let (runtime, _capture) = runtime(&root);
    run_cjs(&runtime, &root, &format!("{KEYS}{PRELUDE}{script}")).await
}

const SECRET_SCRIPT: &str = r##"
const data = Buffer.from("The quick brown fox jumps over the lazy dog");
const key16 = Buffer.from("000102030405060708090a0b0c0d0e0f", "hex");
const key32 = Buffer.alloc(32, 7);
const iv16 = Buffer.from("f0f1f2f3f4f5f6f7f8f9fafbfcfdfeff", "hex");
out.digest = [];
for (const h of ["SHA-1", "SHA-256", "SHA-384", "SHA-512"]) out.digest.push(sha(await subtle.digest(h, data)));
out.digestView = hex(await subtle.digest({ name: "sha-256" }, new DataView(data.buffer, data.byteOffset, 3)));
out.hmac = [];
for (const h of ["SHA-1", "SHA-256", "SHA-384", "SHA-512"]) {
  const k = await subtle.importKey("raw", key32, { name: "HMAC", hash: h }, true, ["sign", "verify"]);
  const sig = await subtle.sign("HMAC", k, data);
  out.hmac.push([sha(sig), await subtle.verify("HMAC", k, sig, data), await subtle.verify("HMAC", k, sig.slice(1), data), k.algorithm.length]);
}
out.hmacFull = hex(await subtle.sign("HMAC", await subtle.importKey("raw", key16, { name: "HMAC", hash: "SHA-256" }, false, ["sign"]), data));
const hkdfKey = await subtle.importKey("raw", Buffer.alloc(22, 0x0b), "HKDF", false, ["deriveBits", "deriveKey"]);
const salt = Buffer.from("000102030405060708090a0b0c", "hex");
out.hkdf = hex(await subtle.deriveBits({ name: "HKDF", hash: "SHA-256", salt, info: Buffer.from("f0f1f2f3f4f5f6f7f8f9", "hex") }, hkdfKey, 336));
out.hkdfZero = (await subtle.deriveBits({ name: "HKDF", hash: "SHA-1", salt: new Uint8Array(), info: new Uint8Array() }, hkdfKey, 0)).byteLength;
const pbKey = await subtle.importKey("raw", Buffer.from("password"), { name: "PBKDF2" }, false, ["deriveBits", "deriveKey"]);
out.pbkdf2 = hex(await subtle.deriveBits({ name: "PBKDF2", hash: "SHA-256", salt: Buffer.from("salt"), iterations: 2 }, pbKey, 256));
const derived = await subtle.deriveKey({ name: "PBKDF2", hash: "SHA-512", salt: Buffer.from("salt"), iterations: 3 }, pbKey, { name: "AES-GCM", length: 256 }, true, ["encrypt"]);
out.derivedKey = [derived.algorithm, hex(await subtle.exportKey("raw", derived))];
const hkdfParams = { name: "HKDF", hash: "SHA-384", salt: new Uint8Array(4), info: new Uint8Array(2) };
const derivedHmac = await subtle.deriveKey(hkdfParams, hkdfKey, { name: "HMAC", hash: "SHA-1" }, true, ["sign"]);
out.derivedHmac = [derivedHmac.algorithm, await subtle.exportKey("jwk", derivedHmac)];
const cbc = await subtle.importKey("raw", key16, "AES-CBC", true, ["encrypt", "decrypt"]);
const cbcCt = await subtle.encrypt({ name: "AES-CBC", iv: iv16 }, cbc, data);
out.cbc = [hex(cbcCt), Buffer.from(await subtle.decrypt({ name: "AES-CBC", iv: iv16 }, cbc, cbcCt)).toString()];
// AES-CTR counters that wrap within `length` bits restart at zero.
const ctr = await subtle.importKey("raw", key32, "AES-CTR", true, ["encrypt", "decrypt"]);
out.ctr = [];
for (const [counter, length] of [[iv16, 128], [iv16, 64], ["000000000000000000000000fffffffe", 3], ["0000000000000000000000000000000f", 4]]) {
  const ct = await subtle.encrypt({ name: "AES-CTR", counter: Buffer.from(counter, "hex"), length }, ctr, data);
  out.ctr.push(hex(ct));
}
const gcm = await subtle.importKey("raw", key16, "AES-GCM", true, ["encrypt", "decrypt"]);
const gcmIv = Buffer.from("cafebabefacedbaddecaf888", "hex");
out.gcm = [];
for (const params of [{}, { tagLength: 96, additionalData: Buffer.from("aad") }, { tagLength: 32 }]) {
  const ct = await subtle.encrypt({ name: "AES-GCM", iv: gcmIv, ...params }, gcm, data);
  out.gcm.push([hex(ct), Buffer.from(await subtle.decrypt({ name: "AES-GCM", iv: gcmIv, ...params }, gcm, ct)).toString()]);
}
const gcm24 = await subtle.importKey("raw", Buffer.alloc(24, 3), "AES-GCM", true, ["encrypt"]);
out.gcm192 = hex(await subtle.encrypt({ name: "AES-GCM", iv: Buffer.alloc(16, 1) }, gcm24, Buffer.from("abc")));
// AES-KW: the RFC 3394 4.2 vector, raw and JWK wraps, and unwrap.
const kek = Buffer.from("000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f", "hex");
const kw = await subtle.importKey("raw", kek, "AES-KW", true, ["wrapKey", "unwrapKey"]);
const rfcKey = await subtle.importKey("raw", Buffer.from("00112233445566778899aabbccddeeff0001020304050607", "hex"), "AES-KW", true, ["wrapKey"]);
out.kwRfc = hex(await subtle.wrapKey("raw", rfcKey, kw, "AES-KW"));
const toWrap = await subtle.importKey("raw", Buffer.from("00112233445566778899aabbccddeeff", "hex"), "AES-CBC", true, ["encrypt"]);
const wrapped = await subtle.wrapKey("raw", toWrap, kw, "AES-KW");
out.kw = [hex(wrapped), hex(await subtle.exportKey("raw", await subtle.unwrapKey("raw", wrapped, kw, "AES-KW", "AES-CBC", true, ["encrypt"])))];
const hmacKey = await subtle.importKey("raw", key32, { name: "HMAC", hash: "SHA-256" }, true, ["sign"]);
const wrappedJwk = await subtle.wrapKey("jwk", hmacKey, kw, { name: "AES-KW" });
const back = await subtle.unwrapKey("jwk", wrappedJwk, kw, "AES-KW", { name: "HMAC", hash: "SHA-256" }, false, ["sign"]);
out.kwJwk = [hex(wrappedJwk), back.extractable, back.algorithm];
const wrapGcm = await subtle.importKey("raw", key16, "AES-GCM", false, ["wrapKey", "unwrapKey"]);
const wg = await subtle.wrapKey("jwk", toWrap, wrapGcm, { name: "AES-GCM", iv: gcmIv });
out.wrapGcm = [sha(wg), (await subtle.unwrapKey("jwk", wg, wrapGcm, { name: "AES-GCM", iv: gcmIv }, "AES-CBC", true, ["encrypt"])).algorithm];
const jwk = { kty: "oct", k: key32.toString("base64url"), alg: "A256CTR", ext: true, key_ops: ["encrypt", "decrypt", "foo"] };
const aesJwk = await subtle.importKey("jwk", jwk, "AES-CTR", true, ["encrypt"]);
out.aesJwk = [aesJwk.algorithm, aesJwk.usages, await subtle.exportKey("jwk", aesJwk)];
const gen = await subtle.generateKey({ name: "AES-GCM", length: 192 }, true, ["encrypt", "encrypt", "decrypt"]);
out.genAes = [gen.algorithm, gen.usages, (await subtle.exportKey("raw", gen)).byteLength];
const genH = await subtle.generateKey({ name: "HMAC", hash: "SHA-384" }, false, ["verify", "sign", "verify"]);
const genH2 = await subtle.generateKey({ name: "HMAC", hash: { name: "SHA-1" }, length: 72 }, true, ["sign"]);
out.genHmac = [genH.algorithm, genH.usages, genH.extractable, genH2.algorithm, (await subtle.exportKey("raw", genH2)).byteLength];
return out;
"##;

const SECRET_EXPECTED: &str = r##"{
  "digest": ["01e41a6756edb81f4f439290","6d37795021e544d82b41850e","0743eb48e2913c5d5d963ec1","372efde43fa80632b3a582d8"],
  "digestView": "b344d80e24a3679999fa964450b34bc24d1578a35509f934c1418b0a20d21a67",
  "hmac": [["d3179404c7a16ff5904b34ab",true,false,256],["3b117490c818ae1238158dcf",true,false,256],["2aec0c65460c561b9acb60eb",true,false,256],["0944dbf6db39caa23357383d",true,false,256]],
  "hmacFull": "19cef782d548b76d9e5e1b5c057dd9c1f2817c1df3a626e4c5dfffc88e6e4304",
  "hkdf": "3cb25f25faacd57a90434f64d0362f2a2d2d0a90cf1a5a4c5db02d56ecc4c5bf34007208d5b887185865",
  "hkdfZero": 0,
  "pbkdf2": "ae4d0c95af6b46d32d0adff928f06dd02a303f8ef3c251dfd6e2d85a95474c43",
  "derivedKey": [{"name":"AES-GCM","length":256},"b6b07cb2cebf4ad84468391a543824fccffe0e0769dbe6bddf10a65673c4b648"],
  "derivedHmac": [{"name":"HMAC","hash":{"name":"SHA-1"},"length":512},{"key_ops":["sign"],"ext":true,"alg":"HS1","kty":"oct","k":"6S3EAYShL4sTBjzFgI1B-aFS7sKru9WBFHueOMxp5NM-MfhmgjjucdgHB7mWNXOS20iU5rvJZe0psIGm-ATSHQ"}],
  "cbc": ["639dd4c509b902e13dd2ee7500cf3af9be0e128e87ffefd09f0b8132b71d1d5750d0ced90c1f47388114f14250d31b88","The quick brown fox jumps over the lazy dog"],
  "ctr": ["a5ee523a1a0b476d1d049d53b7bde9bbbe13a561bd6d5e266e3c9463b37c35b5c775a9e7e014ee26732893","a5ee523a1a0b476d1d049d53b7bde9bbbe13a561bd6d5e266e3c9463b37c35b5c775a9e7e014ee26732893","f63c64fa8ad7a854ca8ec726679be2ca7e7e0b9c472b8019a58c502c5fa1cd3cd340d94df6506d8f56faad","7532fd6127a2b827ccc1c65f442339056c2296a78d198da40f4e73a9a21f00d736be7115379b4ede70f8a9"],
  "gcm": [["dd11a296f482e862c130abfa328dcec7e0644004319eddd542c9790d1355cda5f24cb96c936e645ca920d7a14b8c00985cb9d68a5bc81811a78c58","The quick brown fox jumps over the lazy dog"],["dd11a296f482e862c130abfa328dcec7e0644004319eddd542c9790d1355cda5f24cb96c936e645ca920d78d8b698c4e355c81d88b20f6","The quick brown fox jumps over the lazy dog"],["dd11a296f482e862c130abfa328dcec7e0644004319eddd542c9790d1355cda5f24cb96c936e645ca920d7a14b8c00","The quick brown fox jumps over the lazy dog"]],
  "gcm192": "fa35ddccacbc8a9be670e1c10bc04d0b4f2ecf",
  "kwRfc": "a8f9bc1612c68b3ff6e6f4fbe30e71e4769c8b80a32cb8958cd5d17d6b254da1",
  "kw": ["64e8c3f9ce0f5ba263e9777905818a2a93c8191e7d6e8ae7","00112233445566778899aabbccddeeff"],
  "kwJwk": ["4ee9550ad880edf983cb31a4b538959820fea8fa76ec478fb167e657c780f3d1fbaecbfeea6b29bf67cff4a4af5229c9d100e587629363951fd15d3e74b38d1605159a659fc03aa9a1f3fb99b1d16e806482306fb57bac5a6643fbc08276382f2076ce33d11fe85f9f202e6c3f58e324ac5d761c51e347d7",false,{"name":"HMAC","hash":{"name":"SHA-256"},"length":256}],
  "wrapGcm": ["cc85e7837069ce63e4077cce",{"name":"AES-CBC","length":128}],
  "aesJwk": [{"name":"AES-CTR","length":256},["encrypt"],{"key_ops":["encrypt"],"ext":true,"alg":"A256CTR","kty":"oct","k":"BwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwc"}],
  "genAes": [{"name":"AES-GCM","length":192},["encrypt","decrypt"],24],
  "genHmac": [{"name":"HMAC","length":1024,"hash":{"name":"SHA-384"}},["verify","sign"],false,{"name":"HMAC","length":72,"hash":{"name":"SHA-1"}},9]
}"##;

#[tokio::test]
async fn secret_key_algorithms_match_node() {
    let expected: serde_json::Value = serde_json::from_str(SECRET_EXPECTED).unwrap();
    assert_same_keys(&run(SECRET_SCRIPT).await, &expected);
}

const PAIR_SCRIPT: &str = r##"
const data = Buffer.from("signed payload");
const rsaKo = crypto.createPrivateKey({ key: der("rsa"), format: "der", type: "pkcs8" });
const rsaSpki = crypto.createPublicKey(rsaKo).export({ format: "der", type: "spki" });
const rsaPriv = await subtle.importKey("pkcs8", der("rsa"), { name: "RSASSA-PKCS1-v1_5", hash: "SHA-256" }, true, ["sign"]);
const rsaJwk = await subtle.exportKey("jwk", rsaPriv);
const rsaPub = await subtle.importKey("jwk", { kty: "RSA", n: rsaJwk.n, e: rsaJwk.e }, { name: "RSASSA-PKCS1-v1_5", hash: "SHA-256" }, true, ["verify"]);
out.rsa = [rsaPriv.algorithm, rsaPub.type, rsaPub.usages, short(rsaJwk), short(await subtle.exportKey("jwk", rsaPub))];
out.rsaExport = [sha(await subtle.exportKey("spki", rsaPub)) === sha(rsaSpki), sha(await subtle.exportKey("pkcs8", rsaPriv)) === sha(der("rsa"))];
const sig = await subtle.sign("RSASSA-PKCS1-v1_5", rsaPriv, data);
out.rsaSig = [hex(sig), await subtle.verify({ name: "RSASSA-PKCS1-v1_5" }, rsaPub, sig, data), await subtle.verify("RSASSA-PKCS1-v1_5", rsaPub, sig, Buffer.from("other")), await subtle.verify("RSASSA-PKCS1-v1_5", rsaPub, sig.slice(2), data)];
out.rsaHashes = [];
for (const h of ["SHA-1", "SHA-384", "SHA-512"]) {
  const k = await subtle.importKey("pkcs8", der("rsa"), { name: "RSASSA-PKCS1-v1_5", hash: h }, false, ["sign"]);
  out.rsaHashes.push(sha(await subtle.sign("RSASSA-PKCS1-v1_5", k, data)));
}
const pssJwk = await subtle.exportKey("jwk", await subtle.importKey("spki", rsaSpki, { name: "RSA-PSS", hash: "SHA-384" }, true, ["verify"]));
out.rsaAlgs = [pssJwk.alg, pssJwk.key_ops];
for (const [hash, alg] of [["SHA-1", "RSA-OAEP"], ["SHA-256", "RSA-OAEP-256"]]) {
  const oaep = await subtle.importKey("jwk", { kty: "RSA", n: rsaJwk.n, e: rsaJwk.e, alg }, { name: "RSA-OAEP", hash }, true, ["encrypt", "wrapKey"]);
  out.rsaAlgs.push((await subtle.exportKey("jwk", oaep)).alg, oaep.usages);
}
out.ec = [];
for (const [name, curve] of [["p256", "P-256"], ["p384", "P-384"], ["p521", "P-521"]]) {
  const priv = await subtle.importKey("pkcs8", der(name), { name: "ECDSA", namedCurve: curve }, true, ["sign"]);
  const jwk = await subtle.exportKey("jwk", priv);
  const pub = await subtle.importKey("jwk", { kty: "EC", crv: curve, x: jwk.x, y: jwk.y }, { name: "ECDSA", namedCurve: curve }, true, ["verify"]);
  const raw = Buffer.from(await subtle.exportKey("raw", pub));
  const nodeCurve = { "P-256": "prime256v1", "P-384": "secp384r1", "P-521": "secp521r1" }[curve];
  const compressed = crypto.ECDH.convertKey(raw, nodeCurve, undefined, undefined, "compressed");
  const fromCompressed = await subtle.importKey("raw", compressed, { name: "ECDSA", namedCurve: curve }, true, ["verify"]);
  out.ec.push([priv.algorithm, Object.keys(jwk), short(jwk), sha(raw), sha(await subtle.exportKey("spki", pub)),
    sha(await subtle.exportKey("pkcs8", priv)) === sha(der(name)), raw.equals(Buffer.from(await subtle.exportKey("raw", fromCompressed)))]);
}
const ecdhA = await subtle.importKey("pkcs8", der("p256"), { name: "ECDH", namedCurve: "P-256" }, true, ["deriveBits", "deriveKey"]);
const ecdhBJwk = await subtle.exportKey("jwk", await subtle.importKey("pkcs8", der("p256b"), { name: "ECDH", namedCurve: "P-256" }, true, ["deriveBits"]));
const ecdhB = await subtle.importKey("jwk", { kty: "EC", crv: "P-256", x: ecdhBJwk.x, y: ecdhBJwk.y }, { name: "ECDH", namedCurve: "P-256" }, true, []);
const ecdhKey = await subtle.deriveKey({ name: "ECDH", public: ecdhB }, ecdhA, { name: "AES-KW", length: 128 }, true, ["wrapKey"]);
out.ecdh = [hex(await subtle.deriveBits({ name: "ECDH", public: ecdhB }, ecdhA)), hex(await subtle.deriveBits({ name: "ECDH", public: ecdhB }, ecdhA, 100)),
  ecdhB.usages, ecdhKey.algorithm, hex(await subtle.exportKey("raw", ecdhKey))];
for (const [name, alg, other] of [["x25519", "X25519", "x25519b"], ["x448", "X448", "x448b"]]) {
  const a = await subtle.importKey("pkcs8", der(name), alg, true, ["deriveBits"]);
  const bJwk = await subtle.exportKey("jwk", await subtle.importKey("pkcs8", der(other), { name: alg }, true, ["deriveBits"]));
  const b = await subtle.importKey("raw", Buffer.from(bJwk.x, "base64url"), alg, true, []);
  out[alg] = [hex(await subtle.deriveBits({ name: alg, public: b }, a, null)), hex(await subtle.deriveBits({ name: alg, public: b }, a, 13)),
    await subtle.exportKey("jwk", a), sha(await subtle.exportKey("spki", b)), a.algorithm, b.usages];
}
for (const [name, alg] of [["ed25519", "Ed25519"], ["ed448", "Ed448"]]) {
  const priv = await subtle.importKey("pkcs8", der(name), alg, true, ["sign"]);
  const jwk = await subtle.exportKey("jwk", priv);
  const pub = await subtle.importKey("jwk", { kty: "OKP", crv: alg, x: jwk.x, alg: "EdDSA" }, alg, true, ["verify"]);
  const s = await subtle.sign(alg, priv, data);
  const again = await subtle.sign(alg, await subtle.importKey("jwk", jwk, alg, false, ["sign"]), data);
  out[alg] = [hex(s), await subtle.verify(alg, pub, s, data), await subtle.verify({ name: alg }, pub, s, Buffer.from("x")), jwk,
    hex(await subtle.exportKey("raw", pub)), sha(await subtle.exportKey("spki", pub)), pub.algorithm, hex(again) === hex(s)];
}
return out;
"##;

const PAIR_EXPECTED: &str = r##"{
  "rsa": [{"name":"RSASSA-PKCS1-v1_5","modulusLength":1024,"publicExponent":{"0":1,"1":0,"2":1},"hash":{"name":"SHA-256"}},"public",["verify"],{"key_ops":["sign"],"ext":true,"alg":"RS256","kty":"RSA","n":"c2d60c1693b96160b25cebcb","e":"AQAB","d":"074e3a177a8225fb27fc5a23","p":"aabfd531ac263ad87efc597f","q":"2236a4f709609f4bcae45066","dp":"b46b572c88e3f3762ecbf2be","dq":"9a3c13848e0803738c472eb3","qi":"390b6d8005dd7b74b66e51a3"},{"key_ops":["verify"],"ext":true,"alg":"RS256","kty":"RSA","n":"c2d60c1693b96160b25cebcb","e":"AQAB"}],
  "rsaExport": [true,true],
  "rsaSig": ["af5829316601d6eebe6698960b76a3e151b8cc0befba8074d4d9c7829837425f79bc484cf5625e28258639ce2c9346b8c2d6465baeb3a5bf8c68ba03c0093d04e59982d1f65a9be1388035925d4a61899b11be2f18f0f5407bb9fb968515acc89c666302ff97e5fe26785868a81898c808f482c057988afa2fe7cb86acf7a9cc",true,false,false],
  "rsaHashes": ["9c0f24a2d92d41f286f2c3c0","c1ad3d081b9b6f63e66a1286","d893faba41624b759eb9ee53"],
  "rsaAlgs": ["PS384",["verify"],"RSA-OAEP",["encrypt","wrapKey"],"RSA-OAEP-256",["encrypt","wrapKey"]],
  "ec": [[{"name":"ECDSA","namedCurve":"P-256"},["key_ops","ext","kty","x","y","crv","d"],{"key_ops":["sign"],"ext":true,"kty":"EC","x":"DFKimG_Ji5U1amlTnm1cThwGpWgjhgxjXW7bUvek8to","y":"9hvg8CEjf8_cMtSSXgih-fK8nN3Pe6324i21sT9J8J8","crv":"P-256","d":"yetOGWcCiO_nlUQabhIsxy678dsSYvCIoHzDvDrrCTI"},"62ed7c454f5028d2bb586621","dc2e2f387affd80db662b718",true,true],[{"name":"ECDSA","namedCurve":"P-384"},["key_ops","ext","kty","x","y","crv","d"],{"key_ops":["sign"],"ext":true,"kty":"EC","x":"aa5b73b5d37cdf2ab9575555","y":"ce8c44235a988ca36e6f0808","crv":"P-384","d":"5b2ec9ce296d0a35c3dede6c"},"94cc142e7c565fdf23a09b41","8b26df623548cfc71684e58c",true,true],[{"name":"ECDSA","namedCurve":"P-521"},["key_ops","ext","kty","x","y","crv","d"],{"key_ops":["sign"],"ext":true,"kty":"EC","x":"32db38b588c319c731a9ed94","y":"542691cafc77b3a9d54ab3c9","crv":"P-521","d":"b0503f0248de061ec0d8484f"},"f02089d6821c4ab858cbc83e","9c30c1436131d9499a529de1",true,true]],
  "ecdh": ["54a77d54e709d61f40cabf2c260132bce5fcd060587f7058cd1a8507e2e4400d","54a77d54e709d61f40cabf2c20",[],{"name":"AES-KW","length":128},"54a77d54e709d61f40cabf2c260132bc"],
  "X25519": ["5ced9683a60fbb033df18faf1835837959c2d947ac70f5ef323a10e181f77003","5ce8",{"key_ops":["deriveBits"],"ext":true,"crv":"X25519","d":"kPdmclAzf-p7UbZnyEIU0alEDU1z1jh5xJm8OTuyAWg","x":"uOmw1llKVStAKSlQ6DDIhH35UTdun1TRSMGiwh5qdiQ","kty":"OKP"},"7b7a7175446bac7dd8012b49",{"name":"X25519"},[]],
  "X448": ["9f4b50ed50d83775d85b6d42cb6258f2d97e18da2ea80f5a3aaa9419858a8df41807deb10993645f5593c9fe47d606afd93573616fd67fd9","9f48",{"key_ops":["deriveBits"],"ext":true,"crv":"X448","d":"1FGrT885n7_5TXGTQmhITSh6rDrVFewW9HuRCXnpKgmpub9UHwlO2aue73CXN3cdCFcQe7VUNbI","x":"FaSKShFKWU18o9IwFC1X5Af7_cPrHVUOGY8O4q1lvpsC01sbLn3HHhdmrUiOLflpKvCkYGth-Dk","kty":"OKP"},"bc7cf359cd33d292ddef8374",{"name":"X448"},[]],
  "Ed25519": ["b54c2eca4ea2a99507b48427e17c5311c8aa79c063b0e617b31d6bfedf8582b25a6801edd67ab910c16d0ab61a82351c79d78794497d7d0dbea6e21a4c471003",true,false,{"key_ops":["sign"],"ext":true,"alg":"Ed25519","crv":"Ed25519","d":"04YsoC_6et3-yfB45jHaKPAYV5FSlsu2Q23BpODcINM","x":"b-HTfB2zMymOX4u2C4UCFSjirDgpxffmL9x1LUx8FMk","kty":"OKP"},"6fe1d37c1db333298e5f8bb60b85021528e2ac3829c5f7e62fdc752d4c7c14c9","15a8b64fc294b43e4467d796",{"name":"Ed25519"},true],
  "Ed448": ["c37729f3c54fd8eacfe833e188f3de860497837611dac80916f7cbf8fd79b9deff63e142951fa97a12e88c2552d45336fc84d47c01e532ce808c9101dcc168393687fe0009bb013b440d8d5ebc49443a40bb31eff4b12c4d421db51e1a8c06c6f3ada1b7af5bf08a3ece05277b5a4e2d0a00",true,false,{"key_ops":["sign"],"ext":true,"alg":"Ed448","crv":"Ed448","d":"aEB6ZKLPihMWnEdjFB2D0CzKRSizKwTZbs-Pimi7EgZKAJ8qxipfs9nvrKDK0lEHsWEBcnmyR0y2","x":"5nUt2fNzMwmVbV-E179Xe9QzyIcLWqvBbOC73zzVsgVmNJrqpASmeEFkCAhibVhQF_yHwvnMPfuA","kty":"OKP"},"e6752dd9f3733309956d5f84d7bf577bd433c8870b5aabc16ce0bbdf3cd5b20566349aeaa404a67841640808626d585017fc87c2f9cc3dfb80","807aec3ba128dd2ee9f659be",{"name":"Ed448"},true]
}"##;

#[tokio::test]
async fn key_pair_algorithms_match_node() {
    let expected: serde_json::Value = serde_json::from_str(PAIR_EXPECTED).unwrap();
    assert_same_keys(&run(PAIR_SCRIPT).await, &expected);
}

const RANDOM_SCRIPT: &str = r##"
const data = Buffer.from("randomized payload");
const rsaKo = crypto.createPrivateKey({ key: der("rsa"), format: "der", type: "pkcs8" });
const rsaSpki = crypto.createPublicKey(rsaKo).export({ format: "der", type: "spki" });
const pssPriv = await subtle.importKey("pkcs8", der("rsa"), { name: "RSA-PSS", hash: "SHA-256" }, false, ["sign"]);
const pssPub = await subtle.importKey("spki", rsaSpki, { name: "RSA-PSS", hash: "SHA-256" }, false, ["verify"]);
const pssSig = Buffer.from(await subtle.sign({ name: "RSA-PSS", saltLength: 32 }, pssPriv, data));
const nodePss = crypto.sign("sha256", data, { key: rsaKo, padding: crypto.constants.RSA_PKCS1_PSS_PADDING, saltLength: 32 });
out.pss = [pssSig.length, crypto.verify("sha256", data, { key: rsaKo, padding: 6, saltLength: 32 }, pssSig),
  await subtle.verify({ name: "RSA-PSS", saltLength: 32 }, pssPub, nodePss, data), await subtle.verify({ name: "RSA-PSS", saltLength: 20 }, pssPub, nodePss, data)];
for (const saltLength of [94, 95]) {
  try { out[`pss${saltLength}`] = (await subtle.sign({ name: "RSA-PSS", saltLength }, pssPriv, data)).byteLength; } catch (e) { out[`pss${saltLength}`] = [e.name, e.message]; }
}
out.ecdsa = [];
for (const [name, curve, hash] of [["p256", "P-256", "SHA-256"], ["p384", "P-384", "SHA-384"], ["p521", "P-521", "SHA-512"]]) {
  const ko = crypto.createPrivateKey({ key: der(name), format: "der", type: "pkcs8" });
  const priv = await subtle.importKey("pkcs8", der(name), { name: "ECDSA", namedCurve: curve }, false, ["sign"]);
  const pub = await subtle.importKey("spki", crypto.createPublicKey(ko).export({ format: "der", type: "spki" }), { name: "ECDSA", namedCurve: curve }, false, ["verify"]);
  const sig = Buffer.from(await subtle.sign({ name: "ECDSA", hash }, priv, data));
  const nodeHash = hash.replace("-", "").toLowerCase();
  const nodeSig = crypto.sign(nodeHash, data, { key: ko, dsaEncoding: "ieee-p1363" });
  out.ecdsa.push([sig.length, crypto.verify(nodeHash, data, { key: ko, dsaEncoding: "ieee-p1363" }, sig), await subtle.verify({ name: "ECDSA", hash }, pub, nodeSig, data)]);
}
out.oaep = [];
for (const [hash, label] of [["SHA-1", undefined], ["SHA-256", Buffer.from("label")]]) {
  const priv = await subtle.importKey("pkcs8", der("rsa"), { name: "RSA-OAEP", hash }, false, ["decrypt", "unwrapKey"]);
  const pub = await subtle.importKey("spki", rsaSpki, { name: "RSA-OAEP", hash }, false, ["encrypt", "wrapKey"]);
  const params = { name: "RSA-OAEP", label };
  const oaepHash = hash.replace("-", "").toLowerCase();
  const ct = Buffer.from(await subtle.encrypt(params, pub, data));
  const nodeCt = crypto.publicEncrypt({ key: rsaKo, oaepHash, oaepLabel: label }, data);
  const aes = await subtle.generateKey({ name: "AES-CBC", length: 128 }, true, ["encrypt"]);
  const unwrapped = await subtle.unwrapKey("raw", await subtle.wrapKey("raw", aes, pub, params), priv, params, "AES-CBC", true, ["encrypt"]);
  out.oaep.push([ct.length, crypto.privateDecrypt({ key: rsaKo, oaepHash, oaepLabel: label }, ct).toString(), Buffer.from(await subtle.decrypt(params, priv, nodeCt)).toString(),
    Buffer.from(await subtle.exportKey("raw", unwrapped)).equals(Buffer.from(await subtle.exportKey("raw", aes)))]);
}
const rsaGen = await subtle.generateKey({ name: "RSASSA-PKCS1-v1_5", modulusLength: 1024, publicExponent: new Uint8Array([1, 0, 1]), hash: "SHA-256" }, true, ["sign", "verify"]);
const rsaSig = Buffer.from(await subtle.sign("RSASSA-PKCS1-v1_5", rsaGen.privateKey, data));
out.genRsa = [rsaGen.publicKey.algorithm.modulusLength, Array.from(rsaGen.publicKey.algorithm.publicExponent), rsaGen.publicKey.usages, rsaGen.privateKey.usages,
  rsaGen.publicKey.extractable, crypto.verify("sha256", data, crypto.KeyObject.from(rsaGen.publicKey), rsaSig), (await subtle.exportKey("jwk", rsaGen.privateKey)).alg];
const oaepGen = await subtle.generateKey({ name: "RSA-OAEP", modulusLength: 1024, publicExponent: new Uint8Array([3]), hash: "SHA-256" }, false, ["encrypt", "decrypt", "wrapKey"]);
out.genOaep = [oaepGen.publicKey.usages, oaepGen.privateKey.usages, Buffer.from(await subtle.decrypt("RSA-OAEP", oaepGen.privateKey, await subtle.encrypt("RSA-OAEP", oaepGen.publicKey, data))).toString(),
  crypto.KeyObject.from(oaepGen.publicKey).asymmetricKeyDetails.publicExponent.toString()];
for (const curve of ["P-256", "P-384", "P-521"]) {
  const pair = await subtle.generateKey({ name: "ECDSA", namedCurve: curve }, true, ["sign", "verify"]);
  const sig = Buffer.from(await subtle.sign({ name: "ECDSA", hash: "SHA-384" }, pair.privateKey, data));
  const a = await subtle.generateKey({ name: "ECDH", namedCurve: curve }, true, ["deriveBits"]);
  const b = await subtle.generateKey({ name: "ECDH", namedCurve: curve }, true, ["deriveKey", "deriveBits"]);
  const s1 = Buffer.from(await subtle.deriveBits({ name: "ECDH", public: b.publicKey }, a.privateKey));
  const s2 = Buffer.from(await subtle.deriveBits({ name: "ECDH", public: a.publicKey }, b.privateKey));
  const nodeSecret = crypto.diffieHellman({ privateKey: crypto.KeyObject.from(a.privateKey), publicKey: crypto.KeyObject.from(b.publicKey) });
  out[curve] = [pair.privateKey.algorithm, await subtle.verify({ name: "ECDSA", hash: "SHA-384" }, pair.publicKey, sig, data),
    crypto.verify("sha384", data, { key: crypto.KeyObject.from(pair.publicKey), dsaEncoding: "ieee-p1363" }, sig), s1.equals(s2), s1.equals(nodeSecret), s1.length, a.publicKey.usages, b.privateKey.usages];
}
for (const alg of ["Ed25519", "Ed448", "X25519", "X448"]) {
  const usages = alg.startsWith("Ed") ? ["sign", "verify"] : ["deriveBits"];
  const [a, b] = [await subtle.generateKey(alg, true, usages), await subtle.generateKey({ name: alg }, true, usages)];
  if (alg.startsWith("Ed")) {
    const sig = Buffer.from(await subtle.sign(alg, a.privateKey, data));
    out[alg] = [Object.keys(a), await subtle.verify(alg, a.publicKey, sig, data), crypto.verify(null, data, crypto.KeyObject.from(a.publicKey), sig), (await subtle.exportKey("raw", a.publicKey)).byteLength];
  } else {
    const s1 = Buffer.from(await subtle.deriveBits({ name: alg, public: b.publicKey }, a.privateKey, null));
    out[alg] = [Object.keys(a), s1.equals(Buffer.from(await subtle.deriveBits({ name: alg, public: a.publicKey }, b.privateKey, null))), s1.length, a.publicKey.usages];
  }
}
const kek = await subtle.generateKey({ name: "AES-KW", length: 256 }, true, ["wrapKey", "unwrapKey"]);
const ed = await subtle.generateKey("Ed25519", true, ["sign", "verify"]);
const unwrappedEd = await subtle.unwrapKey("pkcs8", await subtle.wrapKey("pkcs8", ed.privateKey, kek, "AES-KW"), kek, "AES-KW", "Ed25519", true, ["sign"]);
out.kwEd = [Buffer.from(await subtle.exportKey("pkcs8", unwrappedEd)).equals(Buffer.from(await subtle.exportKey("pkcs8", ed.privateKey))),
  (await subtle.unwrapKey("jwk", await subtle.wrapKey("jwk", ed.privateKey, kek, "AES-KW"), kek, "AES-KW", "Ed25519", false, ["sign"])).type];
return out;
"##;

const RANDOM_EXPECTED: &str = r##"{
  "pss": [128,true,true,false],
  "pss94": 128,
  "pss95": ["OperationError","The operation failed for an operation-specific reason"],
  "ecdsa": [[64,true,true],[96,true,true],[132,true,true]],
  "oaep": [[128,"randomized payload","randomized payload",true],[128,"randomized payload","randomized payload",true]],
  "genRsa": [1024,[1,0,1],["verify"],["sign"],true,true,"RS256"],
  "genOaep": [["encrypt","wrapKey"],["decrypt"],"randomized payload","3"],
  "P-256": [{"name":"ECDSA","namedCurve":"P-256"},true,true,true,true,32,[],["deriveKey","deriveBits"]],
  "P-384": [{"name":"ECDSA","namedCurve":"P-384"},true,true,true,true,48,[],["deriveKey","deriveBits"]],
  "P-521": [{"name":"ECDSA","namedCurve":"P-521"},true,true,true,true,66,[],["deriveKey","deriveBits"]],
  "Ed25519": [["privateKey","publicKey"],true,true,32],
  "Ed448": [["privateKey","publicKey"],true,true,57],
  "X25519": [["privateKey","publicKey"],true,32,[]],
  "X448": [["privateKey","publicKey"],true,56,[]],
  "kwEd": [true,"private"]
}"##;

#[tokio::test]
async fn randomized_operations_round_trip_with_node_crypto() {
    let expected: serde_json::Value = serde_json::from_str(RANDOM_EXPECTED).unwrap();
    assert_same_keys(&run(RANDOM_SCRIPT).await, &expected);
}

// A JWT flow as jose runs it on WebCrypto: import JWK, PKCS#8, and SPKI keys, sign, and verify.
const JWT_SCRIPT: &str = r##"
const b64u = (value) => Buffer.from(value).toString("base64url");
const part = (object) => b64u(JSON.stringify(object));
async function signJwt(alg, params, key, payload) {
  const input = `${part({ alg, typ: "JWT" })}.${part(payload)}`;
  return `${input}.${b64u(await subtle.sign(params, key, Buffer.from(input)))}`;
}
async function verifyJwt(token, params, key) {
  const [header, payload, signature] = token.split(".");
  const valid = await subtle.verify(params, key, Buffer.from(signature, "base64url"), Buffer.from(`${header}.${payload}`));
  return valid ? JSON.parse(Buffer.from(payload, "base64url")) : null;
}
const payload = { sub: "user-1", iat: 1700000000 };
const hs = await subtle.importKey("jwk", { kty: "oct", k: Buffer.alloc(32, 9).toString("base64url"), alg: "HS256" }, { name: "HMAC", hash: "SHA-256" }, false, ["sign", "verify"]);
const rsPriv = await subtle.importKey("pkcs8", der("rsa"), { name: "RSASSA-PKCS1-v1_5", hash: "SHA-256" }, true, ["sign"]);
const { n, e } = await subtle.exportKey("jwk", rsPriv);
const rsPub = await subtle.importKey("jwk", { kty: "RSA", n, e, alg: "RS256", use: "sig" }, { name: "RSASSA-PKCS1-v1_5", hash: "SHA-256" }, true, ["verify"]);
const es = await subtle.generateKey({ name: "ECDSA", namedCurve: "P-256" }, true, ["sign", "verify"]);
const edPriv = await subtle.importKey("pkcs8", der("ed25519"), { name: "Ed25519" }, false, ["sign"]);
const edSpki = crypto.createPublicKey({ key: der("ed25519"), format: "der", type: "pkcs8" }).export({ format: "der", type: "spki" });
const edPub = await subtle.importKey("spki", edSpki, "Ed25519", true, ["verify"]);
const flows = {
  HS256: ["HMAC", hs, hs, "sha256"],
  RS256: ["RSASSA-PKCS1-v1_5", rsPriv, rsPub, "sha256"],
  ES256: [{ name: "ECDSA", hash: "SHA-256" }, es.privateKey, es.publicKey, "sha256"],
  EdDSA: ["Ed25519", edPriv, edPub, null],
};
for (const [alg, [params, signKey, verifyKey, hash]] of Object.entries(flows)) {
  const token = await signJwt(alg, params, signKey, payload);
  const [header, body, signature] = token.split(".");
  const tampered = `${header}.${part({ ...payload, sub: "admin" })}.${signature}`;
  const input = Buffer.from(`${header}.${body}`);
  const sig = Buffer.from(signature, "base64url");
  const ko = crypto.KeyObject.from(verifyKey);
  const nodeValid = alg === "HS256" ? crypto.createHmac(hash, verifyKey).update(input).digest().equals(sig) : crypto.verify(hash, input, { key: ko, dsaEncoding: "ieee-p1363" }, sig);
  out[alg] = [alg === "ES256" ? sig.length : token, await verifyJwt(token, params, verifyKey), await verifyJwt(tampered, params, verifyKey), nodeValid];
}
return out;
"##;

const JWT_EXPECTED: &str = r##"{
  "HS256": ["eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiJ1c2VyLTEiLCJpYXQiOjE3MDAwMDAwMDB9.pBLpZeQA5wnemNpXS86_WY4ItdOe_t3PSfYGnlJD1M0",{"sub":"user-1","iat":1700000000},null,true],
  "RS256": ["eyJhbGciOiJSUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiJ1c2VyLTEiLCJpYXQiOjE3MDAwMDAwMDB9.MuZ2iSZk7HSsW0gRAvKhbM5CoCYxemVWfGKSQTyAWx4ffrlOklqUWgLvmSMOZufDuvTDYX019PPuSbG6bSYdKJdIr3pDLX63AxgGrcH6p7D8i1KFDG3ueSh2qTZbEAs6br9het20wXKH3UY9N8d6ArTk9RtUyQM2rsofJuqja4U",{"sub":"user-1","iat":1700000000},null,true],
  "ES256": [64,{"sub":"user-1","iat":1700000000},null,true],
  "EdDSA": ["eyJhbGciOiJFZERTQSIsInR5cCI6IkpXVCJ9.eyJzdWIiOiJ1c2VyLTEiLCJpYXQiOjE3MDAwMDAwMDB9.utnETV1WtCRkDZZZzO5EUHVS3HLTbpcyKyrAddsfD_dIp54eIRAw4Uu4mUwqt3zyBGBRYdaobWW2qX8y3bxjCg",{"sub":"user-1","iat":1700000000},null,true]
}"##;

#[tokio::test]
async fn jwt_flow_signs_and_verifies_through_subtle() {
    let expected: serde_json::Value = serde_json::from_str(JWT_EXPECTED).unwrap();
    assert_same_keys(&run(JWT_SCRIPT).await, &expected);
}
