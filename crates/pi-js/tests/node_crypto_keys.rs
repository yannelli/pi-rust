mod common;

use common::*;
use serde_json::json;

// Vectors: RFC 7914 (scrypt), RFC 5869 (HKDF), RFC 3526/2409 (MODP groups), the GCM spec
// test cases, RFC 8439 (ChaCha20-Poly1305), RFC 8032 (Ed25519), RFC 7748 (X25519), RFC 6979 (ECDSA).

async fn run(script: &str) -> serde_json::Value {
    let (_dir, root) = tempdir();
    let (runtime, _capture) = runtime(&root);
    run_cjs(&runtime, &root, script).await
}

const KDF_SCRIPT: &str = r##"
const crypto = require("node:crypto");
const out = {};
out.scrypt = [
  crypto.scryptSync("", "", 64, { N: 16, r: 1, p: 1 }).toString("hex"),
  crypto.scryptSync("password", "NaCl", 64, { cost: 1024, blockSize: 8, parallelization: 16 }).toString("hex"),
  crypto.scryptSync("pleaseletmein", "SodiumChloride", 64, { N: 16384, r: 8, p: 1 }).toString("hex"),
];
out.scryptAsync = await new Promise((r) => crypto.scrypt("", "", 16, { N: 16, r: 1, p: 1 }, (e, k) => r(k.toString("hex"))));
const salt = Buffer.from("000102030405060708090a0b0c", "hex");
const info = Buffer.from("f0f1f2f3f4f5f6f7f8f9", "hex");
const tc1 = crypto.hkdfSync("sha256", Buffer.alloc(22, 0x0b), salt, info, 42);
out.hkdf = [
  tc1 instanceof ArrayBuffer,
  Buffer.from(tc1).toString("hex"),
  Buffer.from(crypto.hkdfSync("sha256", Buffer.alloc(22, 0x0b), "", "", 42)).toString("hex"),
  Buffer.from(crypto.hkdfSync("sha1", Buffer.alloc(11, 0x0b), salt, info, 42)).toString("hex"),
];
out.hkdfAsync = await new Promise((r) => crypto.hkdf("sha256", crypto.createSecretKey(Buffer.alloc(22, 0x0b)), salt, info, 42, (e, k) => r(Buffer.from(k).toString("hex") === out.hkdf[1])));
const codes = [];
for (const f of [
  () => crypto.scryptSync("a", "b", 16, { N: 3 }),
  () => crypto.scryptSync("a", "b", 16, { N: 32768 }),
  () => crypto.scryptSync("a", "b", 16, { N: 16, cost: 16 }),
  () => crypto.hkdfSync("sha256", "k", "", "", 255 * 32 + 1),
  () => crypto.hkdfSync("nope", "k", "", "", 1),
  () => crypto.hkdfSync("sha256", "k", "", Buffer.alloc(1025), 1),
]) {
  try { f(); codes.push(null); } catch (e) { codes.push(e.code); }
}
out.codes = codes;
out.heap = crypto.secureHeapUsed();
return out;
"##;

