mod common;

use common::*;
use serde_json::json;

// Vectors: NIST SP 800-38A (ECB, CBC, CFB, OFB, CTR), the GCM spec test cases 5, 7 and 8,
// RFC 3610 (CCM), RFC 7253 (OCB), the classic DES worked example, RFC 8032 (Ed448), and
// RFC 7748 (X448). Encrypted keys come from OpenSSL 3 (`pkcs8 -topk8 -v2`, `ec -aes128`).

async fn run(script: &str) -> serde_json::Value {
    let (_dir, root) = tempdir();
    let (runtime, _capture) = runtime(&root);
    run_cjs(&runtime, &root, script).await
}

const MODES_SCRIPT: &str = r##"
const crypto = require("node:crypto");
const hex = (s) => Buffer.from(s, "hex");
const out = {};
const P = "6bc1bee22e409f96e93d7e117393172aae2d8a571e03ac9c9eb76fac45af8e5130c81c46a35ce411e5fbc1191a0a52eff69f2445df4f9b17ad2b417be66c3710";
const K = "2b7e151628aed2a6abf7158809cf4f3c", IV = "000102030405060708090a0b0c0d0e0f";
// Encrypt in uneven chunks, then decrypt the result in other chunks.
const both = (name, key, iv, plain, padding = false) => {
  const c = crypto.createCipheriv(name, hex(key), iv === null ? null : hex(iv));
  if (!padding) c.setAutoPadding(false);
  const p = hex(plain), parts = [];
  for (let i = 0; i < p.length; i += 7) parts.push(c.update(p.subarray(i, i + 7)));
  parts.push(c.final());
  const sealed = Buffer.concat(parts);
  const d = crypto.createDecipheriv(name, hex(key), iv === null ? null : hex(iv));
  if (!padding) d.setAutoPadding(false);
  const opened = Buffer.concat([d.update(sealed.subarray(0, 5)), d.update(sealed.subarray(5)), d.final()]);
  return [sealed.toString("hex"), opened.toString("hex") === plain];
};
out.ecb = both("aes-128-ecb", K, null, P);
out.cbc = both("aes-128-cbc", K, IV, P);
out.cbc256 = both("aes256", "603deb1015ca71be2b73aef0857d77811f352c073b6108d72d9810a30914dff4", IV, P);
out.cfb = both("aes-128-cfb", K, IV, P);
out.ofb = both("aes-128-ofb", K, IV, P);
out.ctr = both("aes-128-ctr", K, "f0f1f2f3f4f5f6f7f8f9fafbfcfdfeff", P);
out.ctrOdd = both("aes-128-ctr", K, IV, P.slice(0, 50))[1];
const padded = both("aes-192-cbc", "00".repeat(24), IV, "616263", true);
out.padded = [padded[0].length, padded[1], both("aes-128-cbc", K, IV, P.slice(0, 64), true)[0].length];
out.des = both("des-ecb", "133457799bbcdff1", null, "0123456789abcdef");
out.des3 = both("des-ede3", "133457799bbcdff1".repeat(3), null, "0123456789abcdef");
out.des3cbc = both("des-ede3-cbc", "0123456789abcdeffedcba987654321089abcdef01234567", "1234567890abcdef", "616263", true);
out.desRound = ["des-cbc", "des-ede-cbc", "des3", "des-ede3-cfb", "des-ofb"].map((name) => {
  const key = name === "des-cbc" || name === "des-ofb" ? "11".repeat(8) : name === "des-ede-cbc" ? "22".repeat(16) : "33".repeat(24);
  return both(name, key, "0102030405060708", P.slice(0, 42), name.endsWith("cbc") || name === "des3")[1];
});
const seal = (name, key, iv, plain, aad, tagLength) => {
  const options = tagLength ? { authTagLength: tagLength } : undefined;
  const c = crypto.createCipheriv(name, hex(key), hex(iv), options);
  if (aad) c.setAAD(hex(aad));
  const sealed = Buffer.concat([c.update(hex(plain)), c.final()]).toString("hex");
  const tag = c.getAuthTag();
  const d = crypto.createDecipheriv(name, hex(key), hex(iv), options);
  if (aad) d.setAAD(hex(aad));
  d.setAuthTag(tag);
  const opened = Buffer.concat([d.update(hex(sealed)), d.final()]).toString("hex");
  return [sealed, tag.toString("hex"), opened === plain];
};
out.gcm7 = seal("aes-192-gcm", "00".repeat(24), "00".repeat(12), "");
out.gcm8 = seal("aes-192-gcm", "00".repeat(24), "00".repeat(12), "00".repeat(16));
const p4 = "d9313225f88406e5a55909c5aff5269a86a7a9531534f7da2e4c303d8a318a721c3c0c95956809532fcf0e2449a6b525b16aedf5aa0de657ba637b39";
out.gcm5 = seal("aes-128-gcm", "feffe9928665731c6d6a8f9467308308", "cafebabefacedbad", p4, "feedfacedeadbeeffeedfacedeadbeefabaddad2");
out.gcm16 = seal("aes-256-gcm", "11".repeat(32), "22".repeat(16), "abcdef")[2];
const ccmKey = "c0c1c2c3c4c5c6c7c8c9cacbcccdcecf";
out.ccm = seal("aes-128-ccm", ccmKey, "00000003020100a0a1a2a3a4a5", "08090a0b0c0d0e0f101112131415161718191a1b1c1d1e", "0001020304050607", 8);
const ocbKey = "000102030405060708090a0b0c0d0e0f";
out.ocb = [
  seal("aes-128-ocb", ocbKey, "bbaa99887766554433221100", ""),
  seal("aes-128-ocb", ocbKey, "bbaa99887766554433221101", "0001020304050607", "0001020304050607"),
  seal("aes-128-ocb", ocbKey, "bbaa99887766554433221103", "0001020304050607"),
];
const failures = [];
for (const [name, key, iv, options] of [["aes-128-ccm", ccmKey, "00000003020100a0a1a2a3a4a5", { authTagLength: 8 }], ["aes-128-ocb", ocbKey, "bbaa99887766554433221101"], ["aes-192-gcm", "00".repeat(24), "00".repeat(12)]]) {
  const d = crypto.createDecipheriv(name, hex(key), hex(iv), options);
  d.setAuthTag(Buffer.alloc(options ? 8 : 16));
  d.update(Buffer.alloc(8));
  try { d.final(); failures.push(null); } catch (e) { failures.push(e.message); }
}
out.failures = failures;
const codes = [];
for (const f of [
  () => { const d = crypto.createDecipheriv("aes-128-cbc", hex(K), hex(IV)); d.update(Buffer.alloc(16)); d.final(); },
  () => { const c = crypto.createCipheriv("aes-128-cbc", hex(K), hex(IV)); c.setAutoPadding(false); c.update("abc"); c.final(); },
  () => crypto.createCipheriv("aes-128-cbc", hex(K), Buffer.alloc(8)),
  () => crypto.createCipheriv("des-ede3-cbc", Buffer.alloc(16), Buffer.alloc(8)),
  () => crypto.createCipheriv("aes-128-ccm", hex(K), Buffer.alloc(12)),
  () => crypto.createCipheriv("aes-128-ctr", hex(K), hex(IV)).getAuthTag(),
  () => crypto.createCipheriv("aes-128-cbc", hex(K), hex(IV)).setAAD(Buffer.alloc(1)),
]) {
  try { f(); codes.push(null); } catch (e) { codes.push(e.code); }
}
out.codes = codes;
return out;
"##;

