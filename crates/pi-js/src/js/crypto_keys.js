// node:crypto key objects, key generation, signatures, RSA encryption, and diffieHellman.
const { Buffer } = require("buffer");
const { Writable } = require("stream");
const nativeCrypto = native.crypto;

function codeError(Ctor, code, message) {
  return Object.assign(new Ctor(message), { code });
}

function describe(value) {
  if (value === null || value === undefined) return String(value);
  if (typeof value === "function") return `function ${value.name}`;
  if (typeof value === "object") return value.constructor && value.constructor.name ? `an instance of ${value.constructor.name}` : "an instance of Object";
  const shown = typeof value === "string" ? `'${value.length > 28 ? value.slice(0, 25) + "..." : value}'` : String(value) + (typeof value === "bigint" ? "n" : "");
  return `type ${typeof value} (${shown})`;
}

function invalidArg(name, expected, value) {
  const kind = name.includes(".") ? "property" : "argument";
  return codeError(TypeError, "ERR_INVALID_ARG_TYPE", `The "${name}" ${kind} must be ${expected}. Received ${describe(value)}`);
}

function invalidValue(name, value, reason = "is invalid") {
  const kind = name.includes(".") ? "property" : "argument";
  const shown = typeof value === "string" ? `'${value}'` : typeof value === "object" && value !== null ? "[object]" : String(value);
  return codeError(TypeError, "ERR_INVALID_ARG_VALUE", `The ${kind} '${name}' ${reason}. Received ${shown}`);
}

function outOfRange(name, range, value) {
  return codeError(RangeError, "ERR_OUT_OF_RANGE", `The value of "${name}" is out of range. It must be ${range}. Received ${value}`);
}

function validateObject(value, name) {
  if (value === null || typeof value !== "object" || Array.isArray(value)) throw invalidArg(name, "of type object", value);
}

function validateString(value, name) {
  if (typeof value !== "string") throw invalidArg(name, "of type string", value);
}

function validateFunction(value, name) {
  if (typeof value !== "function") throw invalidArg(name, "of type function", value);
}

function validateInteger(value, name, min = Number.MIN_SAFE_INTEGER, max = Number.MAX_SAFE_INTEGER) {
  if (typeof value !== "number") throw invalidArg(name, "of type number", value);
  if (!Number.isInteger(value)) throw outOfRange(name, "an integer", value);
  if (value < min || value > max) throw outOfRange(name, `>= ${min} && <= ${max}`, value);
}

function isBytes(value) {
  return ArrayBuffer.isView(value) || value instanceof ArrayBuffer || (typeof SharedArrayBuffer !== "undefined" && value instanceof SharedArrayBuffer);
}

const BYTES = "of type string or an instance of ArrayBuffer, Buffer, TypedArray, or DataView";

// Bytes of a string (in `encoding`) or binary value, as a latin1 string for the natives.
function latin1(data, encoding, name = "data") {
  if (typeof data === "string") return internal.bytesToLatin1(Buffer.from(data, encoding || "utf8"));
  const bytes = internal.bytesToLatin1(data);
  if (bytes === undefined) throw invalidArg(name, BYTES, data);
  return bytes;
}

function toBuffer(bytes) {
  return Buffer.from(bytes.buffer, bytes.byteOffset, bytes.byteLength);
}

function encodeOut(bytes, encoding) {
  const buffer = toBuffer(bytes);
  return encoding && encoding !== "buffer" ? buffer.toString(encoding) : buffer;
}

function bigToLatin1(value) {
  if (value < 0n) throw outOfRange("candidate", ">= 0", `${value}n`);
  let hex = value.toString(16);
  if (hex.length % 2) hex = "0" + hex;
  return value === 0n ? "\0" : internal.bytesToLatin1(Buffer.from(hex, "hex"));
}

function latin1ToBig(bytes) {
  return bytes.length === 0 ? 0n : BigInt("0x" + Buffer.from(bytes, "latin1").toString("hex"));
}

// Runs `work` now, or on the next tick with a Node-style callback.
function maybeAsync(work, callback) {
  if (callback === undefined) return work();
  validateFunction(callback, "callback");
  let result;
  try {
    result = work();
  } catch (error) {
    process.nextTick(callback, error);
    return undefined;
  }
  process.nextTick(callback, null, result);
  return undefined;
}