#[tokio::test]
async fn scrypt_and_hkdf_match_rfc_vectors() {
    let expected = json!({
        "scrypt": [
            "77d6576238657b203b19ca42c18a0497f16b4844e3074ae8dfdffa3fede21442fcd0069ded0948f8326a753a0fc81f17e8d3e0fb2e0d3628cf35e20c38d18906",
            "fdbabe1c9d3472007856e7190d01e9fe7c6ad7cbc8237830e77376634b3731622eaf30d92e22a3886ff109279d9830dac727afb94a83ee6d8360cbdfa2cc0640",
            "7023bdcb3afd7348461c06cd81fd38ebfda8fbba904f8e3ea9b543f6545da1f2d5432955613f0fcf62d49705242a9af9e61e85dc0d651e40dfcf017b45575887"
        ],
        "scryptAsync": "77d6576238657b203b19ca42c18a0497",
        "hkdf": [
            true,
            "3cb25f25faacd57a90434f64d0362f2a2d2d0a90cf1a5a4c5db02d56ecc4c5bf34007208d5b887185865",
            "8da4e775a563c18f715f802a063c5a31b8a11f5c5ee1879ec3454e5f3c738d2d9d201395faa4b61a96c8",
            "085a01ea1b10f36933068b56efa5ad81a4f14b822f5b091568a9cdd4f155fda2c22e422478d305f3f896"
        ],
        "hkdfAsync": true,
        "codes": [
            "ERR_CRYPTO_INVALID_SCRYPT_PARAMS",
            "ERR_CRYPTO_INVALID_SCRYPT_PARAMS",
            "ERR_INCOMPATIBLE_OPTION_PAIR",
            "ERR_CRYPTO_INVALID_KEYLEN",
            "ERR_CRYPTO_INVALID_DIGEST",
            "ERR_OUT_OF_RANGE"
        ],
        "heap": { "total": 0, "min": 0, "used": 0, "utilization": 0 }
    });
    assert_same_keys(&run(KDF_SCRIPT).await, &expected);
}

const CIPHER_SCRIPT: &str = r##"
const crypto = require("node:crypto");
const hex = (s) => Buffer.from(s, "hex");
const out = {};
const seal = (name, key, iv, plain, aad, chunks = 1) => {
  const c = crypto.createCipheriv(name, hex(key), hex(iv));
  if (aad) c.setAAD(hex(aad));
  const p = hex(plain), parts = [];
  const step = Math.ceil(p.length / chunks) || 1;
  for (let i = 0; i < p.length; i += step) parts.push(c.update(p.subarray(i, i + step)));
  parts.push(c.final());
  return [Buffer.concat(parts).toString("hex"), c.getAuthTag().toString("hex")];
};
out.gcm1 = seal("aes-128-gcm", "00000000000000000000000000000000", "000000000000000000000000", "");
out.gcm2 = seal("aes-128-gcm", "00000000000000000000000000000000", "000000000000000000000000", "00000000000000000000000000000000");
const p4 = "d9313225f88406e5a55909c5aff5269a86a7a9531534f7da2e4c303d8a318a721c3c0c95956809532fcf0e2449a6b525b16aedf5aa0de657ba637b39";
out.gcm4 = seal("aes-128-gcm", "feffe9928665731c6d6a8f9467308308", "cafebabefacedbaddecaf888", p4, "feedfacedeadbeeffeedfacedeadbeefabaddad2", 3);
out.gcm14 = seal("aes-256-gcm", "00".repeat(32), "00".repeat(12), "00".repeat(16));
const open = (tag) => {
  const d = crypto.createDecipheriv("aes-128-gcm", hex("feffe9928665731c6d6a8f9467308308"), hex("cafebabefacedbaddecaf888"));
  d.setAAD(hex("feedfacedeadbeeffeedfacedeadbeefabaddad2"));
  d.setAuthTag(hex(tag));
  const text = d.update(out.gcm4[0], "hex", "hex");
  return text + d.final("hex");
};
out.open = open(out.gcm4[1]) === p4;
try { open("00".repeat(16)); } catch (e) { out.openBad = e.message; }
const sunscreen = Buffer.from("Ladies and Gentlemen of the class of '99: If I could offer you only one tip for the future, sunscreen would be it.").toString("hex");
const key = Array.from({ length: 32 }, (_, i) => (0x80 + i).toString(16)).join("");
const chacha = seal("chacha20-poly1305", key, "070000004041424344454647", sunscreen, "50515253c0c1c2c3c4c5c6c7", 2);
out.chacha = [chacha[0].slice(0, 32), chacha[1]];
const short = crypto.createCipheriv("aes-256-gcm", Buffer.alloc(32), Buffer.alloc(12), { authTagLength: 8 });
short.update("hi"); short.final();
out.shortTag = short.getAuthTag().length;
out.ciphers = ["aes-128-gcm", "aes-192-ccm", "aes-256-ocb", "chacha20-poly1305", "des-ede3-cbc", "id-aes256-gcm"].every((name) => crypto.getCiphers().includes(name));
const codes = [];
for (const f of [
  () => crypto.createCipheriv("aes-256-cfb8", Buffer.alloc(32), Buffer.alloc(16)),
  () => crypto.createCipheriv("nope-cipher", Buffer.alloc(32), Buffer.alloc(16)),
  () => crypto.createCipheriv("aes-256-gcm", Buffer.alloc(31), Buffer.alloc(12)),
  () => crypto.createCipheriv("chacha20-poly1305", Buffer.alloc(32), Buffer.alloc(8)),
  () => crypto.createCipheriv("aes-128-gcm", Buffer.alloc(16), Buffer.alloc(12)).getAuthTag(),
]) {
  try { f(); codes.push(null); } catch (e) { codes.push(e.code); }
}
out.codes = codes;
return out;
"##;

