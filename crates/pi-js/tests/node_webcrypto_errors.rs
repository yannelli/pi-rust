use crate::common;
use crate::node_webcrypto::run;

// Expected values come from running the same scripts under Node 22.23.5.

const ERRORS_SCRIPT: &str = r##"
const settle = async (f) => {
  try {
    const value = await f();
    return ["ok", value === undefined ? null : typeof value];
  } catch (e) {
    return [e.constructor.name, e.name, e.message, e.code];
  }
};
const u8 = (n) => new Uint8Array(n);
const aes = await subtle.importKey("raw", u8(16), "AES-GCM", false, ["encrypt"]);
const gcmDec = await subtle.importKey("raw", u8(16), "AES-GCM", true, ["decrypt"]);
const cbcDec = await subtle.importKey("raw", u8(16), "AES-CBC", true, ["decrypt"]);
const kw = await subtle.importKey("raw", u8(16), "AES-KW", true, ["wrapKey", "unwrapKey"]);
const hmac15 = await subtle.importKey("raw", u8(15), { name: "HMAC", hash: "SHA-256" }, true, ["sign"]);
const hkdf = await subtle.importKey("raw", u8(16), "HKDF", false, ["deriveBits"]);
const pbkdf2 = await subtle.importKey("raw", u8(16), "PBKDF2", false, ["deriveBits", "deriveKey"]);
const ecdsa = await subtle.generateKey({ name: "ECDSA", namedCurve: "P-256" }, false, ["sign", "verify"]);
const x25519 = await subtle.generateKey("X25519", true, ["deriveBits"]);
const ecdh = await subtle.generateKey({ name: "ECDH", namedCurve: "P-384" }, true, ["deriveBits"]);
const ed = await subtle.generateKey("Ed25519", true, ["sign", "verify"]);
const hkdfParams = { name: "HKDF", hash: "SHA-256", salt: u8(0), info: u8(0) };
const pbkdf2Params = { name: "PBKDF2", hash: "SHA-256", salt: u8(0), iterations: 1 };
const gcm = { name: "AES-GCM", iv: u8(12) };
const oct = (extra) => ({ kty: "oct", k: "AAAAAAAAAAAAAAAAAAAAAA", ...extra });
const okp = (extra) => ({ kty: "OKP", crv: "Ed25519", x: "AAAA", ...extra });
const ec = (extra) => ({ kty: "EC", crv: "P-256", x: "AAAA", y: "AAAA", ...extra });
const cases = {
  ctor: () => new CryptoKey(),
  keyObjectFrom: () => crypto.KeyObject.from({}),
  invalidThis: () => subtle.digest.call({}, "SHA-256", u8(1)),
  unboundRandom: () => { const { getRandomValues } = globalThis.crypto; return getRandomValues(u8(2)); },
  randomArgs: () => globalThis.crypto.getRandomValues(),
  missingArgs: () => subtle.importKey("raw"),
  badFormat: () => subtle.importKey("foo", u8(16), "AES-GCM", true, ["encrypt"]),
  badUsage: () => subtle.importKey("raw", u8(16), "AES-GCM", true, ["foo"]),
  usagesNotSequence: () => subtle.importKey("raw", u8(16), "AES-GCM", true, "encrypt"),
  unknownAlgorithm: () => subtle.importKey("raw", u8(16), "AES-FOO", true, ["encrypt"]),
  algorithmWithoutName: () => subtle.importKey("raw", u8(16), {}, true, ["encrypt"]),
  stringData: () => subtle.importKey("raw", "abc", "AES-GCM", true, ["encrypt"]),
  aesLength: () => subtle.importKey("raw", u8(15), "AES-GCM", true, ["encrypt"]),
  aesUsage: () => subtle.importKey("raw", u8(16), "AES-KW", true, ["encrypt"]),
  emptyUsages: () => subtle.importKey("raw", u8(16), "AES-GCM", true, []),
  hmacNoHash: () => subtle.importKey("raw", u8(16), { name: "HMAC" }, true, ["sign"]),
  hmacBadHash: () => subtle.importKey("raw", u8(16), { name: "HMAC", hash: "SHA-3" }, true, ["sign"]),
  hmacOddLength: () => subtle.importKey("raw", u8(16), { name: "HMAC", hash: "SHA-256", length: 100 }, true, ["sign"]),
  hmacLengthMismatch: () => subtle.importKey("raw", u8(16), { name: "HMAC", hash: "SHA-256", length: 64 }, true, ["sign"]),
  hmacEmpty: () => subtle.importKey("raw", u8(0), { name: "HMAC", hash: "SHA-256" }, true, ["sign"]),
  hmacRange: () => subtle.importKey("raw", u8(1), { name: "HMAC", hash: "SHA-256", length: -1 }, true, ["sign"]),
  hkdfExtractable: () => subtle.importKey("raw", u8(16), "HKDF", true, ["deriveBits"]),
  hkdfUsage: () => subtle.importKey("raw", u8(16), "HKDF", false, ["sign"]),
  hkdfJwk: () => subtle.importKey("jwk", oct(), "HKDF", false, ["deriveBits"]),
  genAesLength: () => subtle.generateKey({ name: "AES-GCM", length: 100 }, true, ["encrypt"]),
  genEmptyUsages: () => subtle.generateKey({ name: "AES-GCM", length: 128 }, true, []),
  genCurve: () => subtle.generateKey({ name: "ECDSA", namedCurve: "P-999" }, true, ["sign"]),
  genEcdhUsage: () => subtle.generateKey({ name: "ECDH", namedCurve: "P-256" }, true, ["sign"]),
  genEdUsage: () => subtle.generateKey({ name: "Ed25519" }, true, ["deriveBits"]),
  genRsaUsage: () => subtle.generateKey({ name: "RSA-OAEP", modulusLength: 1024, publicExponent: new Uint8Array([1, 0, 1]), hash: "SHA-256" }, true, ["sign"]),
  genRsaExponent: () => subtle.generateKey({ name: "RSA-PSS", modulusLength: 1024, publicExponent: new Uint8Array([1, 0, 0, 0, 0, 1]), hash: "SHA-256" }, true, ["sign"]),
  genRsaExponentType: () => subtle.generateKey({ name: "RSA-PSS", modulusLength: 1024, publicExponent: [1, 0, 1], hash: "SHA-256" }, true, ["sign"]),
  genHmacZero: () => subtle.generateKey({ name: "HMAC", hash: "SHA-256", length: 0 }, true, ["sign"]),
  notExtractable: () => subtle.exportKey("raw", aes),
  gcmNoIv: () => subtle.encrypt({ name: "AES-GCM" }, aes, u8(1)),
  wrongUsage: () => subtle.decrypt(gcm, aes, u8(1)),
  wrongAlgorithm: () => subtle.encrypt({ name: "AES-CBC", iv: u8(16) }, aes, u8(1)),
  cbcIv: () => subtle.encrypt({ name: "AES-CBC", iv: u8(15) }, aes, u8(1)),
  ctrLength: () => subtle.encrypt({ name: "AES-CTR", counter: u8(16), length: 0 }, aes, u8(1)),
  ctrRange: () => subtle.encrypt({ name: "AES-CTR", counter: u8(16), length: 300 }, aes, u8(1)),
  gcmTag: () => subtle.encrypt({ ...gcm, tagLength: 100 }, aes, u8(1)),
  signWithAes: () => subtle.sign("AES-GCM", aes, u8(1)),
  notAKey: () => subtle.encrypt(gcm, {}, u8(1)),
  gcmAuth: () => subtle.decrypt(gcm, gcmDec, u8(20)),
  gcmShort: () => subtle.decrypt(gcm, gcmDec, u8(5)),
  cbcPadding: () => subtle.decrypt({ name: "AES-CBC", iv: u8(16) }, cbcDec, u8(16)),
  kwEncrypt: () => subtle.encrypt("AES-KW", kw, u8(16)),
  kwLength: () => subtle.wrapKey("raw", hmac15, kw, "AES-KW"),
  kwIntegrity: () => subtle.unwrapKey("raw", u8(24), kw, "AES-KW", "AES-GCM", true, ["encrypt"]),
  hkdfOddLength: () => subtle.deriveBits(hkdfParams, hkdf, 7),
  hkdfNoLength: () => subtle.deriveBits(hkdfParams, hkdf),
  hkdfTooLong: () => subtle.deriveBits(hkdfParams, hkdf, 255 * 256 + 8),
  hkdfNoSalt: () => subtle.deriveBits({ name: "HKDF", hash: "SHA-256", info: u8(0) }, hkdf, 8),
  hkdfDeriveKeyUsage: () => subtle.deriveKey(hkdfParams, hkdf, "AES-GCM", true, ["encrypt"]),
  keyAlgorithmMismatch: () => subtle.deriveBits(pbkdf2Params, hkdf, 8),
  pbkdf2Zero: () => subtle.deriveBits({ ...pbkdf2Params, iterations: 0 }, pbkdf2, 8),
  deriveKeyNoLength: () => subtle.deriveKey(pbkdf2Params, pbkdf2, { name: "AES-GCM" }, true, ["encrypt"]),
  deriveKeyEcdsa: () => subtle.deriveKey(pbkdf2Params, pbkdf2, { name: "ECDSA", namedCurve: "P-256" }, false, ["sign"]),
  digestName: () => subtle.digest("SHA-3", u8(0)),
  exportHkdf: () => subtle.exportKey("raw", hkdf),
  exportFormat: () => subtle.exportKey("spki", hmac15),
  jwkKty: () => subtle.importKey("jwk", oct({ kty: "RSA" }), "AES-GCM", true, ["encrypt"]),
  jwkNoKty: () => subtle.importKey("jwk", { k: "AAAA" }, "AES-GCM", true, ["encrypt"]),
  jwkUse: () => subtle.importKey("jwk", oct({ use: "sig" }), "AES-GCM", true, ["encrypt"]),
  jwkOps: () => subtle.importKey("jwk", oct({ key_ops: ["decrypt"] }), "AES-GCM", true, ["encrypt"]),
  jwkDuplicateOps: () => subtle.importKey("jwk", oct({ key_ops: ["encrypt", "encrypt"] }), "AES-GCM", true, ["encrypt"]),
  jwkExt: () => subtle.importKey("jwk", oct({ ext: false }), "AES-GCM", true, ["encrypt"]),
  jwkAlg: () => subtle.importKey("jwk", oct({ alg: "A256GCM" }), "AES-GCM", true, ["encrypt"]),
  jwkNoK: () => subtle.importKey("jwk", { kty: "oct" }, "AES-GCM", true, ["encrypt"]),
  hmacJwkAlg: () => subtle.importKey("jwk", oct({ alg: "HS384" }), { name: "HMAC", hash: "SHA-256" }, true, ["sign"]),
  spkiGarbage: () => subtle.importKey("spki", u8(10), { name: "ECDSA", namedCurve: "P-256" }, true, ["verify"]),
  spkiUsage: () => subtle.importKey("spki", u8(10), { name: "RSA-OAEP", hash: "SHA-256" }, true, ["decrypt"]),
  rawEcPoint: () => subtle.importKey("raw", u8(65), { name: "ECDSA", namedCurve: "P-256" }, true, ["verify"]),
  rawRsa: () => subtle.importKey("raw", u8(65), { name: "RSA-OAEP", hash: "SHA-256" }, true, ["encrypt"]),
  rawEdLength: () => subtle.importKey("raw", u8(31), "Ed25519", true, ["verify"]),
  rawEdUsage: () => subtle.importKey("raw", u8(32), "Ed25519", true, ["sign"]),
  okpCrv: () => subtle.importKey("jwk", okp({ crv: "X25519" }), "Ed25519", true, ["verify"]),
  okpAlg: () => subtle.importKey("jwk", okp({ alg: "ES256" }), "Ed25519", true, ["verify"]),
  okpNoX: () => subtle.importKey("jwk", okp({ x: undefined, d: "AAAA" }), "Ed25519", true, ["sign"]),
  okpShort: () => subtle.importKey("jwk", okp(), "Ed25519", true, ["verify"]),
  ecCrv: () => subtle.importKey("jwk", ec({ crv: "P-384" }), { name: "ECDSA", namedCurve: "P-256" }, true, ["verify"]),
  ecAlg: () => subtle.importKey("jwk", ec({ alg: "ES384" }), { name: "ECDSA", namedCurve: "P-256" }, true, ["verify"]),
  ecPoint: () => subtle.importKey("jwk", ec(), { name: "ECDSA", namedCurve: "P-256" }, true, ["verify"]),
  rsaJwkAlg: () => subtle.importKey("jwk", { kty: "RSA", alg: "RS384", n: "AAAA", e: "AQAB" }, { name: "RSASSA-PKCS1-v1_5", hash: "SHA-256" }, true, ["verify"]),
  curveName: () => subtle.importKey("raw", u8(65), { name: "ECDSA", namedCurve: "secp256k1" }, true, ["verify"]),
  ed448Context: () => subtle.sign({ name: "Ed448", context: u8(1) }, aes, u8(1)),
  ecdsaNoHash: () => subtle.sign({ name: "ECDSA" }, ecdsa.privateKey, u8(1)),
  signWithPublic: () => subtle.sign({ name: "ECDSA", hash: "SHA-256" }, ecdsa.publicKey, u8(1)),
  verifyWithPrivate: () => subtle.verify({ name: "ECDSA", hash: "SHA-256" }, ecdsa.privateKey, u8(64), u8(1)),
  verifyShortSignature: () => subtle.verify({ name: "ECDSA", hash: "SHA-256" }, ecdsa.publicKey, u8(3), u8(1)),
  exportPrivateRaw: () => subtle.exportKey("raw", ed.privateKey),
  exportPublicPkcs8: () => subtle.exportKey("pkcs8", ed.publicKey),
  ecdhPublicType: () => subtle.deriveBits({ name: "X25519", public: x25519.privateKey }, x25519.privateKey, 8),
  ecdhPublicAlgorithm: () => subtle.deriveBits({ name: "X25519", public: ecdh.publicKey }, x25519.privateKey, 8),
  ecdhNoPublic: () => subtle.deriveBits({ name: "X25519" }, x25519.privateKey, 8),
  ecdhBaseUsage: () => subtle.deriveBits({ name: "X25519", public: x25519.publicKey }, x25519.publicKey, 8),
  ecdhTooLong: () => subtle.deriveBits({ name: "X25519", public: x25519.publicKey }, x25519.privateKey, 264),
  toCryptoKeyFamily: () => crypto.createSecretKey(u8(16)).toCryptoKey("Ed25519", true, ["sign"]),
  toCryptoKeyUsages: () => crypto.createSecretKey(u8(16)).toCryptoKey("AES-GCM", true, []),
  toCryptoKeyLength: () => crypto.createSecretKey(u8(15)).toCryptoKey("AES-GCM", true, ["encrypt"]),
  toCryptoKeyCurve: () => crypto.generateKeyPairSync("ec", { namedCurve: "P-256" }).privateKey.toCryptoKey({ name: "ECDSA", namedCurve: "P-384" }, true, ["sign"]),
  toCryptoKeyType: () => crypto.generateKeyPairSync("ed25519").publicKey.toCryptoKey("X25519", true, []),
  createPrivateKey: () => crypto.createPrivateKey(ed.privateKey),
  createPublicKeyFromPublic: () => crypto.createPublicKey(ed.publicKey),
  getter: () => Object.getOwnPropertyDescriptor(CryptoKey.prototype, "type").get.call({}),
};
for (const [name, f] of Object.entries(cases)) out[name] = await settle(f);
return out;
"##;

