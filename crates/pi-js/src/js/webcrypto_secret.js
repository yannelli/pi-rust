// SubtleCrypto algorithms over secret keys: HMAC, AES (CBC, CTR, GCM, KW), HKDF, PBKDF2, and digests.
// Ported from Node's lib/internal/crypto/{mac,aes,hkdf,pbkdf2,hash}.js (see LICENSE-node). AES-KW
// follows RFC 3394 over AES-ECB.
const { Buffer } = require("buffer");
const keys = require("internal/crypto_keys");
const { hkdfSync } = require("internal/crypto_extra");
const idl = require("internal/webcrypto_idl");
const { bytes, checkJwk, checkJwkType, createKey, domError, hasAnyNotIn, nodeHash, operationError, toArrayBuffer } = idl;
const { kKeyObject } = keys.helpers;
const nativeCrypto = native.crypto;

function digest(algorithm, data) {
  idl.validateMaxBufferLength(data, "data");
  const id = nativeCrypto.hash(nodeHash(algorithm.name));
  nativeCrypto.update(id, bytes(data));
  return toArrayBuffer(nativeCrypto.digest(id));
}

function generateSecret(type, length) {
  try {
    return keys.generateKeySync(type, { length });
  } catch (error) {
    throw operationError(error);
  }
}

function jwkSecret(keyData) {
  if (typeof keyData.k !== "string") throw domError("Invalid keyData", "DataError");
  return new keys.SecretKeyObject(Buffer.from(keyData.k, "base64").toString("latin1"));
}

function rawSecret(keyData) {
  return new keys.SecretKeyObject(bytes(keyData));
}

const BLOCK_BITS = { "SHA-1": 512, "SHA-256": 512, "SHA-384": 1024, "SHA-512": 1024 };

function hmacUsages(keyUsages) {
  const usages = new Set(keyUsages);
  if (hasAnyNotIn(usages, ["sign", "verify"])) throw domError("Unsupported key usage for an HMAC key", "SyntaxError");
  return usages;
}

function hmacGenerateKey({ name, hash, length = BLOCK_BITS[hash.name] }, extractable, keyUsages) {
  const usages = hmacUsages(keyUsages);
  return createKey(generateSecret("hmac", length), { name, length, hash: { name: hash.name } }, [...usages], extractable);
}

function hmacImportKey(format, keyData, algorithm, extractable, keyUsages) {
  const usages = hmacUsages(keyUsages);
  let keyObject;
  if (format === "KeyObject") keyObject = keyData;
  else if (format === "raw") keyObject = rawSecret(keyData);
  else if (format === "jwk") {
    checkJwkType(keyData, "oct");
    checkJwk(keyData, usages, "sig", extractable);
    if (keyData.alg !== undefined && keyData.alg !== idl.jwkHashAlg("HMAC", algorithm.hash.name)) throw idl.jwkAlgMismatch();
    keyObject = jwkSecret(keyData);
  } else return undefined;
  const length = keyObject.symmetricKeySize * 8;
  if (length === 0) throw domError("Zero-length key is not supported", "DataError");
  if (algorithm.length !== undefined && algorithm.length !== length) throw domError("Invalid key length", "DataError");
  return createKey(keyObject, { name: "HMAC", hash: algorithm.hash, length }, keyUsages, extractable);
}

function hmac(key, data) {
  const id = nativeCrypto.hmac(nodeHash(key.algorithm.hash.name), key[kKeyObject]._bytes);
  nativeCrypto.update(id, data);
  return internal.bytesToLatin1(nativeCrypto.digest(id));
}

function hmacSignVerify(key, data, signature) {
  const mac = hmac(key, data);
  if (signature === undefined) return toArrayBuffer(mac);
  let difference = mac.length ^ signature.length;
  for (let i = 0; i < mac.length; i++) difference |= mac.charCodeAt(i) ^ signature.charCodeAt(i);
  return difference === 0;
}

function aesUsages(name, keyUsages) {
  const usages = new Set(keyUsages);
  const allowed = name === "AES-KW" ? ["wrapKey", "unwrapKey"] : ["wrapKey", "unwrapKey", "encrypt", "decrypt"];
  if (hasAnyNotIn(usages, allowed)) throw domError("Unsupported key usage for an AES key", "SyntaxError");
  return usages;
}

