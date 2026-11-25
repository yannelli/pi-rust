// SubtleCrypto helpers: WebIDL conversion, algorithm normalization, and the CryptoKey class.
// Ported from Node's lib/internal/crypto/{webidl,util,keys}.js and lib/internal/webidl.js (see LICENSE-node).
const { Buffer } = require("buffer");
const { DOMException } = require("internal/web");
const { codeError, kKeyObject } = require("internal/crypto_keys").helpers;
const customInspect = Symbol.for("nodejs.util.inspect.custom");

function domError(message, options) {
  return new DOMException(message, options);
}

// The error Node gives when a crypto job fails.
function operationError(cause) {
  return domError("The operation failed for an operation-specific reason", { name: "OperationError", cause });
}

function invalidThis(type) {
  return codeError(TypeError, "ERR_INVALID_THIS", `Value of "this" must be of type ${type}`);
}

function illegalConstructor() {
  return codeError(TypeError, "ERR_ILLEGAL_CONSTRUCTOR", "Illegal constructor");
}

function makeException(message, opts = {}) {
  const prefix = opts.prefix ? `${opts.prefix}: ` : "";
  const context = opts.context?.length === 0 ? "" : `${opts.context ?? "Value"} `;
  return codeError(TypeError, opts.code || "ERR_INVALID_ARG_TYPE", `${prefix}${context}${message}`);
}

function isObject(value) {
  return value !== null && (typeof value === "object" || typeof value === "function");
}

function toNumber(value, opts) {
  if (typeof value === "bigint") throw makeException("is a BigInt and cannot be converted to a number.", opts);
  if (typeof value === "symbol") throw makeException("is a Symbol and cannot be converted to a number.", opts);
  return Number(value);
}

function integer(bits) {
  const upper = 2 ** bits - 1;
  return (value, opts = {}) => {
    let x = toNumber(value, opts);
    if (opts.enforceRange) {
      if (!Number.isFinite(x)) throw makeException("is not a finite number.", opts);
      x = Math.trunc(x);
      if (x < 0 || x > upper) throw makeException(`is outside the expected range of 0 to ${upper}.`, { ...opts, code: "ERR_OUT_OF_RANGE" });
      return x;
    }
    if (!Number.isFinite(x) || x === 0) return 0;
    x = Math.trunc(x);
    return x >= 0 && x <= upper ? x : x % 2 ** bits;
  };
}

const enforced = (convert) => (value, opts) => convert(value, { ...opts, enforceRange: true });
const isShared = (buffer) => typeof SharedArrayBuffer !== "undefined" && buffer instanceof SharedArrayBuffer;

const converters = {
  boolean: (value) => !!value,
  octet: integer(8),
  "unsigned short": integer(16),
  "unsigned long": integer(32),
  DOMString(value, opts = {}) {
    if (typeof value === "symbol") throw makeException("is a Symbol and cannot be converted to a string.", opts);
    return String(value);
  },
  object(value, opts) {
    if (!isObject(value)) throw makeException("is not an object.", opts);
    return value;
  },
  Uint8Array(value, opts = {}) {
    if (!ArrayBuffer.isView(value) || value[Symbol.toStringTag] !== "Uint8Array") throw makeException("is not an Uint8Array object.", opts);
    if (isShared(value.buffer)) throw makeException("is a view on a SharedArrayBuffer, which is not allowed.", opts);
    return value;
  },
  BufferSource(value, opts = {}) {
    if (ArrayBuffer.isView(value)) {
      if (isShared(value.buffer)) throw makeException("is a view on a SharedArrayBuffer, which is not allowed.", opts);
      return value;
    }
    if (!(value instanceof ArrayBuffer)) throw makeException("is not instance of ArrayBuffer, Buffer, TypedArray, or DataView.", opts);
    return value;
  },
};

function enumConverter(name, values) {
  return (value, opts = {}) => {
    const text = String(value);
    if (!values.includes(text)) throw makeException(`value '${text}' is not a valid enum value of type ${name}.`, { ...opts, code: "ERR_INVALID_ARG_VALUE" });
    return text;
  };
}