const ERRORS_EXPECTED: &str = r##"{
  "ctor": ["TypeError","TypeError","Illegal constructor","ERR_ILLEGAL_CONSTRUCTOR"],
  "keyObjectFrom": ["TypeError","TypeError","The \"key\" argument must be an instance of CryptoKey. Received an instance of Object","ERR_INVALID_ARG_TYPE"],
  "invalidThis": ["TypeError","TypeError","Value of \"this\" must be of type SubtleCrypto","ERR_INVALID_THIS"],
  "unboundRandom": ["TypeError","TypeError","Value of \"this\" must be of type Crypto","ERR_INVALID_THIS"],
  "randomArgs": ["TypeError","TypeError","Failed to execute 'getRandomValues' on 'Crypto': 1 argument required, but only 0 present.","ERR_MISSING_ARGS"],
  "missingArgs": ["TypeError","TypeError","Failed to execute 'importKey' on 'SubtleCrypto': 4 arguments required, but only 1 present.","ERR_MISSING_ARGS"],
  "badFormat": ["TypeError","TypeError","Failed to execute 'importKey' on 'SubtleCrypto': 1st argument value 'foo' is not a valid enum value of type KeyFormat.","ERR_INVALID_ARG_VALUE"],
  "badUsage": ["TypeError","TypeError","Failed to execute 'importKey' on 'SubtleCrypto': 5th argument, index 0 value 'foo' is not a valid enum value of type KeyUsage.","ERR_INVALID_ARG_VALUE"],
  "usagesNotSequence": ["TypeError","TypeError","Failed to execute 'importKey' on 'SubtleCrypto': 5th argument can not be converted to sequence.","ERR_INVALID_ARG_TYPE"],
  "unknownAlgorithm": ["DOMException","NotSupportedError","Unrecognized algorithm name",9],
  "algorithmWithoutName": ["TypeError","TypeError","Failed to normalize algorithm: passed algorithm can not be converted to 'Algorithm' because 'name' is required in 'Algorithm'.","ERR_MISSING_OPTION"],
  "stringData": ["TypeError","TypeError","Failed to execute 'importKey' on 'SubtleCrypto': 2nd argument is not instance of ArrayBuffer, Buffer, TypedArray, or DataView.","ERR_INVALID_ARG_TYPE"],
  "aesLength": ["DOMException","DataError","Invalid key length",0],
  "aesUsage": ["DOMException","SyntaxError","Unsupported key usage for an AES key",12],
  "emptyUsages": ["DOMException","SyntaxError","Usages cannot be empty when importing a secret key.",12],
  "hmacNoHash": ["TypeError","TypeError","Failed to normalize algorithm: passed algorithm can not be converted to 'HmacImportParams' because 'hash' is required in 'HmacImportParams'.","ERR_MISSING_OPTION"],
  "hmacBadHash": ["DOMException","NotSupportedError","Unrecognized algorithm name",9],
  "hmacOddLength": ["DOMException","NotSupportedError","Unsupported algorithm.length",9],
  "hmacLengthMismatch": ["DOMException","DataError","Invalid key length",0],
  "hmacEmpty": ["DOMException","DataError","Zero-length key is not supported",0],
  "hmacRange": ["TypeError","TypeError","Failed to normalize algorithm: 'length' of 'HmacImportParams' (passed algorithm) is outside the expected range of 0 to 4294967295.","ERR_OUT_OF_RANGE"],
  "hkdfExtractable": ["DOMException","SyntaxError","HKDF keys are not extractable",12],
  "hkdfUsage": ["DOMException","SyntaxError","Unsupported key usage for a HKDF key",12],
  "hkdfJwk": ["DOMException","NotSupportedError","Unable to import HKDF using jwk format",9],
  "genAesLength": ["DOMException","OperationError","AES key length must be 128, 192, or 256 bits",0],
  "genEmptyUsages": ["DOMException","SyntaxError","Usages cannot be empty when creating a key.",12],
  "genCurve": ["DOMException","NotSupportedError","Unrecognized namedCurve",9],
  "genEcdhUsage": ["DOMException","SyntaxError","Unsupported key usage for an ECDH key",12],
  "genEdUsage": ["DOMException","SyntaxError","Unsupported key usage for an Ed25519 key",12],
  "genRsaUsage": ["DOMException","SyntaxError","Unsupported key usage for a RSA key",12],
  "genRsaExponent": ["DOMException","OperationError","The publicExponent must be equivalent to an unsigned 32-bit value",0],
  "genRsaExponentType": ["TypeError","TypeError","Failed to normalize algorithm: 'publicExponent' of 'RsaHashedKeyGenParams' (passed algorithm) is not an Uint8Array object.","ERR_INVALID_ARG_TYPE"],
  "genHmacZero": ["DOMException","DataError","Zero-length key is not supported",0],
  "notExtractable": ["DOMException","InvalidAccessException","key is not extractable",0],
  "gcmNoIv": ["TypeError","TypeError","Failed to normalize algorithm: passed algorithm can not be converted to 'AesGcmParams' because 'iv' is required in 'AesGcmParams'.","ERR_MISSING_OPTION"],
  "wrongUsage": ["DOMException","InvalidAccessError","The requested operation is not valid for the provided key",15],
  "wrongAlgorithm": ["DOMException","InvalidAccessError","The requested operation is not valid for the provided key",15],
  "cbcIv": ["DOMException","OperationError","algorithm.iv must contain exactly 16 bytes",0],
  "ctrLength": ["DOMException","OperationError","AES-CTR algorithm.length must be between 1 and 128",0],
  "ctrRange": ["TypeError","TypeError","Failed to normalize algorithm: 'length' of 'AesCtrParams' (passed algorithm) is outside the expected range of 0 to 255.","ERR_OUT_OF_RANGE"],
  "gcmTag": ["DOMException","OperationError","100 is not a valid AES-GCM tag length",0],
  "signWithAes": ["DOMException","NotSupportedError","Unrecognized algorithm name",9],
  "notAKey": ["TypeError","TypeError","Failed to execute 'encrypt' on 'SubtleCrypto': 2nd argument is not of type CryptoKey.","ERR_INVALID_ARG_TYPE"],
  "gcmAuth": ["DOMException","OperationError","The operation failed for an operation-specific reason",0],
  "gcmShort": ["DOMException","OperationError","The provided data is too small.",0],
  "cbcPadding": ["DOMException","OperationError","The operation failed for an operation-specific reason",0],
  "kwEncrypt": ["DOMException","NotSupportedError","Unrecognized algorithm name",9],
  "kwLength": ["DOMException","OperationError","The operation failed for an operation-specific reason",0],
  "kwIntegrity": ["DOMException","OperationError","The operation failed for an operation-specific reason",0],
  "hkdfOddLength": ["DOMException","OperationError","length must be a multiple of 8",0],
  "hkdfNoLength": ["DOMException","OperationError","length cannot be null",0],
  "hkdfTooLong": ["DOMException","OperationError","The operation failed for an operation-specific reason",0],
  "hkdfNoSalt": ["TypeError","TypeError","Failed to normalize algorithm: passed algorithm can not be converted to 'HkdfParams' because 'salt' is required in 'HkdfParams'.","ERR_MISSING_OPTION"],
  "hkdfDeriveKeyUsage": ["DOMException","InvalidAccessError","baseKey does not have deriveKey usage",15],
  "keyAlgorithmMismatch": ["DOMException","InvalidAccessError","Key algorithm mismatch",15],
  "pbkdf2Zero": ["DOMException","OperationError","iterations cannot be zero",0],
  "deriveKeyNoLength": ["TypeError","TypeError","Failed to normalize algorithm: passed algorithm can not be converted to 'AesDerivedKeyParams' because 'length' is required in 'AesDerivedKeyParams'.","ERR_MISSING_OPTION"],
  "deriveKeyEcdsa": ["DOMException","NotSupportedError","Unrecognized algorithm name",9],
  "digestName": ["DOMException","NotSupportedError","Unrecognized algorithm name",9],
  "exportHkdf": ["DOMException","NotSupportedError","HKDF key export is not supported",9],
  "exportFormat": ["DOMException","NotSupportedError","Unable to export HMAC secret key using spki format",9],
  "jwkKty": ["DOMException","DataError","Invalid JWK \"kty\" Parameter",0],
  "jwkNoKty": ["DOMException","DataError","Invalid keyData",0],
  "jwkUse": ["DOMException","DataError","Invalid JWK \"use\" Parameter",0],
  "jwkOps": ["DOMException","DataError","Key operations and usage mismatch",0],
  "jwkDuplicateOps": ["DOMException","DataError","Duplicate key operation",0],
  "jwkExt": ["DOMException","DataError","JWK \"ext\" Parameter and extractable mismatch",0],
  "jwkAlg": ["DOMException","DataError","JWK \"alg\" does not match the requested algorithm",0],
  "jwkNoK": ["DOMException","DataError","Invalid keyData",0],
  "hmacJwkAlg": ["DOMException","DataError","JWK \"alg\" does not match the requested algorithm",0],
  "spkiGarbage": ["DOMException","DataError","Invalid keyData",0],
  "spkiUsage": ["DOMException","SyntaxError","Unsupported key usage for an RSA-OAEP key",12],
  "rawEcPoint": ["DOMException","DataError","Invalid keyData",0],
  "rawRsa": ["DOMException","NotSupportedError","Unable to import RSA-OAEP using raw format",9],
  "rawEdLength": ["DOMException","DataError","Ed25519 raw keys must be exactly 32-bytes",0],
  "rawEdUsage": ["DOMException","SyntaxError","Unsupported key usage for a Ed25519 key",12],
  "okpCrv": ["DOMException","DataError","JWK \"crv\" Parameter and algorithm name mismatch",0],
  "okpAlg": ["DOMException","DataError","JWK \"alg\" does not match the requested algorithm",0],
  "okpNoX": ["DOMException","DataError","Invalid JWK",0],
  "okpShort": ["DOMException","DataError","Invalid keyData",0],
  "ecCrv": ["DOMException","DataError","JWK \"crv\" does not match the requested algorithm",0],
  "ecAlg": ["DOMException","DataError","JWK \"alg\" does not match the requested algorithm",0],
  "ecPoint": ["DOMException","DataError","Invalid keyData",0],
  "rsaJwkAlg": ["DOMException","DataError","JWK \"alg\" does not match the requested algorithm",0],
  "curveName": ["DOMException","NotSupportedError","Unrecognized namedCurve",9],
  "ed448Context": ["DOMException","NotSupportedError","Non zero-length context is not supported.",9],
  "ecdsaNoHash": ["TypeError","TypeError","Failed to normalize algorithm: passed algorithm can not be converted to 'EcdsaParams' because 'hash' is required in 'EcdsaParams'.","ERR_MISSING_OPTION"],
  "signWithPublic": ["DOMException","InvalidAccessError","Unable to use this key to sign",15],
  "verifyWithPrivate": ["DOMException","InvalidAccessError","Unable to use this key to verify",15],
  "verifyShortSignature": ["ok","boolean"],
  "exportPrivateRaw": ["DOMException","NotSupportedError","Unable to export Ed25519 private key using raw format",9],
  "exportPublicPkcs8": ["DOMException","NotSupportedError","Unable to export Ed25519 public key using pkcs8 format",9],
  "ecdhPublicType": ["DOMException","InvalidAccessError","algorithm.public must be a public key",15],
  "ecdhPublicAlgorithm": ["DOMException","InvalidAccessError","algorithm.public must be an X25519 key",15],
  "ecdhNoPublic": ["TypeError","TypeError","Failed to normalize algorithm: passed algorithm can not be converted to 'EcdhKeyDeriveParams' because 'public' is required in 'EcdhKeyDeriveParams'.","ERR_MISSING_OPTION"],
  "ecdhBaseUsage": ["DOMException","InvalidAccessError","baseKey does not have deriveBits usage",15],
  "ecdhTooLong": ["DOMException","OperationError","derived bit length is too small",0],
  "toCryptoKeyFamily": ["DOMException","NotSupportedError","Unrecognized algorithm name",9],
  "toCryptoKeyUsages": ["DOMException","SyntaxError","Usages cannot be empty when importing a secret key.",12],
  "toCryptoKeyLength": ["DOMException","DataError","Invalid key length",0],
  "toCryptoKeyCurve": ["DOMException","DataError","Named curve mismatch",0],
  "toCryptoKeyType": ["DOMException","DataError","Invalid key type",0],
  "createPrivateKey": ["TypeError","TypeError","The \"key\" argument must be of type string or an instance of ArrayBuffer, Buffer, TypedArray, or DataView. Received an instance of PrivateKeyObject","ERR_INVALID_ARG_TYPE"],
  "createPublicKeyFromPublic": ["TypeError","TypeError","Invalid key object type public, expected private.","ERR_CRYPTO_INVALID_KEY_OBJECT_TYPE"],
  "getter": ["TypeError","TypeError","Value of \"this\" must be of type CryptoKey","ERR_INVALID_THIS"]
}"##;

