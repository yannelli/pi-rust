// Web Crypto: the Crypto and SubtleCrypto classes and the KeyObject to CryptoKey conversion.
// Ported from Node's lib/internal/crypto/webcrypto.js (see LICENSE-node).
const { TextDecoder, TextEncoder } = require("internal/web");
const idl = require("internal/webcrypto_idl");
const secret = require("internal/webcrypto_secret");
const asym = require("internal/webcrypto_asym");
const { kKeyObject } = require("internal/crypto_keys").helpers;
const { CryptoKey, converters, domError, normalizeAlgorithm } = idl;

const ORDINALS = ["1st", "2nd", "3rd", "4th", "5th", "6th", "7th"];
const AES_NAMES = ["AES-CTR", "AES-CBC", "AES-GCM", "AES-KW"];
const SECRET_NAMES = ["HMAC", ...AES_NAMES, "HKDF", "PBKDF2"];

// Checks `this` and the argument count, then returns a converter for each argument by index.
function prepare(self, method, args, required) {
  if (self !== subtle) throw idl.invalidThis("SubtleCrypto");
  const prefix = `Failed to execute '${method}' on 'SubtleCrypto'`;
  idl.requiredArguments(args.length, required, { prefix });
  return (type, index) => converters[type](args[index], { prefix, context: `${ORDINALS[index]} argument` });
}

async function digest(algorithm, data) {
  const arg = prepare(this, "digest", arguments, 2);
  algorithm = arg("AlgorithmIdentifier", 0);
  data = arg("BufferSource", 1);
  return secret.digest(normalizeAlgorithm(algorithm, "digest"), data);
}

async function generateKey(algorithm, extractable, keyUsages) {
  const arg = prepare(this, "generateKey", arguments, 3);
  algorithm = arg("AlgorithmIdentifier", 0);
  extractable = arg("boolean", 1);
  keyUsages = arg("sequence<KeyUsage>", 2);
  algorithm = normalizeAlgorithm(algorithm, "generateKey");
  let result;
  if (algorithm.name === "HMAC") result = secret.hmacGenerateKey(algorithm, extractable, keyUsages);
  else if (AES_NAMES.includes(algorithm.name)) result = secret.aesGenerateKey(algorithm, extractable, keyUsages);
  else result = asym.generateKey(algorithm, extractable, keyUsages);
  if ((result instanceof CryptoKey ? result : result.privateKey).usages.length === 0) {
    throw domError("Usages cannot be empty when creating a key.", "SyntaxError");
  }
  return result;
}

function deriveWith(algorithm, baseKey, length) {
  if (algorithm.name === "HKDF") return secret.hkdfDeriveBits(algorithm, baseKey, length);
  if (algorithm.name === "PBKDF2") return secret.pbkdf2DeriveBits(algorithm, baseKey, length);
  return asym.ecdhDeriveBits(algorithm, baseKey, length);
}

function checkBaseKey(baseKey, algorithm, usage) {
  if (!baseKey.usages.includes(usage)) throw domError(`baseKey does not have ${usage} usage`, "InvalidAccessError");
  if (baseKey.algorithm.name !== algorithm.name) throw domError("Key algorithm mismatch", "InvalidAccessError");
}

async function deriveBits(algorithm, baseKey, length = null) {
  const arg = prepare(this, "deriveBits", arguments, 2);
  algorithm = arg("AlgorithmIdentifier", 0);
  baseKey = arg("CryptoKey", 1);
  if (length !== null) length = arg("unsigned long", 2);
  algorithm = normalizeAlgorithm(algorithm, "deriveBits");
  checkBaseKey(baseKey, algorithm, "deriveBits");
  return deriveWith(algorithm, baseKey, length);
}