function sequenceConverter(convert) {
  return (value, opts = {}) => {
    const iterator = isObject(value) ? value[Symbol.iterator]?.() : undefined;
    if (iterator === undefined) throw makeException("can not be converted to sequence.", opts);
    const array = [];
    for (;;) {
      const result = iterator?.next?.();
      if (result === undefined) throw makeException("can not be converted to sequence.", opts);
      if (result.done === true) return array;
      array.push(convert(result.value, { ...opts, context: `${opts.context}, index ${array.length}` }));
    }
  };
}

function requiredArguments(length, required, opts) {
  if (length < required) {
    const message = `${required} argument${required === 1 ? "" : "s"} required, but only ${length} present.`;
    throw makeException(message, { ...opts, context: "", code: "ERR_MISSING_ARGS" });
  }
}

const member = (key, converter, required = false, validator = undefined) => ({ key, converter, required, validator });

function dictionary(name, ...groups) {
  const members = groups.flat().sort((a, b) => (a.key < b.key ? -1 : a.key > b.key ? 1 : 0));
  const hasRequired = members.some((entry) => entry.required);
  return (value, opts = {}) => {
    if (value !== undefined && value !== null && !isObject(value)) throw makeException("can not be converted to a dictionary", opts);
    const result = {};
    if (value == null && !hasRequired) return result;
    for (const { key, converter, required, validator } of members) {
      const entry = value == null ? undefined : value[key];
      if (entry !== undefined) {
        const converted = converter(entry, { ...opts, context: `'${key}' of '${name}'${opts.context ? ` (${opts.context})` : ""}` });
        if (validator) validator(converted, value);
        result[key] = converted;
      } else if (required) {
        throw makeException(`can not be converted to '${name}' because '${key}' is required in '${name}'.`, { ...opts, code: "ERR_MISSING_OPTION" });
      }
    }
    return result;
  };
}

const NAMED_CURVES = { "P-256": "prime256v1", "P-384": "secp384r1", "P-521": "secp521r1" };
const USAGES = ["encrypt", "decrypt", "sign", "verify", "deriveKey", "deriveBits", "wrapKey", "unwrapKey"];
const MAX_BUFFER = 2 ** 31 - 1;

function validateMaxBufferLength(data, name) {
  if (data.byteLength > MAX_BUFFER) throw domError(`${name} must be less than ${MAX_BUFFER + 1} bits`, "OperationError");
}

const byteLength = (name, target) => (value) => {
  if (value.byteLength !== target) throw domError(`${name} must contain exactly ${target} bytes`, "OperationError");
};
const maxLength = (name) => (value) => validateMaxBufferLength(value, name);

function aesLength(value) {
  if (value !== 128 && value !== 192 && value !== 256) throw domError("AES key length must be 128, 192, or 256 bits", "OperationError");
}

function hmacLength(value) {
  if (value === 0) throw domError("Zero-length key is not supported", "DataError");
  if (value % 8) throw domError("Unsupported algorithm.length", "NotSupportedError");
}

function namedCurve(value) {
  if (!Object.prototype.hasOwnProperty.call(NAMED_CURVES, value)) throw domError("Unrecognized namedCurve", "NotSupportedError");
}

function gcmTagLength(value) {
  if (![32, 64, 96, 104, 112, 120, 128].includes(value)) throw domError(`${value} is not a valid AES-GCM tag length`, "OperationError");
}

function ctrLength(value) {
  if (value === 0 || value > 128) throw domError("AES-CTR algorithm.length must be between 1 and 128", "OperationError");
}

function ecdhPublic(value, dict) {
  if (value.type !== "public") throw domError("algorithm.public must be a public key", "InvalidAccessError");
  if (value.algorithm.name.toUpperCase() !== dict.name.toUpperCase()) {
    throw domError(`algorithm.public must be an ${dict.name.toUpperCase()} key`, "InvalidAccessError");
  }
}

function ed448Context(value) {
  if (value.byteLength) throw domError("Non zero-length context is not supported.", "NotSupportedError");
}