function invalidKeyObjectType(type, expected) {
  return codeError(TypeError, "ERR_CRYPTO_INVALID_KEY_OBJECT_TYPE", `Invalid key object type ${type}, expected ${expected}.`);
}

class KeyObject {
  constructor(type, data) {
    if (type !== "secret" && type !== "public" && type !== "private") throw invalidValue("type", type);
    Object.defineProperty(this, "_type", { value: type });
    Object.defineProperty(this, type === "secret" ? "_bytes" : "_der", { value: data });
  }
  get type() {
    return this._type;
  }
  equals(other) {
    if (!(other instanceof KeyObject)) throw invalidArg("otherKeyObject", "an instance of KeyObject", other);
    return other.type === this.type && (this.type === "secret" ? other._bytes === this._bytes : other._der === this._der);
  }
}

class SecretKeyObject extends KeyObject {
  constructor(bytes) {
    super("secret", bytes);
  }
  get symmetricKeySize() {
    return this._bytes.length;
  }
  export(options) {
    if (options !== undefined) validateObject(options, "options");
    const format = options && options.format;
    if (format === "jwk") return { kty: "oct", k: Buffer.from(this._bytes, "latin1").toString("base64url") };
    if (format !== undefined && format !== "buffer") throw invalidValue("options.format", format);
    return Buffer.from(this._bytes, "latin1");
  }
}

class AsymmetricKeyObject extends KeyObject {
  _info() {
    if (!this._cachedInfo) Object.defineProperty(this, "_cachedInfo", { value: JSON.parse(nativeCrypto.keyInfo(this.type === "private", this._der)) });
    return this._cachedInfo;
  }
  get asymmetricKeyType() {
    return this._info().type;
  }
  get asymmetricKeyDetails() {
    const details = { ...this._info().details };
    if (details.publicExponent !== undefined) details.publicExponent = BigInt(details.publicExponent);
    return details;
  }
  export(options) {
    validateObject(options === undefined ? {} : options, "options");
    const { format, type, cipher } = options || {};
    const isPrivate = this.type === "private";
    if (format === "jwk") return JSON.parse(Buffer.from(nativeCrypto.keyExport(isPrivate, this._der, "jwk", "")).toString("latin1"));
    if (format !== "pem" && format !== "der") throw invalidValue("options.format", format);
    const types = isPrivate ? ["pkcs1", "pkcs8", "sec1"] : ["pkcs1", "spki"];
    if (!types.includes(type)) throw invalidValue("options.type", type);
    let passphrase;
    if (cipher !== undefined) {
      if (!isPrivate) throw codeError(TypeError, "ERR_CRYPTO_INCOMPATIBLE_KEY_OPTIONS", "The selected key encoding does not support encryption.");
      validateString(cipher, "options.cipher");
      if (options.passphrase === undefined) throw invalidValue("options.passphrase", options.passphrase);
      passphrase = latin1(options.passphrase, undefined, "options.passphrase");
    }
    const bytes = nativeCrypto.keyExport(isPrivate, this._der, format, type, cipher, passphrase);
    return format === "pem" ? toBuffer(bytes).toString("latin1") : toBuffer(bytes);
  }
}

class PublicKeyObject extends AsymmetricKeyObject {
  constructor(der) {
    super("public", der);
  }
}

class PrivateKeyObject extends AsymmetricKeyObject {
  constructor(der) {
    super("private", der);
  }
}

const KEY_INPUT = "of type string or an instance of ArrayBuffer, Buffer, TypedArray, DataView, KeyObject, or CryptoKey";

// Parse PEM, DER, or JWK input into canonical PKCS#8 (`kind` "private") or SPKI DER.
function parseKeyData(kind, data, options, name) {
  const format = options.format === undefined ? "pem" : options.format;
  if (format === "jwk") {
    validateObject(data, `${name}.key`);
    return internal.bytesToLatin1(nativeCrypto.keyParse(kind, "jwk", "", JSON.stringify(data)));
  }
  if (format !== "pem" && format !== "der") throw invalidValue(`${name}.format`, format);
  if (typeof data !== "string" && !isBytes(data)) throw invalidArg(name, KEY_INPUT, data);
  const type = options.type === undefined ? "" : options.type;
  const passphrase = options.passphrase === undefined || options.passphrase === null ? undefined : latin1(options.passphrase, undefined, `${name}.passphrase`);
  return internal.bytesToLatin1(nativeCrypto.keyParse(kind, format, type, latin1(data, options.encoding, name), passphrase));
}