#[tokio::test]
async fn aead_ciphers_match_known_vectors() {
    let expected = json!({
        "gcm1": ["", "58e2fccefa7e3061367f1d57a4e7455a"],
        "gcm2": ["0388dace60b6a392f328c2b971b2fe78", "ab6e47d42cec13bdf53a67b21257bddf"],
        "gcm4": [
            "42831ec2217774244b7221b784d0d49ce3aa212f2c02a4e035c17e2329aca12e21d514b25466931c7d8f6a5aac84aa051ba30b396a0aac973d58e091",
            "5bc94fbc3221a5db94fae95ae7121a47"
        ],
        "gcm14": ["cea7403d4d606b6e074ec5d3baf39d18", "d0d1c8a799996bf0265b98b5d48ab919"],
        "open": true,
        "openBad": "Unsupported state or unable to authenticate data",
        "chacha": ["d31a8d34648e60db7b86afbc53ef7ec2", "1ae10b594f09e26a7e902ecbd0600691"],
        "shortTag": 8,
        "ciphers": true,
        "codes": ["ERR_PI_UNSUPPORTED", "ERR_CRYPTO_UNKNOWN_CIPHER", "ERR_CRYPTO_INVALID_KEYLEN", "ERR_CRYPTO_INVALID_IV", "ERR_CRYPTO_INVALID_STATE"]
    });
    assert_same_keys(&run(CIPHER_SCRIPT).await, &expected);
}