function getKeyLength({ name, length, hash }) {
  if (AES_NAMES.includes(name)) return length;
  if (name !== "HMAC") return null;
  if (length === undefined) return { "SHA-1": 512, "SHA-256": 512, "SHA-384": 1024, "SHA-512": 1024 }[hash?.name];
  if (typeof length === "number" && length !== 0) return length;
  throw domError("Invalid key length", "OperationError");
}

async function deriveKey(algorithm, baseKey, derivedKeyAlgorithm, extractable, keyUsages) {
  const arg = prepare(this, "deriveKey", arguments, 5);
  algorithm = arg("AlgorithmIdentifier", 0);
  baseKey = arg("CryptoKey", 1);
  derivedKeyAlgorithm = arg("AlgorithmIdentifier", 2);
  extractable = arg("boolean", 3);
  keyUsages = arg("sequence<KeyUsage>", 4);
  algorithm = normalizeAlgorithm(algorithm, "deriveBits");
  derivedKeyAlgorithm = normalizeAlgorithm(derivedKeyAlgorithm, "importKey");
  checkBaseKey(baseKey, algorithm, "deriveKey");
  const length = getKeyLength(normalizeAlgorithm(arguments[2], "get key length"));
  const bits = deriveWith(algorithm, baseKey, length);
  return importKey.call(this, "raw", bits, derivedKeyAlgorithm, extractable, keyUsages);
}

function exportKeyData(format, key) {
  const { name } = key.algorithm;
  const isSecret = SECRET_NAMES.includes(name);
  if (format === "jwk") {
    const parameters = { key_ops: key.usages, ext: key.extractable };
    if (!isSecret) return asym.exportJwk(key, parameters);
    parameters.alg = name === "HMAC" ? idl.jwkHashAlg(name, key.algorithm.hash.name) : secret.aesJwkAlg(name, key.algorithm.length);
    return Object.assign(parameters, key[kKeyObject].export({ format: "jwk" }));
  }
  if (format === "raw" && key.type === "secret") return idl.toArrayBuffer(key[kKeyObject]._bytes);
  const allowed = { spki: "public", pkcs8: "private", raw: "public" }[format];
  return key.type === allowed ? asym.exportKey(key, format) : undefined;
}

async function exportKey(format, key) {
  const arg = prepare(this, "exportKey", arguments, 2);
  format = arg("KeyFormat", 0);
  key = arg("CryptoKey", 1);
  try {
    normalizeAlgorithm(key.algorithm, "exportKey");
  } catch {
    throw domError(`${key.algorithm.name} key export is not supported`, "NotSupportedError");
  }
  if (!key.extractable) throw domError("key is not extractable", "InvalidAccessException");
  const result = exportKeyData(format, key);
  if (!result) throw domError(`Unable to export ${key.algorithm.name} ${key.type} key using ${format} format`, "NotSupportedError");
  return result;
}

function importWith(format, keyData, algorithm, extractable, keyUsages) {
  const { name } = algorithm;
  if (name === "HMAC") return secret.hmacImportKey(format, keyData, algorithm, extractable, keyUsages);
  if (AES_NAMES.includes(name)) return secret.aesImportKey(algorithm, format, keyData, extractable, keyUsages);
  if (name === "HKDF" || name === "PBKDF2") return secret.importGenericSecretKey(algorithm, format, keyData, extractable, keyUsages);
  return asym.importKey(format, keyData, algorithm, extractable, keyUsages);
}

async function importKey(format, keyData, algorithm, extractable, keyUsages) {
  const arg = prepare(this, "importKey", arguments, 4);
  format = arg("KeyFormat", 0);
  keyData = arg(format === "jwk" ? "JsonWebKey" : "BufferSource", 1);
  algorithm = arg("AlgorithmIdentifier", 2);
  extractable = arg("boolean", 3);
  keyUsages = arg("sequence<KeyUsage>", 4);
  algorithm = normalizeAlgorithm(algorithm, "importKey");
  const result = importWith(format, keyData, algorithm, extractable, keyUsages);
  if (!result) throw domError(`Unable to import ${algorithm.name} using ${format} format`, "NotSupportedError");
  if ((result.type === "secret" || result.type === "private") && result.usages.length === 0) {
    throw domError(`Usages cannot be empty when importing a ${result.type} key.`, "SyntaxError");
  }
  return result;
}