function splitKeyInput(key, name) {
  if (key instanceof KeyObject) return { data: key, options: {} };
  if (key !== null && typeof key === "object" && !isBytes(key)) return { data: key.key, options: key };
  if (typeof key !== "string" && !isBytes(key)) throw invalidArg(name, KEY_INPUT, key);
  return { data: key, options: {} };
}

// Resolve a key argument to a KeyObject; "public" accepts private keys too, as Node does.
function asymmetricKey(input, kind, name = "key") {
  const { data, options } = splitKeyInput(input, name);
  if (data instanceof KeyObject) {
    if (data.type === "secret" || (kind === "private" && data.type !== "private")) throw invalidKeyObjectType(data.type, kind === "private" ? "private" : "public or private");
    return { key: data, options };
  }
  if (kind === "public") {
    try {
      return { key: new PrivateKeyObject(parseKeyData("private", data, options, name)), options };
    } catch (error) {
      if (error && (error.code === "ERR_MISSING_PASSPHRASE" || error.code === "ERR_OSSL_BAD_DECRYPT")) throw error;
      return { key: new PublicKeyObject(parseKeyData("public", data, options, name)), options };
    }
  }
  return { key: new PrivateKeyObject(parseKeyData("private", data, options, name)), options };
}

function publicOf(key) {
  return key.type === "public" ? key : new PublicKeyObject(internal.bytesToLatin1(nativeCrypto.keyParse("public", "der", "pkcs8", key._der)));
}

function createSecretKey(key, encoding) {
  if (typeof key !== "string" && !isBytes(key)) throw invalidArg("key", BYTES, key);
  return new SecretKeyObject(latin1(key, encoding, "key"));
}

function createPublicKey(key) {
  const { data, options } = splitKeyInput(key, "key");
  if (data instanceof KeyObject) {
    if (data.type !== "private") throw invalidKeyObjectType(data.type, "private");
    return publicOf(data);
  }
  return new PublicKeyObject(parseKeyData("public", data, options, "key"));
}

function createPrivateKey(key) {
  if (key instanceof KeyObject) throw invalidArg("key", "of type string or an instance of ArrayBuffer, Buffer, TypedArray, or DataView", key);
  const { data, options } = splitKeyInput(key, "key");
  return new PrivateKeyObject(parseKeyData("private", data, options, "key"));
}

const KEY_TYPES = ["rsa", "rsa-pss", "dsa", "ec", "ed25519", "ed448", "x25519", "x448", "dh"];

// Validate generateKeyPair arguments and return the work to run.
function prepareKeyPair(type, options) {
  validateString(type, "type");
  if (!KEY_TYPES.includes(type)) throw invalidValue("type", type, "must be a supported key type");
  if (options !== undefined) validateObject(options, "options");
  const opts = options || {};
  const params = {};
  if (type === "rsa" || type === "rsa-pss") {
    validateInteger(opts.modulusLength, "options.modulusLength", 0, 2 ** 32 - 1);
    params.modulusLength = opts.modulusLength;
    if (opts.publicExponent !== undefined) validateInteger(opts.publicExponent, "options.publicExponent", 0, 2 ** 32 - 1);
    params.publicExponent = opts.publicExponent;
  } else if (type === "ec") {
    validateString(opts.namedCurve, "options.namedCurve");
    params.namedCurve = opts.namedCurve;
  } else if (type === "dh") {
    if (opts.group !== undefined) params.group = opts.group;
    else if (opts.prime !== undefined) params.prime = Buffer.from(latin1(opts.prime, undefined, "options.prime"), "latin1").toString("hex");
    else validateInteger(opts.primeLength, "options.primeLength", 0, 2 ** 31 - 1);
    params.primeLength = opts.primeLength;
    params.generator = opts.generator;
  } else if (type === "dsa") {
    validateInteger(opts.modulusLength, "options.modulusLength", 0, 2 ** 32 - 1);
    if (opts.divisorLength !== undefined && opts.divisorLength !== null) validateInteger(opts.divisorLength, "options.divisorLength", 0, 2 ** 31 - 1);
    params.modulusLength = opts.modulusLength;
    params.divisorLength = opts.divisorLength === null ? undefined : opts.divisorLength;
  }
  for (const name of ["publicKeyEncoding", "privateKeyEncoding"]) if (opts[name] !== undefined) validateObject(opts[name], `options.${name}`);
  return () => {
    const pkcs8 = internal.bytesToLatin1(nativeCrypto.keyGenerate(type, JSON.stringify(params)));
    const privateKey = new PrivateKeyObject(pkcs8);
    const publicKey = publicOf(privateKey);
    return {
      publicKey: opts.publicKeyEncoding ? publicKey.export(opts.publicKeyEncoding) : publicKey,
      privateKey: opts.privateKeyEncoding ? privateKey.export(opts.privateKeyEncoding) : privateKey,
    };
  };
}