#[tokio::test]
async fn cipher_modes_match_nist_and_rfc_vectors() {
    let expected = json!({
        "ecb": ["3ad77bb40d7a3660a89ecaf32466ef97f5d3d58503b9699de785895a96fdbaaf43b1cd7f598ece23881b00e3ed0306887b0c785e27e8ad3f8223207104725dd4", true],
        "cbc": ["7649abac8119b246cee98e9b12e9197d5086cb9b507219ee95db113a917678b273bed6b8e3c1743b7116e69e222295163ff1caa1681fac09120eca307586e1a7", true],
        "cbc256": ["f58c4c04d6e5f1ba779eabfb5f7bfbd69cfc4e967edb808d679f777bc6702c7d39f23369a9d9bacfa530e26304231461b2eb05e2c39be9fcda6c19078c6a9d1b", true],
        "cfb": ["3b3fd92eb72dad20333449f8e83cfb4ac8a64537a0b3a93fcde3cdad9f1ce58b26751f67a3cbb140b1808cf187a4f4dfc04b05357c5d1c0eeac4c66f9ff7f2e6", true],
        "ofb": ["3b3fd92eb72dad20333449f8e83cfb4a7789508d16918f03f53c52dac54ed8259740051e9c5fecf64344f7a82260edcc304c6528f659c77866a510d9c1d6ae5e", true],
        "ctr": ["874d6191b620e3261bef6864990db6ce9806f66b7970fdff8617187bb9fffdff5ae4df3edbd5d35e5b4f09020db03eab1e031dda2fbe03d1792170a0f3009cee", true],
        "ctrOdd": true,
        "padded": [32, true, 96],
        "des": ["85e813540f0ab405", true],
        "des3": ["85e813540f0ab405", true],
        "des3cbc": ["793dec494a1faa0a", true],
        "desRound": [true, true, true, true, true],
        "gcm7": ["", "cd33b28ac773f74ba00ed1f312572435", true],
        "gcm8": ["98e7247c07f0fe411c267e4384b0f600", "2ff58d80033927ab8ef4d4587514f0fb", true],
        "gcm5": [
            "61353b4c2806934a777ff51fa22a4755699b2a714fcdc6f83766e5f97b6c742373806900e49f24b22b097544d4896b424989b5e1ebac0f07c23f4598",
            "3612d2e79e3b0785561be14aaca2fccb",
            true
        ],
        "gcm16": true,
        "ccm": ["588c979a61c663d2f066d0c2c0f989806d5f6b61dac384", "17e8d12cfdf926e0", true],
        "ocb": [
            ["", "785407bfffc8ad9edcc5520ac9111ee6", true],
            ["6820b3657b6f615a", "5725bda0d3b4eb3a257c9af1f8f03009", true],
            ["45dd69f8f5aae724", "14054cd1f35d82760b2cd00d2f99bfa9", true]
        ],
        "failures": [
            "Unsupported state or unable to authenticate data",
            "Unsupported state or unable to authenticate data",
            "Unsupported state or unable to authenticate data"
        ],
        "codes": [
            "ERR_OSSL_BAD_DECRYPT",
            "ERR_OSSL_WRONG_FINAL_BLOCK_LENGTH",
            "ERR_CRYPTO_INVALID_IV",
            "ERR_CRYPTO_INVALID_KEYLEN",
            "ERR_CRYPTO_INVALID_AUTH_TAG",
            "ERR_CRYPTO_INVALID_STATE",
            "ERR_CRYPTO_INVALID_STATE"
        ]
    });
    assert_same_keys(&run(MODES_SCRIPT).await, &expected);
}