#[tokio::test]
async fn errors_match_node() {
    let expected: serde_json::Value = serde_json::from_str(ERRORS_EXPECTED).unwrap();
    common::assert_same_keys(&run(ERRORS_SCRIPT).await, &expected);
}

const INTEROP_SCRIPT: &str = r##"
const data = Buffer.from("payload");
// pi-js defines lazy globals as accessors until their first read.
const ownGlobal = (name) => globalThis[name] && Object.values(Object.getOwnPropertyDescriptor(globalThis, name)).map((value) => typeof value);
out.globals = [["CryptoKey", "SubtleCrypto", "Crypto"].map(ownGlobal), crypto.webcrypto === globalThis.crypto, crypto.subtle === globalThis.crypto.subtle,
  crypto.webcrypto.CryptoKey, Object.prototype.toString.call(globalThis.crypto), Object.prototype.toString.call(subtle)];
out.shape = [Object.keys(SubtleCrypto.prototype), Object.keys(Crypto.prototype), Object.keys(CryptoKey.prototype),
  subtle.deriveBits.length, subtle.generateKey.length, subtle.unwrapKey.length, typeof crypto.getRandomValues(new Uint8Array(4)).length];
const hmacKey = await subtle.importKey("raw", Buffer.alloc(16, 1), { name: "HMAC", hash: "SHA-256" }, false, ["sign"]);
out.key = [String(hmacKey), Object.keys(hmacKey), hmacKey.algorithm === hmacKey.algorithm, hmacKey instanceof CryptoKey, require("util").inspect(hmacKey, { depth: 0 })];
const ko = crypto.KeyObject.from(hmacKey);
out.keyObject = [ko.type, ko.symmetricKeySize, crypto.createHmac("sha256", hmacKey).update(data).digest("hex"),
  Buffer.from(await subtle.sign("HMAC", hmacKey, data)).toString("hex")];