converters.AlgorithmIdentifier = (value, opts) => (isObject(value) ? converters.object(value, opts) : converters.DOMString(value, opts));
converters.HashAlgorithmIdentifier = converters.AlgorithmIdentifier;
converters.BigInteger = converters.Uint8Array;
converters.KeyFormat = enumConverter("KeyFormat", ["raw", "pkcs8", "spki", "jwk"]);
converters.KeyUsage = enumConverter("KeyUsage", USAGES);
converters["sequence<KeyUsage>"] = sequenceConverter(converters.KeyUsage);
converters["sequence<DOMString>"] = sequenceConverter(converters.DOMString);
converters.CryptoKey = (value, opts) => {
  if (!CryptoKey.prototype.isPrototypeOf(value)) throw makeException("is not of type CryptoKey.", opts);
  return value;
};

const algorithmMembers = [member("name", converters.DOMString, true)];
const hash = member("hash", converters.HashAlgorithmIdentifier, true);
const ulong = enforced(converters["unsigned long"]);
const ushort = enforced(converters["unsigned short"]);
const octet = enforced(converters.octet);
const bufferSource = converters.BufferSource;
const curve = member("namedCurve", converters.DOMString, true, namedCurve);
const DICTIONARIES = {
  RsaHashedKeyGenParams: [member("modulusLength", ulong, true), member("publicExponent", converters.BigInteger, true), hash],
  RsaHashedImportParams: [hash],
  EcKeyImportParams: [curve],
  EcKeyGenParams: [curve],
  AesKeyGenParams: [member("length", ushort, true, aesLength)],
  HmacKeyGenParams: [hash, member("length", ulong, false, hmacLength)],
  RsaPssParams: [member("saltLength", ulong, true)],
  RsaOaepParams: [member("label", bufferSource)],
  EcdsaParams: [hash],
  HmacImportParams: [hash, member("length", ulong, false, hmacLength)],
  HkdfParams: [hash, member("salt", bufferSource, true), member("info", bufferSource, true)],
  Pbkdf2Params: [hash, member("salt", bufferSource, true), member("iterations", ulong, true, (value) => {
    if (value === 0) throw domError("iterations cannot be zero", "OperationError");
  })],
  AesDerivedKeyParams: [member("length", ushort, true, aesLength)],
  AesCbcParams: [member("iv", bufferSource, true, byteLength("algorithm.iv", 16))],
  AesGcmParams: [
    member("iv", bufferSource, true, maxLength("algorithm.iv")),
    member("tagLength", octet, false, gcmTagLength),
    member("additionalData", bufferSource, false, maxLength("algorithm.additionalData")),
  ],
  AesCtrParams: [member("counter", bufferSource, true, byteLength("algorithm.counter", 16)), member("length", octet, true, ctrLength)],
  EcdhKeyDeriveParams: [member("public", converters.CryptoKey, true, ecdhPublic)],
  Ed448Params: [member("context", bufferSource, false, ed448Context)],
};
converters.Algorithm = dictionary("Algorithm", algorithmMembers);
for (const [name, members] of Object.entries(DICTIONARIES)) converters[name] = dictionary(name, algorithmMembers, members);
const domStrings = (...keys) => keys.map((key) => member(key, converters.DOMString));
converters.RsaOtherPrimesInfo = dictionary("RsaOtherPrimesInfo", domStrings("r", "d", "t"));
converters.JsonWebKey = dictionary("JsonWebKey", domStrings("kty", "use", "alg", "crv", "x", "y", "d", "n", "e", "p", "q", "dp", "dq", "qi", "k"), [
  member("key_ops", converters["sequence<DOMString>"]),
  member("ext", converters.boolean),
  member("oth", sequenceConverter(converters.RsaOtherPrimesInfo)),
]);