function generateKeyPairSync(type, options) {
  return prepareKeyPair(type, options)();
}

function generateKeyPair(type, options, callback) {
  if (typeof options === "function") [callback, options] = [options, undefined];
  validateFunction(callback, "callback");
  const work = prepareKeyPair(type, options);
  maybeAsync(work, (error, pair) => (error ? callback(error) : callback(null, pair.publicKey, pair.privateKey)));
}

function generateKeySync(type, options) {
  validateString(type, "type");
  validateObject(options, "options");
  const { length } = options;
  if (type === "hmac") validateInteger(length, "options.length", 8, 2 ** 31 - 1);
  else if (type === "aes") {
    if (![128, 192, 256].includes(length)) throw invalidValue("options.length", length, "must be one of: 128, 192, 256");
  } else throw invalidValue("type", type, "must be a supported key type");
  return new SecretKeyObject(internal.bytesToLatin1(nativeCrypto.randomBytes(Math.floor(length / 8))));
}

function generateKey(type, options, callback) {
  validateFunction(callback, "callback");
  const key = generateKeySync(type, options);
  process.nextTick(callback, null, key);
}

function signOptions(hash, options) {
  const { padding, saltLength, dsaEncoding } = options;
  if (padding !== undefined) validateInteger(padding, "options.padding", -(2 ** 31), 2 ** 31 - 1);
  if (saltLength !== undefined) validateInteger(saltLength, "options.saltLength", -(2 ** 31), 2 ** 31 - 1);
  if (dsaEncoding !== undefined && dsaEncoding !== "der" && dsaEncoding !== "ieee-p1363") throw invalidValue("options.dsaEncoding", dsaEncoding);
  return JSON.stringify({ hash, padding, saltLength, dsaEncoding });
}

function keyRequired(key) {
  if (!key) throw codeError(Error, "ERR_CRYPTO_SIGN_KEY_REQUIRED", "No key provided to sign");
}

function streamingKeyCheck(key) {
  if (key.asymmetricKeyType === "ed25519") {
    throw codeError(Error, "ERR_OSSL_EVP_OPERATION_NOT_SUPPORTED_FOR_THIS_KEYTYPE", "error:1C8000A5:Provider routines::operation not supported for this keytype");
  }
}

function SignBase(algorithm, options) {
  validateString(algorithm, "algorithm");
  Writable.call(this, options);
  this._hash = nativeCrypto.normalizeHash(algorithm);
  this._chunks = [];
}
Object.setPrototypeOf(SignBase.prototype, Writable.prototype);
SignBase.prototype._write = function _write(chunk, encoding, callback) {
  this.update(chunk, encoding);
  callback();
};
SignBase.prototype.update = function update(data, inputEncoding) {
  this._chunks.push(latin1(data, inputEncoding, "data"));
  return this;
};

function Sign(algorithm, options) {
  if (!(this instanceof Sign)) return new Sign(algorithm, options);
  SignBase.call(this, algorithm, options);
}
Object.setPrototypeOf(Sign.prototype, SignBase.prototype);
Sign.prototype.sign = function sign(privateKey, outputEncoding) {
  keyRequired(privateKey);
  const { key, options } = asymmetricKey(privateKey, "private", "privateKey");
  streamingKeyCheck(key);
  return encodeOut(nativeCrypto.sign(true, key._der, this._chunks.join(""), signOptions(this._hash, options)), outputEncoding);
};