function checkAesLength(length) {
  if (length !== 128 && length !== 192 && length !== 256) throw domError("Invalid key length", "DataError");
}

function aesGenerateKey({ name, length }, extractable, keyUsages) {
  const usages = aesUsages(name, keyUsages);
  return createKey(generateSecret("aes", length), { name, length }, [...usages], extractable);
}

function aesJwkAlg(name, length) {
  return `A${length}${name.slice(4)}`;
}

function aesImportKey(algorithm, format, keyData, extractable, keyUsages) {
  const { name } = algorithm;
  const usages = aesUsages(name, keyUsages);
  let keyObject;
  if (format === "KeyObject") {
    checkAesLength(keyData.symmetricKeySize * 8);
    keyObject = keyData;
  } else if (format === "raw") {
    checkAesLength(keyData.byteLength * 8);
    keyObject = rawSecret(keyData);
  } else if (format === "jwk") {
    checkJwkType(keyData, "oct");
    checkJwk(keyData, usages, "enc", extractable);
    keyObject = jwkSecret(keyData);
    checkAesLength(keyObject.symmetricKeySize * 8);
    if (keyData.alg !== undefined && keyData.alg !== aesJwkAlg(name, keyObject.symmetricKeySize * 8)) throw idl.jwkAlgMismatch();
  } else return undefined;
  return createKey(keyObject, { name, length: keyObject.symmetricKeySize * 8 }, keyUsages, extractable);
}

// One native cipher pass over latin1 `data`; returns the output and, when encrypting with AEAD, the tag.
function runCipher(name, key, iv, decrypt, data, options = {}) {
  const id = nativeCrypto.cipherNew(name, key, iv, decrypt, options.tagLength);
  try {
    if (options.aad !== undefined) nativeCrypto.cipherAad(id, options.aad);
    if (options.tag !== undefined) nativeCrypto.cipherTag(id, options.tag);
    if (options.padding === false) nativeCrypto.cipherPadding(id, false);
    const head = internal.bytesToLatin1(nativeCrypto.cipherUpdate(id, data));
    const [tail, tag] = nativeCrypto.cipherFinal(id);
    return [head + internal.bytesToLatin1(tail), internal.bytesToLatin1(tag)];
  } finally {
    nativeCrypto.drop(id);
  }
}

// AES-CTR with a `length`-bit counter: like Node, a counter that wraps restarts at zero, and
// more blocks than the counter can count fail.
function aesCtr(name, key, data, { counter, length }) {
  const block = Buffer.from(bytes(counter), "latin1");
  const value = BigInt(`0x${block.toString("hex")}`);
  const mask = (1n << BigInt(length)) - 1n;
  const blocks = BigInt(Math.ceil(data.length / 16));
  if (blocks > mask + 1n) throw operationError();
  const untilReset = mask + 1n - (value & mask);
  if (blocks <= untilReset) return runCipher(name, key, bytes(counter), false, data)[0];
  const split = Number(untilReset) * 16;
  const reset = Buffer.from((value & ~mask).toString(16).padStart(32, "0"), "hex").toString("latin1");
  return runCipher(name, key, bytes(counter), false, data.slice(0, split))[0] + runCipher(name, key, reset, false, data.slice(split))[0];
}

function aesGcm(encrypt, name, key, data, { iv, additionalData, tagLength = 128 }) {
  const tagBytes = tagLength / 8;
  const options = { tagLength: tagBytes, aad: additionalData === undefined ? undefined : bytes(additionalData) };
  if (encrypt) {
    const [output, tag] = runCipher(name, key, bytes(iv), false, data, options);
    return output + tag;
  }
  if (data.length < tagBytes) throw domError("The provided data is too small.", "OperationError");
  options.tag = data.slice(data.length - tagBytes);
  return runCipher(name, key, bytes(iv), true, data.slice(0, data.length - tagBytes), options)[0];
}

const KW_IV = "\xa6".repeat(8);

// XOR the 64-bit big-endian step counter `t` into the 8-byte `a`.
function xorStep(a, t) {
  let out = "";
  for (let i = 7; i >= 0; i--) {
    out = String.fromCharCode(a.charCodeAt(i) ^ (t % 256)) + out;
    t = Math.floor(t / 256);
  }
  return out;
}