// The parameter dictionary of each algorithm per operation; null means the name alone.
const RSA_NAMES = ["RSASSA-PKCS1-v1_5", "RSA-PSS", "RSA-OAEP"];
const AES_NAMES = ["AES-CTR", "AES-CBC", "AES-GCM", "AES-KW"];
const table = (names, type) => Object.fromEntries(names.map((name) => [name, type]));
const SUPPORTED = {
  digest: table(["SHA-1", "SHA-256", "SHA-384", "SHA-512"], null),
  generateKey: {
    ...table(RSA_NAMES, "RsaHashedKeyGenParams"),
    ...table(["ECDSA", "ECDH"], "EcKeyGenParams"),
    ...table(AES_NAMES, "AesKeyGenParams"),
    HMAC: "HmacKeyGenParams",
    ...table(["Ed25519", "X25519", "X448", "Ed448"], null),
  },
  exportKey: table([...RSA_NAMES, "ECDSA", "ECDH", "HMAC", ...AES_NAMES, "Ed25519", "X25519", "X448", "Ed448"], null),
  sign: { "RSASSA-PKCS1-v1_5": null, "RSA-PSS": "RsaPssParams", ECDSA: "EcdsaParams", HMAC: null, Ed25519: null, Ed448: "Ed448Params" },
  importKey: {
    ...table(RSA_NAMES, "RsaHashedImportParams"),
    ...table(["ECDSA", "ECDH"], "EcKeyImportParams"),
    HMAC: "HmacImportParams",
    ...table(["HKDF", "PBKDF2", ...AES_NAMES, "Ed25519", "X25519", "X448", "Ed448"], null),
  },
  deriveBits: { HKDF: "HkdfParams", PBKDF2: "Pbkdf2Params", ...table(["ECDH", "X25519", "X448"], "EcdhKeyDeriveParams") },
  encrypt: { "RSA-OAEP": "RsaOaepParams", "AES-CBC": "AesCbcParams", "AES-GCM": "AesGcmParams", "AES-CTR": "AesCtrParams" },
  "get key length": { ...table(["AES-CBC", "AES-CTR", "AES-GCM", "AES-KW"], "AesDerivedKeyParams"), HMAC: "HmacImportParams", HKDF: null, PBKDF2: null },
  wrapKey: { "AES-KW": null },
};
SUPPORTED.verify = SUPPORTED.sign;
SUPPORTED.decrypt = SUPPORTED.encrypt;
SUPPORTED.unwrapKey = SUPPORTED.wrapKey;

// Members that normalization copies (BufferSource) or normalizes as a digest (hash).
const NORMALIZED_MEMBERS = {
  AesGcmParams: ["iv", "additionalData"],
  HkdfParams: ["hash", "salt", "info"],
  Ed448Params: ["context"],
  Pbkdf2Params: ["hash", "salt"],
  RsaOaepParams: ["label"],
};
for (const type of ["RsaHashedKeyGenParams", "HmacKeyGenParams", "EcdsaParams", "HmacImportParams", "RsaHashedImportParams"]) {
  NORMALIZED_MEMBERS[type] = ["hash"];
}

const EXPERIMENTAL = ["Ed448", "X448"];
const warned = new Set();

function copyBytes(source) {
  const view = ArrayBuffer.isView(source) ? new Uint8Array(source.buffer, source.byteOffset, source.byteLength) : new Uint8Array(source);
  return view.slice();
}

function normalizeAlgorithm(algorithm, op) {
  if (typeof algorithm === "string") return normalizeAlgorithm({ name: algorithm }, op);
  const registered = SUPPORTED[op];
  const opts = { prefix: "Failed to normalize algorithm", context: "passed algorithm" };
  let { name } = converters.Algorithm(algorithm, opts);
  let desiredType;
  for (const key of Object.keys(registered)) {
    if (key.toUpperCase() === name.toUpperCase()) [name, desiredType] = [key, registered[key]];
  }
  if (desiredType === undefined) throw domError("Unrecognized algorithm name", "NotSupportedError");
  if (EXPERIMENTAL.includes(name) && !warned.has(name)) {
    warned.add(name);
    process.emitWarning(`The ${name} Web Crypto API algorithm is an experimental feature and might change at any time`, "ExperimentalWarning");
  }
  if (desiredType === null) return { name };
  const normalized = converters[desiredType](algorithm, opts);
  normalized.name = name;
  for (const key of NORMALIZED_MEMBERS[desiredType] || []) {
    if (key === "hash") normalized.hash = normalizeAlgorithm(normalized.hash, "digest");
    else if (normalized[key]) normalized[key] = copyBytes(normalized[key]);
  }
  return normalized;
}

// "SHA-256" in Node's naming ("sha256") and as the JWK "alg" of RSA and HMAC keys.
function nodeHash(name) {
  return name.replace("-", "").toLowerCase();
}