const CURVE_SCRIPT: &str = r##"
const crypto = require("node:crypto");
const b64 = (h) => Buffer.from(h, "hex").toString("base64url");
const out = {};
const ed = crypto.createPrivateKey({ format: "jwk", key: { kty: "OKP", crv: "Ed25519", d: b64("9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60"), x: b64("d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a") } });
const edPub = crypto.createPublicKey(ed);
out.ed = [ed.asymmetricKeyType, Buffer.from(edPub.export({ format: "jwk" }).x, "base64url").toString("hex"), crypto.sign(null, Buffer.alloc(0), ed).toString("hex")];
const ed2 = crypto.createPrivateKey({ format: "jwk", key: { kty: "OKP", crv: "Ed25519", d: b64("4ccd089b28ff96da9db6c346ec114e0f5b8a319f35aba624da8cf6ed4fb8a6fb"), x: "" } });
const sig2 = crypto.sign(undefined, Buffer.from([0x72]), ed2);
out.ed2 = [sig2.toString("hex"), crypto.verify(null, Buffer.from([0x72]), crypto.createPublicKey(ed2), sig2), crypto.verify(null, Buffer.from([0x73]), ed2, sig2)];
const okp = (d) => crypto.createPrivateKey({ format: "jwk", key: { kty: "OKP", crv: "X25519", d: b64(d), x: "" } });
const alice = okp("77076d0a7318a57d3c16c17251b26645df4c2f87ebc0992ab177fba51db92c2a");
const bob = okp("5dab087e624a8a4b79e17f8b83800ee66f3bb1292618b6fd1c2f8b27ff88e0eb");
const pubHex = (k) => Buffer.from(crypto.createPublicKey(k).export({ format: "jwk" }).x, "base64url").toString("hex");
out.x25519 = [pubHex(alice), pubHex(bob), crypto.diffieHellman({ privateKey: alice, publicKey: crypto.createPublicKey(bob) }).toString("hex"), crypto.diffieHellman({ privateKey: bob, publicKey: alice }).toString("hex")];
const d = "c9afa9d845ba75166b5c215767b1d6934e50c3db36e89b127b8a622b120f6721";
const ecdh = crypto.createECDH("prime256v1");
ecdh.setPrivateKey(d, "hex");
out.ecdhPub = [ecdh.getPublicKey("hex", "compressed"), crypto.ECDH.convertKey(ecdh.getPublicKey(), "prime256v1", undefined, "hex", "compressed")];
const pubJwk = { kty: "EC", crv: "P-256", x: b64(ecdh.getPublicKey("hex").slice(2, 66)), y: b64(ecdh.getPublicKey("hex").slice(66)) };
const ecPub = crypto.createPublicKey({ key: pubJwk, format: "jwk" });
const rfcSig = Buffer.from("efd48b2aacb6a8fd1140dd9cd45e81d69d2c877b56aaf991c34d0ea84eaf3716f7cb1c942d657c41d436c7a1b6e29f65f3e900dbb9aff4064dc4ab2f843acda8", "hex");
out.ecdsaRfc = [crypto.createVerify("SHA256").update("sample").verify({ key: ecPub, dsaEncoding: "ieee-p1363" }, rfcSig), crypto.verify("sha256", Buffer.from("sampl"), { key: ecPub, dsaEncoding: "ieee-p1363" }, rfcSig)];
const ecPriv = crypto.createPrivateKey({ key: { ...pubJwk, d: b64(d) }, format: "jwk" });
const sec1 = ecPriv.export({ type: "sec1", format: "pem" });
const derSig = crypto.createSign("sha384").update("msg").sign(sec1, "base64");
out.ecdsa = [ecPriv.asymmetricKeyDetails.namedCurve, sec1.split("\n")[0], crypto.createVerify("sha384").update("msg").verify(ecPub.export({ type: "spki", format: "pem" }), derSig, "base64"), crypto.createPrivateKey(sec1).equals(ecPriv)];
const a = crypto.createECDH("secp256k1"), b = crypto.createECDH("secp256k1");
a.generateKeys(); b.generateKeys();
out.k1 = [a.computeSecret(b.getPublicKey()).equals(b.computeSecret(a.getPublicKey("hex", "compressed"), "hex")), a.getPrivateKey().length];
for (const [type, opts] of [["ec", { namedCurve: "secp384r1" }], ["ec", { namedCurve: "P-521" }], ["ed25519"], ["x25519"]]) {
  const { publicKey, privateKey } = crypto.generateKeyPairSync(type, { ...opts, publicKeyEncoding: { type: "spki", format: "pem" }, privateKeyEncoding: { type: "pkcs8", format: "der" } });
  const priv = crypto.createPrivateKey({ key: privateKey, format: "der", type: "pkcs8" });
  const ok = type === "x25519" ? crypto.diffieHellman({ privateKey: priv, publicKey: crypto.createPublicKey(publicKey) }).length === 32 : crypto.verify(null, Buffer.from("x"), publicKey, crypto.sign(null, Buffer.from("x"), priv));
  out[type + (opts ? opts.namedCurve : "")] = [publicKey.startsWith("-----BEGIN PUBLIC KEY-----\n"), priv.asymmetricKeyType, ok];
}
out.curves = crypto.getCurves();
const codes = [];
for (const f of [
  () => crypto.createECDH("nope"),
  () => crypto.generateKeyPairSync("ec", { namedCurve: "nope" }),
  () => ecdh.computeSecret(Buffer.from("04" + "00".repeat(64), "hex")),
  () => crypto.createPrivateKey("garbage"),
  () => crypto.generateKeyPairSync("dsa", { modulusLength: 1000 }),
  () => crypto.generateKeyPairSync("nope"),
  () => crypto.createSign("sha256").update("x").sign(ed),
  () => crypto.diffieHellman({ privateKey: ed, publicKey: alice }),
]) {
  try { f(); codes.push(null); } catch (e) { codes.push(e.code); }
}
out.codes = codes;
return out;
"##;