const secret = crypto.createSecretKey(Buffer.alloc(32, 2));
const fromSecret = [secret.toCryptoKey({ name: "HMAC", hash: "SHA-256" }, true, ["sign", "sign"]), secret.toCryptoKey("AES-KW", false, ["wrapKey"]), secret.toCryptoKey("PBKDF2", false, ["deriveBits"])];
out.toCryptoKey = fromSecret.map((key) => [key.type, key.algorithm, key.usages, key.extractable, crypto.KeyObject.from(key) === secret]);
const edKo = crypto.createPrivateKey({ key: der("ed25519"), format: "der", type: "pkcs8" });
const edPriv = edKo.toCryptoKey("Ed25519", false, ["sign"]);
const edPub = crypto.createPublicKey(edKo).toCryptoKey({ name: "Ed25519" }, true, ["verify"]);
const rsaPriv = crypto.createPrivateKey({ key: der("rsa"), format: "der", type: "pkcs8" }).toCryptoKey({ name: "RSA-PSS", hash: "SHA-1" }, false, ["sign"]);
out.toCryptoKeyPair = [edPriv.algorithm, edPub.usages, await subtle.verify("Ed25519", edPub, await subtle.sign("Ed25519", edPriv, data), data), rsaPriv.algorithm];
out.nodeApis = [crypto.createPublicKey(edPriv).asymmetricKeyType, crypto.sign(null, data, edPriv).toString("hex") === crypto.sign(null, data, edKo).toString("hex"),
  crypto.verify(null, data, edPub, crypto.sign(null, data, edKo)), crypto.createCipheriv("aes-256-ecb", fromSecret[1], null).update(Buffer.alloc(16)).toString("hex")];