const KEYS_SCRIPT: &str = r##"
const crypto = require("node:crypto");
const b64 = (h) => Buffer.from(h, "hex").toString("base64url");
const out = {};
const edSeed = "6c82a562cb808d10d632be89c8513ebf6c929f34ddfa8c9f63c9960ef6e348a3528c8a3fcc2f044e39a3fc5b94492f8f032e7549a20098f95b";
const ed = crypto.createPrivateKey({ format: "jwk", key: { kty: "OKP", crv: "Ed448", d: b64(edSeed), x: "" } });
const edPub = crypto.createPublicKey(ed);
const edSig = crypto.sign(null, Buffer.alloc(0), ed);
out.ed448 = [ed.asymmetricKeyType, Buffer.from(edPub.export({ format: "jwk" }).x, "base64url").toString("hex"), edSig.toString("hex"), crypto.verify(null, Buffer.alloc(0), edPub, edSig), crypto.verify(null, Buffer.from("x"), edPub, edSig)];
const x448 = (d) => crypto.createPrivateKey({ format: "jwk", key: { kty: "OKP", crv: "X448", d: b64(d), x: "" } });
const alice = x448("9a8f4925d1519f5775cf46b04b5800d4ee9ee8bae8bc5565d498c28dd9c9baf574a9419744897391006382a6f127ab1d9ac2d8c0a598726b");
const bob = x448("1c306a7ac2a0e2e0990b294470cba339e6453772b075811d8fad0d1d6927c120bb5ee8972b0d3e21374c9c921b09d1b0366f10b65173992d");
out.x448 = [crypto.diffieHellman({ privateKey: alice, publicKey: crypto.createPublicKey(bob) }).toString("hex"), crypto.diffieHellman({ privateKey: bob, publicKey: alice }).toString("hex")];
const pairs = {};
for (const type of ["ed448", "x448"]) {
  const { publicKey, privateKey } = crypto.generateKeyPairSync(type, { publicKeyEncoding: { type: "spki", format: "der" }, privateKeyEncoding: { type: "pkcs8", format: "pem" } });
  const priv = crypto.createPrivateKey(privateKey), pub = crypto.createPublicKey({ key: publicKey, format: "der", type: "spki" });
  pairs[type] = [priv.asymmetricKeyType, type === "x448" ? crypto.diffieHellman({ privateKey: priv, publicKey: pub }).length : crypto.verify(null, Buffer.from("m"), pub, crypto.sign(null, Buffer.from("m"), priv))];
}
out.pairs = pairs;
const dsa = crypto.generateKeyPairSync("dsa", { modulusLength: 1024 });
const dsaPem = dsa.privateKey.export({ type: "pkcs8", format: "pem" });
const dsaSpki = dsa.publicKey.export({ type: "spki", format: "der" });
const reloaded = crypto.createPrivateKey(dsaPem);
const sig = crypto.sign("sha256", Buffer.from("data"), reloaded);
const p1363 = crypto.sign("sha1", Buffer.from("data"), { key: dsa.privateKey, dsaEncoding: "ieee-p1363" });
out.dsa = [
  dsa.privateKey.asymmetricKeyType,
  dsa.privateKey.asymmetricKeyDetails,
  reloaded.equals(dsa.privateKey),
  crypto.verify("sha256", Buffer.from("data"), crypto.createPublicKey({ key: dsaSpki, format: "der", type: "spki" }), sig),
  crypto.verify("sha256", Buffer.from("date"), dsa.publicKey, sig),
  p1363.length,
  crypto.createVerify("sha1").update("data").verify({ key: dsa.publicKey, dsaEncoding: "ieee-p1363" }, p1363),
];
const openssl = `-----BEGIN ENCRYPTED PRIVATE KEY-----
MIH0MF8GCSqGSIb3DQEFDTBSMDEGCSqGSIb3DQEFDDAkBBDaBzge+2gGWzaUAAke
sJH4AgIIADAMBggqhkiG9w0CCQUAMB0GCWCGSAFlAwQBKgQQFTtVy31Pj2z6h5n8
uODvlQSBkK9/tfuydq+xuBOhjBmWQ9smze3ldNDwrKRQNHS9OXo5rNo9sDyDeYB5
UhVDzcQuJMQgY0hemQaTLG9eQrnC6lW5eAj+NHJv5H/dMsLCmSFqsW5vZ0JZUJTu
04O4Js31lY96+RKAANAoe63WrHW4YbDLEZdsMV2N1HewzByxrkvY2ZE7tXYkfjLL
IExtyauaug==
-----END ENCRYPTED PRIVATE KEY-----
`;
const legacy = `-----BEGIN EC PRIVATE KEY-----
Proc-Type: 4,ENCRYPTED
DEK-Info: AES-128-CBC,2744C82E126DF93D584BA2DFFA730094

jvf2PvTdCDWo7alnX2y8OB0r/ZN2X4upErhDP7kPKB5T+ZFOHZoYbUHYl7KRcUUE
EIkTTh/xi8EYdaCM8LnrS8d1lch9TnzMC3Wp2chvIA7IFQ69NEjNH4J9s4DF2NN/
fhsp75CcZKBPk+ZzP1tLpQtZqp/QwUswru2uyoV0380=
-----END EC PRIVATE KEY-----
`;
const publicPem = "-----BEGIN PUBLIC KEY-----\nMFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAEVOFz0d56yH3AE8ZKXxiWo5rviVGk\nMLEZB+ocKSIBAfmlx4SdDtFwiSHtd9vmF6sGQJiUltroV8/X+ALlHjbRyw==\n-----END PUBLIC KEY-----\n";
const fromOpenssl = crypto.createPrivateKey({ key: openssl, passphrase: "hunter2" });
const fromLegacy = crypto.createPrivateKey({ key: legacy, passphrase: Buffer.from("hunter2") });
out.openssl = [fromOpenssl.equals(fromLegacy), crypto.createPublicKey(fromOpenssl).export({ type: "spki", format: "pem" }) === publicPem];
const ec = crypto.generateKeyPairSync("ec", { namedCurve: "P-384" }).privateKey;
const encrypted = ec.export({ type: "pkcs8", format: "pem", cipher: "aes-256-cbc", passphrase: "pw" });
const encryptedDer = ec.export({ type: "pkcs8", format: "der", cipher: "aes-128-cbc", passphrase: "pw" });
const rsa = crypto.generateKeyPairSync("rsa", { modulusLength: 1024, privateKeyEncoding: { type: "pkcs1", format: "pem", cipher: "des-ede3-cbc", passphrase: "pw" }, publicKeyEncoding: { type: "spki", format: "pem" } });
const rsaKey = crypto.createPrivateKey({ key: rsa.privateKey, passphrase: "pw" });
out.encrypted = [
  encrypted.split("\n")[0],
  crypto.createPrivateKey({ key: encrypted, passphrase: "pw" }).equals(ec),
  crypto.createPrivateKey({ key: encryptedDer, format: "der", type: "pkcs8", passphrase: "pw" }).equals(ec),
  rsa.privateKey.split("\n").slice(0, 3).map((line) => line.split(",")[0]),
  crypto.verify("sha256", Buffer.from("m"), rsa.publicKey, crypto.sign("sha256", Buffer.from("m"), { key: rsa.privateKey, passphrase: "pw" })),
  rsaKey.asymmetricKeyType,
];
const codes = [];
for (const f of [
  () => crypto.createPrivateKey(encrypted),
  () => crypto.createPrivateKey({ key: encrypted, passphrase: "wrong" }),
  () => crypto.createPrivateKey({ key: legacy, passphrase: "wrong" }),
  () => crypto.createPublicKey(encrypted),
  () => ec.export({ type: "pkcs8", format: "pem", cipher: "aes-256-cbc" }),
  () => crypto.generateKeyPairSync("dsa", { modulusLength: 2048, divisorLength: 160 }),
]) {
  try { f(); codes.push(null); } catch (e) { codes.push(e.code); }
}
out.codes = codes;
return out;
"##;