#[tokio::test]
async fn curve_keys_match_rfc_vectors() {
    let x = "60fed4ba255a9d31c961eb74c6356d68c049b8923b61fa6ce669622e60f29fb6";
    let expected = json!({
        "ed": [
            "ed25519",
            "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a",
            "e5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e065224901555fb8821590a33bacc61e39701cf9b46bd25bf5f0595bbe24655141438e7a100b"
        ],
        "ed2": [
            "92a009a9f0d4cab8720e820b5f642540a2b27b5416503f8fb3762223ebdb69da085ac1e43e15996e458f3613d0f11d8c387b2eaeb4302aeeb00d291612bb0c00",
            true,
            false
        ],
        "x25519": [
            "8520f0098930a754748b7ddcb43ef75a0dbf3a0d26381af4eba4a98eaa9b4e6a",
            "de9edb7d7b7dc1b4d35b61c2ece435373f8343c85b78674dadfc7e146f882b4f",
            "4a5d9d5ba4ce2de1728e3bf480350f25e07e21c947d19e3376f09b3c1e161742",
            "4a5d9d5ba4ce2de1728e3bf480350f25e07e21c947d19e3376f09b3c1e161742"
        ],
        "ecdhPub": [format!("03{x}"), format!("03{x}")],
        "ecdsaRfc": [true, false],
        "ecdsa": ["prime256v1", "-----BEGIN EC PRIVATE KEY-----", true, true],
        "k1": [true, 32],
        "ecsecp384r1": [true, "ec", true],
        "ecP-521": [true, "ec", true],
        "ed25519": [true, "ed25519", true],
        "x25519": [true, "x25519", true],
        "curves": ["prime256v1", "secp256k1", "secp384r1", "secp521r1"],
        "codes": [
            "ERR_CRYPTO_INVALID_CURVE",
            "ERR_CRYPTO_INVALID_CURVE",
            "ERR_CRYPTO_ECDH_INVALID_PUBLIC_KEY",
            "ERR_OSSL_UNSUPPORTED",
            "ERR_PI_UNSUPPORTED",
            "ERR_INVALID_ARG_VALUE",
            "ERR_OSSL_EVP_OPERATION_NOT_SUPPORTED_FOR_THIS_KEYTYPE",
            "ERR_CRYPTO_INCOMPATIBLE_KEY"
        ]
    });
    assert_same_keys(&run(CURVE_SCRIPT).await, &expected);
}

