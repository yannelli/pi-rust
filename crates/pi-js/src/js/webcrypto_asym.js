// SubtleCrypto algorithms over key pairs: RSASSA-PKCS1-v1_5, RSA-PSS, RSA-OAEP, ECDSA, ECDH,
// Ed25519, Ed448, X25519, and X448. Ported from Node's lib/internal/crypto/{rsa,ec,cfrg,diffiehellman}.js
// (see LICENSE-node).
const { Buffer } = require("buffer");
const keys = require("internal/crypto_keys");
const idl = require("internal/webcrypto_idl");
const { NAMED_CURVES, bytes, checkJwk, checkJwkType, createKey, domError, hasAnyNotIn, nodeHash, operationError, toArrayBuffer } = idl;
const { kKeyObject } = keys.helpers;
const nativeCrypto = native.crypto;

const SIGN = [["verify"], ["sign"]];
const DERIVE = [[], ["deriveKey", "deriveBits"]];
// Per algorithm: the key family, and the usages its public and private keys accept.
const ALGORITHMS = {
  "RSASSA-PKCS1-v1_5": ["rsa", SIGN],
  "RSA-PSS": ["rsa", SIGN],
  "RSA-OAEP": ["rsa", [["encrypt", "wrapKey"], ["decrypt", "unwrapKey"]]],
  ECDSA: ["ec", SIGN],
  ECDH: ["ec", DERIVE],
  Ed25519: ["cfrg", SIGN],
  Ed448: ["cfrg", SIGN],
  X25519: ["cfrg", DERIVE],
  X448: ["cfrg", DERIVE],
};
const RAW_SIZES = { Ed25519: 32, X25519: 32, Ed448: 57, X448: 56 };
const DIGEST_BYTES = { "SHA-1": 20, "SHA-256": 32, "SHA-384": 48, "SHA-512": 64 };

function checkUsages(name, isPublic, usages) {
  const [family, uses] = ALGORITHMS[name];
  if (hasAnyNotIn(usages, uses[isPublic ? 0 : 1])) {
    throw domError(`Unsupported key usage for ${family === "rsa" ? "an" : "a"} ${name} key`, "SyntaxError");
  }
}

function dataError(cause) {
  return domError("Invalid keyData", { name: "DataError", cause });
}

function parsed(create) {
  try {
    return create();
  } catch (error) {
    throw dataError(error);
  }
}

// The unsigned 32-bit value of a big-endian byte array, or undefined when it is larger.
function toUint32(input) {
  let result = 0;
  for (let n = 0; n < input.length; ++n) {
    const shift = input.length - n - 1;
    if (shift >= 4 && input[n]) return undefined;
    result |= input[n] << (8 * shift);
  }
  return result;
}

function generateKey(algorithm, extractable, keyUsages) {
  const { name } = algorithm;
  const [family, [publicUses, privateUses]] = ALGORITHMS[name];
  let type = name.toLowerCase();
  let options;
  let keyAlgorithm = { name };
  if (family === "rsa") {
    const publicExponent = toUint32(algorithm.publicExponent);
    if (publicExponent === undefined) throw domError("The publicExponent must be equivalent to an unsigned 32-bit value", "OperationError");
    type = "rsa";
    options = { modulusLength: algorithm.modulusLength, publicExponent };
    keyAlgorithm = { name, modulusLength: algorithm.modulusLength, publicExponent: algorithm.publicExponent, hash: { name: algorithm.hash.name } };
  } else if (family === "ec") {
    type = "ec";
    options = { namedCurve: algorithm.namedCurve };
    keyAlgorithm = { name, namedCurve: algorithm.namedCurve };
  }
  const usages = new Set(keyUsages);
  if (hasAnyNotIn(usages, [...publicUses, ...privateUses])) {
    throw domError(`Unsupported key usage for ${family === "rsa" ? "a RSA" : `an ${name}`} key`, "SyntaxError");
  }
  let pair;
  try {
    pair = keys.generateKeyPairSync(type, options);
  } catch (error) {
    throw operationError(error);
  }
  const publicKey = createKey(pair.publicKey, keyAlgorithm, idl.usagesUnion(usages, publicUses), true);
  const privateKey = createKey(pair.privateKey, keyAlgorithm, idl.usagesUnion(usages, privateUses), extractable);
  return family === "cfrg" ? { __proto__: null, privateKey, publicKey } : { __proto__: null, publicKey, privateKey };
}

function okpKey(name, keyData, isPublic) {
  if (keyData.byteLength !== RAW_SIZES[name]) throw domError(`${name} raw keys must be exactly ${RAW_SIZES[name]}-bytes`, "DataError");
  const b64 = Buffer.from(bytes(keyData), "latin1").toString("base64url");
  const jwk = isPublic ? { kty: "OKP", crv: name, x: b64 } : { kty: "OKP", crv: name, x: "", d: b64 };
  return parsed(() => (isPublic ? keys.createPublicKey : keys.createPrivateKey)({ key: jwk, format: "jwk" }));
}