// RFC 3394 key wrap; `data` is the key to wrap, or the wrapped key with its 8-byte check value.
function aesKw(encrypt, name, key, data) {
  if (data.length % 8 !== 0 || data.length < (encrypt ? 16 : 24)) throw operationError();
  const id = nativeCrypto.cipherNew(name, key, "", !encrypt, undefined);
  try {
    nativeCrypto.cipherPadding(id, false);
    const aes = (input) => internal.bytesToLatin1(nativeCrypto.cipherUpdate(id, input));
    const start = encrypt ? 0 : 8;
    const n = (data.length - start) / 8;
    const r = Array.from({ length: n }, (_, i) => data.slice(start + i * 8, start + i * 8 + 8));
    let a = encrypt ? KW_IV : data.slice(0, 8);
    for (let step = 0; step < 6 * n; step++) {
      const j = encrypt ? Math.floor(step / n) : 5 - Math.floor(step / n);
      const i = encrypt ? step % n : n - 1 - (step % n);
      const t = n * j + i + 1;
      const b = encrypt ? aes(a + r[i]) : aes(xorStep(a, t) + r[i]);
      a = encrypt ? xorStep(b.slice(0, 8), t) : b.slice(0, 8);
      r[i] = b.slice(8);
    }
    if (encrypt) return a + r.join("");
    if (a !== KW_IV) throw operationError();
    return r.join("");
  } finally {
    nativeCrypto.drop(id);
  }
}

function aesCipher(encrypt, key, data, algorithm) {
  const secret = key[kKeyObject]._bytes;
  const mode = algorithm.name.slice(4).toLowerCase();
  const name = `aes-${key.algorithm.length}-${mode === "kw" ? "ecb" : mode}`;
  let output;
  try {
    if (mode === "cbc") output = runCipher(name, secret, bytes(algorithm.iv), !encrypt, data)[0];
    else if (mode === "ctr") output = aesCtr(name, secret, data, algorithm);
    else if (mode === "gcm") output = aesGcm(encrypt, name, secret, data, algorithm);
    else output = aesKw(encrypt, name, secret, data);
  } catch (error) {
    throw error instanceof idl.DOMException ? error : operationError(error);
  }
  return toArrayBuffer(output);
}

function importGenericSecretKey({ name }, format, keyData, extractable, keyUsages) {
  if (extractable) throw domError(`${name} keys are not extractable`, "SyntaxError");
  if (hasAnyNotIn(new Set(keyUsages), ["deriveKey", "deriveBits"])) throw domError(`Unsupported key usage for a ${name} key`, "SyntaxError");
  let keyObject;
  if (format === "KeyObject") keyObject = keyData;
  else if (format === "raw") keyObject = rawSecret(keyData);
  else return undefined;
  return createKey(keyObject, { name }, keyUsages, false);
}

function checkDeriveLength(length) {
  if (length === null) throw domError("length cannot be null", "OperationError");
  if (length % 8) throw domError("length must be a multiple of 8", "OperationError");
}

function hkdfDeriveBits({ hash, salt, info }, baseKey, length) {
  checkDeriveLength(length);
  if (length === 0) return new ArrayBuffer(0);
  try {
    return hkdfSync(nodeHash(hash.name), baseKey[kKeyObject], salt, info, length / 8);
  } catch (error) {
    throw operationError(error);
  }
}

function pbkdf2DeriveBits({ hash, salt, iterations }, baseKey, length) {
  checkDeriveLength(length);
  if (length === 0) return new ArrayBuffer(0);
  try {
    if (iterations > 2 ** 31 - 1) throw keys.helpers.outOfRange("iterations", `>= 1 && <= ${2 ** 31 - 1}`, iterations);
    return toArrayBuffer(nativeCrypto.pbkdf2(baseKey[kKeyObject]._bytes, bytes(salt), iterations, length / 8, nodeHash(hash.name)));
  } catch (error) {
    throw operationError(error);
  }
}

module.exports = {
  aesCipher,
  aesGenerateKey,
  aesImportKey,
  aesJwkAlg,
  digest,
  hkdfDeriveBits,
  hmacGenerateKey,
  hmacImportKey,
  hmacSignVerify,
  importGenericSecretKey,
  pbkdf2DeriveBits,
};