// KeyObject.prototype.toCryptoKey.
function toCryptoKey(keyObject, algorithm, extractable, keyUsages) {
  algorithm = normalizeAlgorithm(converters.AlgorithmIdentifier(algorithm), "importKey");
  extractable = converters.boolean(extractable);
  keyUsages = converters["sequence<KeyUsage>"](keyUsages);
  const isSecret = keyObject.type === "secret";
  if (isSecret !== SECRET_NAMES.includes(algorithm.name)) throw domError("Unrecognized algorithm name", "NotSupportedError");
  const result = importWith("KeyObject", keyObject, algorithm, extractable, keyUsages);
  if ((isSecret || result.type === "private") && result.usages.length === 0) {
    throw domError(`Usages cannot be empty when importing a ${result.type} key.`, "SyntaxError");
  }
  return result;
}

function normalizeOr(algorithm, op, fallback) {
  try {
    return normalizeAlgorithm(algorithm, op);
  } catch {
    return normalizeAlgorithm(algorithm, fallback);
  }
}

function cipherOrWrap(encrypt, algorithm, key, data, op) {
  if (key.algorithm.name !== algorithm.name || !key.usages.includes(op)) {
    throw domError("The requested operation is not valid for the provided key", "InvalidAccessError");
  }
  idl.validateMaxBufferLength(data, "data");
  const { name } = algorithm;
  if (name === "RSA-OAEP") return asym.rsaCipher(encrypt, key, idl.bytes(data), algorithm);
  if (AES_NAMES.includes(name) && (name !== "AES-KW" || op === "wrapKey" || op === "unwrapKey")) {
    return secret.aesCipher(encrypt, key, idl.bytes(data), algorithm);
  }
  throw domError("Unrecognized algorithm name", "NotSupportedError");
}

async function wrapKey(format, key, wrappingKey, algorithm) {
  const arg = prepare(this, "wrapKey", arguments, 4);
  format = arg("KeyFormat", 0);
  key = arg("CryptoKey", 1);
  wrappingKey = arg("CryptoKey", 2);
  algorithm = normalizeOr(arg("AlgorithmIdentifier", 3), "wrapKey", "encrypt");
  let keyData = await exportKey.call(this, format, key);
  if (format === "jwk") {
    const raw = JSON.stringify(keyData);
    // AES-KW needs a multiple of 8 bytes, so the JSON text is padded with spaces.
    keyData = new TextEncoder().encode(algorithm.name === "AES-KW" && raw.length % 8 !== 0 ? raw + " ".repeat(8 - (raw.length % 8)) : raw);
  }
  return cipherOrWrap(true, algorithm, wrappingKey, keyData, "wrapKey");
}

async function unwrapKey(format, wrappedKey, unwrappingKey, unwrapAlgo, unwrappedKeyAlgo, extractable, keyUsages) {
  const arg = prepare(this, "unwrapKey", arguments, 7);
  format = arg("KeyFormat", 0);
  wrappedKey = arg("BufferSource", 1);
  unwrappingKey = arg("CryptoKey", 2);
  unwrapAlgo = arg("AlgorithmIdentifier", 3);
  unwrappedKeyAlgo = arg("AlgorithmIdentifier", 4);
  extractable = arg("boolean", 5);
  keyUsages = arg("sequence<KeyUsage>", 6);
  unwrapAlgo = normalizeOr(unwrapAlgo, "unwrapKey", "decrypt");
  let keyData = cipherOrWrap(false, unwrapAlgo, unwrappingKey, wrappedKey, "unwrapKey");
  if (format === "jwk") {
    try {
      keyData = JSON.parse(new TextDecoder("utf-8", { fatal: true }).decode(keyData));
    } catch {
      throw domError("Invalid wrapped JWK key", "DataError");
    }
  }
  return importKey.call(this, format, keyData, unwrappedKeyAlgo, extractable, keyUsages);
}