function ecRawKey(namedCurve, keyData) {
  return parsed(() => {
    const point = Buffer.from(nativeCrypto.ecConvert(NAMED_CURVES[namedCurve], bytes(keyData), 4));
    const size = (point.length - 1) / 2;
    const [x, y] = [point.subarray(1, 1 + size), point.subarray(1 + size)].map((part) => part.toString("base64url"));
    return keys.createPublicKey({ key: { kty: "EC", crv: namedCurve, x, y }, format: "jwk" });
  });
}

function jwkKey(keyData, extra) {
  const jwk = { ...keyData, ...extra };
  return parsed(() => (jwk.d === undefined ? keys.createPublicKey : keys.createPrivateKey)({ key: jwk, format: "jwk" }));
}

function importJwk(keyData, algorithm, usages, extractable) {
  const { name } = algorithm;
  const family = ALGORITHMS[name][0];
  checkJwkType(keyData, { rsa: "RSA", ec: "EC", cfrg: "OKP" }[family]);
  const isPublic = keyData.d === undefined;
  const use = ALGORITHMS[name][1] === SIGN ? "sig" : "enc";
  if (family === "rsa") {
    checkUsages(name, isPublic, usages);
    checkJwk(keyData, usages, use, extractable);
    if (keyData.alg !== undefined && keyData.alg !== idl.jwkHashAlg(name, algorithm.hash.name)) throw idl.jwkAlgMismatch();
    return jwkKey(keyData);
  }
  if (family === "ec") {
    if (keyData.crv !== algorithm.namedCurve) throw domError('JWK "crv" does not match the requested algorithm', "DataError");
    checkUsages(name, isPublic, usages);
    checkJwk(keyData, usages, use, extractable);
    const curves = { ES256: "P-256", ES384: "P-384", ES512: "P-521" };
    if (name === "ECDSA" && keyData.alg !== undefined && curves[keyData.alg] !== algorithm.namedCurve) throw idl.jwkAlgMismatch();
    const keyObject = jwkKey(keyData);
    if (!isPublic) {
      const { x, y } = keys.createPublicKey(keyObject).export({ format: "jwk" });
      const same = (a, b) => typeof b === "string" && Buffer.from(a, "base64").equals(Buffer.from(b, "base64"));
      if (!same(x, keyData.x) || !same(y, keyData.y)) throw dataError();
    }
    return keyObject;
  }
  if (keyData.crv !== name) throw domError('JWK "crv" Parameter and algorithm name mismatch', "DataError");
  checkJwk(keyData, usages, use, extractable);
  if (keyData.alg !== undefined && use === "sig" && keyData.alg !== name && keyData.alg !== "EdDSA") throw idl.jwkAlgMismatch();
  if (!isPublic && typeof keyData.x !== "string") throw domError("Invalid JWK", "DataError");
  checkUsages(name, isPublic, usages);
  try {
    const publicKey = okpKey(name, Buffer.from(keyData.x, "base64"), true);
    if (isPublic) return publicKey;
    const privateKey = okpKey(name, Buffer.from(keyData.d, "base64"), false);
    if (!keys.createPublicKey(privateKey).equals(publicKey)) throw keys.helpers.codeError(TypeError, "ERR_CRYPTO_INVALID_JWK", "Invalid JWK data");
    return privateKey;
  } catch (error) {
    throw dataError(error);
  }
}

function importKey(format, keyData, algorithm, extractable, keyUsages) {
  const { name } = algorithm;
  const family = ALGORITHMS[name][0];
  const usages = new Set(keyUsages);
  let keyObject;
  if (format === "KeyObject") {
    checkUsages(name, keyData.type === "public", usages);
    keyObject = keyData;
  } else if (format === "spki") {
    checkUsages(name, true, usages);
    keyObject = parsed(() => keys.createPublicKey({ key: keyData, format: "der", type: "spki" }));
  } else if (format === "pkcs8") {
    checkUsages(name, false, usages);
    keyObject = parsed(() => keys.createPrivateKey({ key: keyData, format: "der", type: "pkcs8" }));
  } else if (format === "jwk") {
    keyObject = importJwk(keyData, algorithm, usages, extractable);
  } else if (format === "raw" && family !== "rsa") {
    checkUsages(name, true, usages);
    keyObject = family === "ec" ? ecRawKey(algorithm.namedCurve, keyData) : okpKey(name, keyData, true);
  } else {
    return undefined;
  }
  const expectedType = family === "cfrg" ? name.toLowerCase() : family;
  if (keyObject.asymmetricKeyType !== expectedType) throw domError("Invalid key type", "DataError");
  const details = keyObject.asymmetricKeyDetails;
  if (family === "ec") {
    if (NAMED_CURVES[algorithm.namedCurve] !== details.namedCurve) throw domError("Named curve mismatch", "DataError");
    return createKey(keyObject, { name, namedCurve: algorithm.namedCurve }, keyUsages, extractable);
  }
  if (family === "cfrg") return createKey(keyObject, { name }, keyUsages, extractable);
  let hex = details.publicExponent.toString(16);
  if (hex.length % 2) hex = `0${hex}`;
  const keyAlgorithm = { name, modulusLength: details.modulusLength, publicExponent: new Uint8Array(Buffer.from(hex, "hex")), hash: algorithm.hash };
  return createKey(keyObject, keyAlgorithm, keyUsages, extractable);
}