const RSA_DH_SCRIPT: &str = r##"
const crypto = require("node:crypto");
const out = {};
const { publicKey, privateKey } = crypto.generateKeyPairSync("rsa", { modulusLength: 1024 });
const details = privateKey.asymmetricKeyDetails;
out.details = [privateKey.asymmetricKeyType, details.modulusLength, typeof details.publicExponent, String(details.publicExponent)];
const pkcs1 = privateKey.export({ type: "pkcs1", format: "pem" });
const spkiDer = publicKey.export({ type: "spki", format: "der" });
const jwk = privateKey.export({ format: "jwk" });
out.encodings = [
  pkcs1.split("\n")[0],
  crypto.createPrivateKey(pkcs1).equals(privateKey),
  crypto.createPublicKey({ key: spkiDer, format: "der", type: "spki" }).equals(publicKey),
  crypto.createPrivateKey({ key: jwk, format: "jwk" }).equals(privateKey),
  Object.keys(jwk).join(","),
  crypto.createPublicKey(pkcs1).equals(publicKey),
  publicKey.export({ type: "pkcs1", format: "pem" }).split("\n")[0],
];
const data = Buffer.from("payload");
const pss = { key: privateKey, padding: crypto.constants.RSA_PKCS1_PSS_PADDING, saltLength: 32 };
out.signatures = [
  crypto.verify("sha256", data, publicKey, crypto.sign("sha256", data, privateKey)),
  crypto.createVerify("RSA-SHA512").update(data).verify(publicKey, crypto.createSign("RSA-SHA512").update(data).sign(pkcs1, "hex"), "hex"),
  crypto.verify("sha256", data, { key: publicKey, padding: 6 }, crypto.sign("sha256", data, pss)),
  crypto.verify("sha256", data, { key: publicKey, padding: 6, saltLength: 20 }, crypto.sign("sha256", data, pss)),
  crypto.sign("sha256", data, privateKey).equals(crypto.sign("sha256", data, privateKey)),
  crypto.verify("sha1", data, privateKey, crypto.sign("sha256", data, privateKey)),
];
const secret = Buffer.from("top secret");
out.encryption = [
  crypto.privateDecrypt(privateKey, crypto.publicEncrypt(publicKey, secret)).toString(),
  crypto.privateDecrypt({ key: privateKey, oaepHash: "sha256", oaepLabel: Buffer.from("l") }, crypto.publicEncrypt({ key: pkcs1, oaepHash: "sha256", oaepLabel: Buffer.from("l") }, secret)).toString(),
  crypto.privateDecrypt({ key: privateKey, padding: 1 }, crypto.publicEncrypt({ key: publicKey, padding: 1 }, secret)).toString(),
  crypto.publicDecrypt(publicKey, crypto.privateEncrypt(privateKey, secret)).toString(),
  crypto.publicEncrypt(publicKey, secret).length,
];
try { crypto.privateDecrypt(privateKey, Buffer.alloc(128, 1)); } catch (e) { out.badDecrypt = e.code; }
out.pair = await new Promise((r) => crypto.generateKeyPair("rsa-pss", { modulusLength: 512 }, (e, pub, priv) => r([e, pub.type, priv.asymmetricKeyType])));
const groups = {};
for (const name of ["modp1", "modp2", "modp5", "modp14", "modp15", "modp16", "modp17", "modp18"]) {
  const prime = crypto.createDiffieHellmanGroup(name).getPrime("hex");
  groups[name] = [prime.length * 4, prime.slice(0, 32), prime.slice(-32)];
}
out.groups = groups;
const alice = crypto.getDiffieHellman("modp14"), bob = crypto.getDiffieHellman("modp14");
alice.generateKeys(); bob.generateKeys();
out.modp14 = [alice.computeSecret(bob.getPublicKey()).equals(bob.computeSecret(alice.getPublicKey("hex"), "hex", "buffer")), alice.getGenerator("hex"), alice.verifyError];
const dh = crypto.createDiffieHellman(Buffer.from("ffffffffffffffc5", "hex"), 5);
dh.setPrivateKey(Buffer.from([3]));
out.small = [dh.getPublicKey("hex"), dh.verifyError];
const generated = crypto.createDiffieHellman(64);
out.generated = [generated.getPrime().length, generated.verifyError, typeof generated.generateKeys("base64")];
const kp = (opts) => crypto.generateKeyPairSync("dh", opts);
const g1 = kp({ group: "modp1" }), g2 = kp({ group: "modp1" });
out.dhKeys = [g1.privateKey.asymmetricKeyType, crypto.diffieHellman({ privateKey: g1.privateKey, publicKey: g2.publicKey }).equals(crypto.diffieHellman({ privateKey: g2.privateKey, publicKey: g1.publicKey }))];
out.primes = [
  crypto.checkPrimeSync(2n ** 127n - 1n),
  crypto.checkPrimeSync(Buffer.from([15])),
  crypto.checkPrimeSync(2n ** 61n - 1n, { checks: 10 }),
];
const p32 = crypto.generatePrimeSync(32, { bigint: true });
const safe = crypto.generatePrimeSync(32, { safe: true, bigint: true });
const constrained = crypto.generatePrimeSync(32, { add: 12n, rem: 11n, bigint: true });
const buf = crypto.generatePrimeSync(48);
out.generatedPrimes = [p32.toString(2).length, crypto.checkPrimeSync(p32), crypto.checkPrimeSync((safe - 1n) / 2n), Number(constrained % 12n), buf instanceof ArrayBuffer, buf.byteLength];
out.primeAsync = await new Promise((r) => crypto.generatePrime(16, { bigint: true }, (e, p) => crypto.checkPrime(p, (e2, ok) => r(ok))));
const keys = [crypto.generateKeySync("aes", { length: 256 }), crypto.generateKeySync("hmac", { length: 64 })];
out.secret = [keys[0].symmetricKeySize, keys[1].symmetricKeySize, keys[1].export({ format: "jwk" }).kty];
const codes = [];
for (const f of [
  () => crypto.generateKeyPairSync("rsa", {}),
  () => crypto.createDiffieHellmanGroup("modp99"),
  () => crypto.generateKeySync("aes", { length: 100 }),
  () => crypto.checkPrimeSync("7"),
  () => privateKey.export({ type: "sec1", format: "pem" }),
  () => publicKey.export({ format: "pem" }),
  () => crypto.createPublicKey(publicKey),
]) {
  try { f(); codes.push(null); } catch (e) { codes.push(e.code); }
}
out.codes = codes;
return out;
"##;