function Verify(algorithm, options) {
  if (!(this instanceof Verify)) return new Verify(algorithm, options);
  SignBase.call(this, algorithm, options);
}
Object.setPrototypeOf(Verify.prototype, SignBase.prototype);
Verify.prototype.verify = function verify(object, signature, signatureEncoding) {
  keyRequired(object);
  const { key, options } = asymmetricKey(object, "public", "key");
  streamingKeyCheck(key);
  const sig = latin1(signature, signatureEncoding, "signature");
  return nativeCrypto.verify(key.type === "private", key._der, this._chunks.join(""), sig, signOptions(this._hash, options));
};

function oneShotArgs(algorithm, data) {
  if (algorithm !== null && algorithm !== undefined) validateString(algorithm, "algorithm");
  if (!ArrayBuffer.isView(data)) throw invalidArg("data", "an instance of Buffer, TypedArray, or DataView", data);
  return algorithm === null || algorithm === undefined ? "" : nativeCrypto.normalizeHash(algorithm);
}

function sign(algorithm, data, key, callback) {
  const hash = oneShotArgs(algorithm, data);
  keyRequired(key);
  const { key: keyObject, options } = asymmetricKey(key, "private");
  return maybeAsync(() => toBuffer(nativeCrypto.sign(true, keyObject._der, latin1(data), signOptions(hash, options))), callback);
}

function verify(algorithm, data, key, signature, callback) {
  const hash = oneShotArgs(algorithm, data);
  if (!ArrayBuffer.isView(signature)) throw invalidArg("signature", "an instance of ArrayBuffer, Buffer, TypedArray, or DataView", signature);
  keyRequired(key);
  const { key: keyObject, options } = asymmetricKey(key, "public");
  const run = () => nativeCrypto.verify(keyObject.type === "private", keyObject._der, latin1(data), latin1(signature), signOptions(hash, options));
  return maybeAsync(run, callback);
}

function rsaCipher(op, kind, defaultPadding) {
  return function rsaOperation(key, buffer) {
    const { key: keyObject, options } = asymmetricKey(key, kind);
    const data = latin1(buffer, options.encoding, "buffer");
    if (options.oaepHash !== undefined) validateString(options.oaepHash, "key.oaepHash");
    const params = {
      padding: options.padding === undefined ? defaultPadding : options.padding,
      oaepHash: options.oaepHash,
      oaepLabel: options.oaepLabel === undefined ? "" : latin1(options.oaepLabel, undefined, "key.oaepLabel"),
    };
    return toBuffer(nativeCrypto.rsaCrypt(op, keyObject.type === "private", keyObject._der, data, JSON.stringify(params)));
  };
}

function diffieHellman(options) {
  validateObject(options, "options");
  const { privateKey, publicKey } = options;
  if (!(privateKey instanceof KeyObject)) throw invalidValue("options.privateKey", privateKey);
  if (!(publicKey instanceof KeyObject)) throw invalidValue("options.publicKey", publicKey);
  if (privateKey.type !== "private") throw invalidKeyObjectType(privateKey.type, "private");
  if (publicKey.type === "secret") throw invalidKeyObjectType(publicKey.type, "private or public");
  return toBuffer(nativeCrypto.keyAgree(privateKey._der, publicOf(publicKey)._der));
}

module.exports = {
  helpers: { codeError, invalidArg, invalidValue, outOfRange, validateObject, validateString, validateFunction, validateInteger, isBytes, latin1, toBuffer, encodeOut, bigToLatin1, latin1ToBig, maybeAsync },
  KeyObject,
  SecretKeyObject,
  createSecretKey,
  createPublicKey,
  createPrivateKey,
  generateKeyPair,
  generateKeyPairSync,
  generateKey,
  generateKeySync,
  Sign,
  Verify,
  sign,
  verify,
  publicEncrypt: rsaCipher("publicEncrypt", "public", 4),
  privateDecrypt: rsaCipher("privateDecrypt", "private", 4),
  privateEncrypt: rsaCipher("privateEncrypt", "private", 1),
  publicDecrypt: rsaCipher("publicDecrypt", "public", 1),
  diffieHellman,
};