// spki, pkcs8, or raw public key data; undefined for a format the key does not have.
function exportKey(key, format) {
  const keyObject = key[kKeyObject];
  if (format !== "raw") return toArrayBuffer(Buffer.from(keyObject._der, "latin1"));
  const family = ALGORITHMS[key.algorithm.name][0];
  if (family === "rsa") return undefined;
  const { x, y } = keyObject.export({ format: "jwk" });
  const parts = family === "ec" ? [Buffer.from([4]), Buffer.from(x, "base64url"), Buffer.from(y, "base64url")] : [Buffer.from(x, "base64url")];
  return toArrayBuffer(Buffer.concat(parts));
}

function exportJwk(key, parameters) {
  const { name } = key.algorithm;
  if (ALGORITHMS[name][0] === "rsa") parameters.alg = idl.jwkHashAlg(name, key.algorithm.hash.name);
  else if (name === "Ed25519" || name === "Ed448") parameters.alg = name;
  return Object.assign(parameters, key[kKeyObject].export({ format: "jwk" }));
}

function signVerify(key, data, algorithm, signature) {
  const type = signature === undefined ? "private" : "public";
  if (key.type !== type) throw domError(`Key must be a ${type} key`, "InvalidAccessError");
  const { name, hash, modulusLength } = key.algorithm;
  let options = { hash: "" };
  try {
    if (name === "RSASSA-PKCS1-v1_5") options = { hash: nodeHash(hash.name), padding: 1 };
    else if (name === "RSA-PSS") {
      const max = Math.ceil((modulusLength - 1) / 8) - DIGEST_BYTES[hash.name] - 2;
      keys.helpers.validateInteger(algorithm.saltLength, "algorithm.saltLength", 0, max);
      options = { hash: nodeHash(hash.name), padding: 6, saltLength: algorithm.saltLength };
    } else if (name === "ECDSA") options = { hash: nodeHash(algorithm.hash.name), dsaEncoding: "ieee-p1363" };
    const der = key[kKeyObject]._der;
    if (signature === undefined) return toArrayBuffer(nativeCrypto.sign(true, der, data, JSON.stringify(options)));
    return nativeCrypto.verify(false, der, data, signature, JSON.stringify(options));
  } catch (error) {
    throw operationError(error);
  }
}

function rsaCipher(encrypt, key, data, algorithm) {
  if (algorithm.label !== undefined) idl.validateMaxBufferLength(algorithm.label, "algorithm.label");
  if (key.type !== (encrypt ? "public" : "private")) {
    throw domError("The requested operation is not valid for the provided key", "InvalidAccessError");
  }
  const params = { padding: 4, oaepHash: nodeHash(key.algorithm.hash.name), oaepLabel: algorithm.label === undefined ? "" : bytes(algorithm.label) };
  try {
    const op = encrypt ? "publicEncrypt" : "privateDecrypt";
    return toArrayBuffer(nativeCrypto.rsaCrypt(op, !encrypt, key[kKeyObject]._der, data, JSON.stringify(params)));
  } catch (error) {
    throw operationError(error);
  }
}

function ecdhDeriveBits(algorithm, baseKey, length) {
  const key = algorithm.public;
  if (baseKey.type !== "private") throw domError("baseKey must be a private key", "InvalidAccessError");
  if (key.algorithm.name !== baseKey.algorithm.name) throw domError("The public and private keys must be of the same type", "InvalidAccessError");
  if (key.algorithm.name === "ECDH" && key.algorithm.namedCurve !== baseKey.algorithm.namedCurve) {
    throw domError("Named curve mismatch", "InvalidAccessError");
  }
  let bits;
  try {
    bits = nativeCrypto.keyAgree(baseKey[kKeyObject]._der, key[kKeyObject]._der);
  } catch (error) {
    throw operationError(error);
  }
  if (length === null) return toArrayBuffer(bits);
  const sliceLength = Math.ceil(length / 8);
  if (bits.byteLength < sliceLength) throw domError("derived bit length is too small", "OperationError");
  const slice = bits.slice(0, sliceLength);
  if (length % 8) slice[sliceLength - 1] &= 0xff00 >> (length % 8);
  return toArrayBuffer(slice);
}

module.exports = { ecdhDeriveBits, exportJwk, exportKey, generateKey, importKey, rsaCipher, signVerify };