#[tokio::test]
async fn rsa_dh_and_primes_round_trip() {
    let head = "ffffffffffffffffc90fdaa22168c234";
    let expected = json!({
        "details": ["rsa", 1024, "bigint", "65537"],
        "encodings": ["-----BEGIN RSA PRIVATE KEY-----", true, true, true, "kty,n,e,d,p,q,dp,dq,qi", true, "-----BEGIN RSA PUBLIC KEY-----"],
        "signatures": [true, true, true, false, true, false],
        "encryption": ["top secret", "top secret", "top secret", "top secret", 128],
        "badDecrypt": "ERR_OSSL_RSA_OAEP_DECODING_ERROR",
        "pair": [null, "public", "rsa-pss"],
        "groups": {
            "modp1": [768, head, "f44c42e9a63a3620ffffffffffffffff"],
            "modp2": [1024, head, "49286651ece65381ffffffffffffffff"],
            "modp5": [1536, head, "f1746c08ca237327ffffffffffffffff"],
            "modp14": [2048, head, "15728e5a8aacaa68ffffffffffffffff"],
            "modp15": [3072, head, "4b82d120a93ad2caffffffffffffffff"],
            "modp16": [4096, head, "4df435c934063199ffffffffffffffff"],
            "modp17": [6144, head, "e694f91e6dcc4024ffffffffffffffff"],
            "modp18": [8192, head, "60c980dd98edd3dfffffffffffffffff"]
        },
        "modp14": [true, "02", 0],
        "small": ["000000000000007d", 2],
        "generated": [8, 0, "string"],
        "dhKeys": ["dh", true],
        "primes": [true, false, true],
        "generatedPrimes": [32, true, true, 11, true, 6],
        "primeAsync": true,
        "secret": [32, 8, "oct"],
        "codes": [
            "ERR_INVALID_ARG_TYPE",
            "ERR_CRYPTO_UNKNOWN_DH_GROUP",
            "ERR_INVALID_ARG_VALUE",
            "ERR_INVALID_ARG_TYPE",
            "ERR_CRYPTO_INCOMPATIBLE_KEY_OPTIONS",
            "ERR_INVALID_ARG_VALUE",
            "ERR_CRYPTO_INVALID_KEY_OBJECT_TYPE"
        ]
    });
    assert_same_keys(&run(RSA_DH_SCRIPT).await, &expected);
}