function jwkHashAlg(algorithm, hashName) {
  const bits = hashName.slice(4);
  if (algorithm === "RSA-OAEP") return bits === "1" ? "RSA-OAEP" : `RSA-OAEP-${bits}`;
  return { "RSASSA-PKCS1-v1_5": "RS", "RSA-PSS": "PS", HMAC: "HS" }[algorithm] + bits;
}

function bytes(source) {
  return internal.bytesToLatin1(source);
}

function toArrayBuffer(data) {
  const view = typeof data === "string" ? Buffer.from(data, "latin1") : data;
  return view.buffer.slice(view.byteOffset, view.byteOffset + view.byteLength);
}

function hasAnyNotIn(set, checks) {
  for (const value of set) if (!checks.includes(value)) return true;
  return false;
}

function usagesUnion(set, usages) {
  return usages.filter((usage) => set.has(usage));
}

function checkJwkType(keyData, kty) {
  if (!keyData.kty) throw domError("Invalid keyData", "DataError");
  if (keyData.kty !== kty) throw domError('Invalid JWK "kty" Parameter', "DataError");
}

// The "use", "key_ops", and "ext" checks of every JWK import.
function checkJwk(keyData, usages, use, extractable) {
  if (usages.size > 0 && keyData.use !== undefined && keyData.use !== use) throw domError('Invalid JWK "use" Parameter', "DataError");
  if (keyData.key_ops !== undefined) {
    const seen = new Set();
    for (const op of keyData.key_ops) {
      if (!USAGES.includes(op)) continue;
      if (seen.has(op)) throw domError("Duplicate key operation", "DataError");
      seen.add(op);
    }
    for (const usage of usages) if (!keyData.key_ops.includes(usage)) throw domError("Key operations and usage mismatch", "DataError");
  }
  if (keyData.ext === false && extractable === true) throw domError('JWK "ext" Parameter and extractable mismatch', "DataError");
}

function jwkAlgMismatch() {
  return domError('JWK "alg" does not match the requested algorithm', "DataError");
}

const kAlgorithm = Symbol("kAlgorithm");
const kExtractable = Symbol("kExtractable");
const kKeyUsages = Symbol("kKeyUsages");

function checkKey(key) {
  if (!(key instanceof CryptoKey)) throw invalidThis("CryptoKey");
  return key;
}

class CryptoKey {
  constructor() {
    throw illegalConstructor();
  }
  [customInspect](depth, options, inspect) {
    if (depth < 0) return this;
    const opts = { ...options, depth: options.depth == null ? null : options.depth - 1 };
    return `CryptoKey ${inspect({ type: this.type, extractable: this.extractable, algorithm: this.algorithm, usages: this.usages }, opts)}`;
  }
  get type() {
    return checkKey(this)[kKeyObject].type;
  }
  get extractable() {
    return checkKey(this)[kExtractable];
  }
  get algorithm() {
    return checkKey(this)[kAlgorithm];
  }
  get usages() {
    return checkKey(this)[kKeyUsages];
  }
}
Object.defineProperties(CryptoKey.prototype, {
  type: { enumerable: true },
  extractable: { enumerable: true },
  algorithm: { enumerable: true },
  usages: { enumerable: true },
  [Symbol.toStringTag]: { configurable: true, value: "CryptoKey" },
});

function createKey(keyObject, algorithm, usages, extractable) {
  return Object.create(CryptoKey.prototype, {
    [kKeyObject]: { value: keyObject },
    [kAlgorithm]: { value: algorithm },
    [kExtractable]: { value: extractable },
    [kKeyUsages]: { value: usages },
  });
}

module.exports = {
  CryptoKey,
  DOMException,
  NAMED_CURVES,
  bytes,
  checkJwk,
  checkJwkType,
  converters,
  createKey,
  domError,
  hasAnyNotIn,
  illegalConstructor,
  invalidThis,
  jwkAlgMismatch,
  jwkHashAlg,
  nodeHash,
  normalizeAlgorithm,
  operationError,
  requiredArguments,
  toArrayBuffer,
  usagesUnion,
  validateMaxBufferLength,
};