#[tokio::test]
async fn new_key_types_and_encrypted_keys_round_trip() {
    let expected = json!({
        "ed448": [
            "ed448",
            "5fd7449b59b461fd2ce787ec616ad46a1da1342485a70e1f8a0ea75d80e96778edf124769b46c7061bd6783df1e50f6cd1fa1abeafe8256180",
            "533a37f6bbe457251f023c0d88f976ae2dfb504a843e34d2074fd823d41a591f2b233f034f628281f2fd7a22ddd47d7828c59bd0a21bfd3980ff0d2028d4b18a9df63e006c5d1c2d345b925d8dc00b4104852db99ac5c7cdda8530a113a0f4dbb61149f05a7363268c71d95808ff2e652600",
            true,
            false
        ],
        "x448": [
            "07fff4181ac6cc95ec1c16a94a0f74d12da232ce40a77552281d282bb60c0b56fd2464c335543936521c24403085d59a449a5037514a879d",
            "07fff4181ac6cc95ec1c16a94a0f74d12da232ce40a77552281d282bb60c0b56fd2464c335543936521c24403085d59a449a5037514a879d"
        ],
        "pairs": { "ed448": ["ed448", true], "x448": ["x448", 56] },
        "dsa": ["dsa", { "modulusLength": 1024, "divisorLength": 160 }, true, true, false, 40, true],
        "openssl": [true, true],
        "encrypted": [
            "-----BEGIN ENCRYPTED PRIVATE KEY-----",
            true,
            true,
            ["-----BEGIN RSA PRIVATE KEY-----", "Proc-Type: 4", "DEK-Info: DES-EDE3-CBC"],
            true,
            "rsa"
        ],
        "codes": [
            "ERR_MISSING_PASSPHRASE",
            "ERR_OSSL_BAD_DECRYPT",
            "ERR_OSSL_BAD_DECRYPT",
            "ERR_MISSING_PASSPHRASE",
            "ERR_INVALID_ARG_VALUE",
            "ERR_PI_UNSUPPORTED"
        ]
    });
    assert_same_keys(&run(KEYS_SCRIPT).await, &expected);
}