return out;
"##;

const INTEROP_EXPECTED: &str = r##"{
  "globals": [[["function","boolean","boolean","boolean"],["function","boolean","boolean","boolean"],["function","boolean","boolean","boolean"]],true,true,null,"[object Crypto]","[object SubtleCrypto]"],
  "shape": [["encrypt","decrypt","sign","verify","digest","generateKey","deriveKey","deriveBits","importKey","exportKey","wrapKey","unwrapKey"],["subtle","getRandomValues","randomUUID"],["type","extractable","algorithm","usages"],2,3,7,"number"],
  "key": ["[object CryptoKey]",[],true,true,"CryptoKey [Object]"],
  "keyObject": ["secret",16,"4dff6098fe8565c8c3002086ebd85641ab64158a73d9c85707a62f24fd1350e2","4dff6098fe8565c8c3002086ebd85641ab64158a73d9c85707a62f24fd1350e2"],
  "toCryptoKey": [["secret",{"name":"HMAC","hash":{"name":"SHA-256"},"length":256},["sign","sign"],true,true],["secret",{"name":"AES-KW","length":256},["wrapKey"],false,true],["secret",{"name":"PBKDF2"},["deriveBits"],false,true]],
  "toCryptoKeyPair": [{"name":"Ed25519"},["verify"],true,{"name":"RSA-PSS","modulusLength":1024,"publicExponent":{"0":1,"1":0,"2":1},"hash":{"name":"SHA-1"}}],
  "nodeApis": ["ed25519",true,true,"f435a111a3e4a1944919f912c5a241de"]
}"##;

#[tokio::test]
async fn crypto_keys_and_globals_match_node() {
    let expected: serde_json::Value = serde_json::from_str(INTEROP_EXPECTED).unwrap();
    common::assert_same_keys(&run(INTEROP_SCRIPT).await, &expected);
}