function signVerify(algorithm, key, data, signature) {
  const usage = signature === undefined ? "sign" : "verify";
  algorithm = normalizeAlgorithm(algorithm, usage);
  if (!key.usages.includes(usage) || algorithm.name !== key.algorithm.name) {
    throw domError(`Unable to use this key to ${usage}`, "InvalidAccessError");
  }
  const signatureBytes = signature === undefined ? undefined : idl.bytes(signature);
  if (algorithm.name === "HMAC") return secret.hmacSignVerify(key, idl.bytes(data), signatureBytes);
  return asym.signVerify(key, idl.bytes(data), algorithm, signatureBytes);
}

async function sign(algorithm, key, data) {
  const arg = prepare(this, "sign", arguments, 3);
  return signVerify(arg("AlgorithmIdentifier", 0), arg("CryptoKey", 1), arg("BufferSource", 2));
}

async function verify(algorithm, key, signature, data) {
  const arg = prepare(this, "verify", arguments, 4);
  algorithm = arg("AlgorithmIdentifier", 0);
  key = arg("CryptoKey", 1);
  signature = arg("BufferSource", 2);
  return signVerify(algorithm, key, arg("BufferSource", 3), signature);
}

async function encrypt(algorithm, key, data) {
  const arg = prepare(this, "encrypt", arguments, 3);
  algorithm = arg("AlgorithmIdentifier", 0);
  key = arg("CryptoKey", 1);
  data = arg("BufferSource", 2);
  return cipherOrWrap(true, normalizeAlgorithm(algorithm, "encrypt"), key, data, "encrypt");
}

async function decrypt(algorithm, key, data) {
  const arg = prepare(this, "decrypt", arguments, 3);
  algorithm = arg("AlgorithmIdentifier", 0);
  key = arg("CryptoKey", 1);
  data = arg("BufferSource", 2);
  return cipherOrWrap(false, normalizeAlgorithm(algorithm, "decrypt"), key, data, "decrypt");
}

class SubtleCrypto {
  constructor() {
    throw idl.illegalConstructor();
  }
}
const subtle = Reflect.construct(function SubtleCryptoInstance() {}, [], SubtleCrypto);

class Crypto {
  constructor() {
    throw idl.illegalConstructor();
  }
  get subtle() {
    if (this !== crypto) throw idl.invalidThis("Crypto");
    return subtle;
  }
}
const crypto = Reflect.construct(function CryptoInstance() {}, [], Crypto);

// The random functions live in node:crypto, which loads this module first.
function getRandomValues(array) {
  if (this !== crypto) throw idl.invalidThis("Crypto");
  idl.requiredArguments(arguments.length, 1, { prefix: "Failed to execute 'getRandomValues' on 'Crypto'" });
  return require("crypto").getRandomValues(array);
}

function randomUUID() {
  if (this !== crypto) throw idl.invalidThis("Crypto");
  return require("crypto").randomUUID();
}

function defineMethods(prototype, tag, methods) {
  Object.defineProperty(prototype, Symbol.toStringTag, { configurable: true, value: tag });
  for (const method of methods) Object.defineProperty(prototype, method.name, { value: method, enumerable: true, writable: true, configurable: true });
}
Object.defineProperty(Crypto.prototype, "subtle", { enumerable: true });
defineMethods(Crypto.prototype, "Crypto", [getRandomValues, randomUUID]);
defineMethods(SubtleCrypto.prototype, "SubtleCrypto", [encrypt, decrypt, sign, verify, digest, generateKey, deriveKey, deriveBits, importKey, exportKey, wrapKey, unwrapKey]);

module.exports = { Crypto, CryptoKey, SubtleCrypto, crypto, toCryptoKey };
