// node:crypto: hashes, HMAC, random values, PBKDF2, and a webcrypto object without subtle.
const { Buffer } = require("buffer");
const { Transform } = require("stream");
const nativeCrypto = native.crypto;
const HASHES = ["md5", "sha1", "sha256", "sha384", "sha512", "sha512-256"];
const released = new FinalizationRegistry((id) => nativeCrypto.drop(id));

function invalidArg(name, expected, value) {
  return Object.assign(new TypeError(`The "${name}" argument must be ${expected}. Received ${value === null ? "null" : typeof value}`), { code: "ERR_INVALID_ARG_TYPE" });
}

function toLatin1(data, encoding, name) {
  if (typeof data === "string") return internal.bytesToLatin1(Buffer.from(data, encoding || "utf8"));
  const bytes = internal.bytesToLatin1(data);
  if (bytes === undefined) throw invalidArg(name, "of type string or an instance of Buffer, TypedArray, or DataView", data);
  return bytes;
}

function encodeOutput(bytes, encoding) {
  const buffer = Buffer.from(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  return encoding && encoding !== "buffer" ? buffer.toString(encoding) : buffer;
}

// Hash and Hmac are Transform streams over a native incremental digest.
function Digest(id, kind) {
  Transform.call(this);
  this._id = id;
  this._kind = kind;
  this._finalized = false;
  released.register(this, id, this);
}
Object.setPrototypeOf(Digest.prototype, Transform.prototype);
Object.setPrototypeOf(Digest, Transform);
Digest.prototype.update = function update(data, inputEncoding) {
  if (this._finalized) throw Object.assign(new Error("Digest already called"), { code: "ERR_CRYPTO_HASH_FINALIZED" });
  nativeCrypto.update(this._id, toLatin1(data, inputEncoding, "data"));
  return this;
};
Digest.prototype.digest = function digest(outputEncoding) {
  if (this._finalized) throw Object.assign(new Error("Digest already called"), { code: "ERR_CRYPTO_HASH_FINALIZED" });
  this._finalized = true;
  released.unregister(this);
  return encodeOutput(nativeCrypto.digest(this._id), outputEncoding);
};
Digest.prototype._transform = function _transform(chunk, encoding, callback) {
  this.update(chunk, encoding);
  callback();
};
Digest.prototype._flush = function _flush(callback) {
  this.push(this.digest());
  callback();
};

function Hash(algorithm, options) {
  if (!(this instanceof Hash)) return new Hash(algorithm, options);
  if (typeof algorithm !== "string") throw invalidArg("algorithm", "of type string", algorithm);
  Digest.call(this, nativeCrypto.hash(algorithm), "Hash");
}
Object.setPrototypeOf(Hash.prototype, Digest.prototype);
Hash.prototype.copy = function copy() {
  if (this._finalized) throw Object.assign(new Error("Digest already called"), { code: "ERR_CRYPTO_HASH_FINALIZED" });
  const clone = Object.create(Hash.prototype);
  Digest.call(clone, nativeCrypto.copy(this._id), "Hash");
  return clone;
};

function Hmac(algorithm, key, options) {
  if (!(this instanceof Hmac)) return new Hmac(algorithm, key, options);
  if (typeof algorithm !== "string") throw invalidArg("hmac", "of type string", algorithm);
  const keyBytes = key && typeof key === "object" && key.type === "secret" && key._bytes ? key._bytes : toLatin1(key, options && options.encoding, "key");
  Digest.call(this, nativeCrypto.hmac(algorithm, keyBytes), "Hmac");
}
Object.setPrototypeOf(Hmac.prototype, Digest.prototype);

function randomBytes(size, callback) {
  if (typeof size !== "number" || size < 0 || !Number.isInteger(size)) {
    throw Object.assign(new RangeError(`The value of "size" is out of range. Received ${size}`), { code: "ERR_OUT_OF_RANGE" });
  }
  const bytes = Buffer.from(nativeCrypto.randomBytes(size).buffer);
  if (typeof callback === "function") {
    process.nextTick(callback, null, bytes);
    return undefined;
  }
  return bytes;
}

function randomFillSync(target, offset = 0, size) {
  const view = new Uint8Array(target.buffer || target, target.byteOffset || 0, target.byteLength);
  const length = size === undefined ? view.length - offset : size;
  view.set(nativeCrypto.randomBytes(length), offset);
  return target;
}

function randomFill(target, offset, size, callback) {
  if (typeof offset === "function") [callback, offset, size] = [offset, 0, undefined];
  else if (typeof size === "function") [callback, size] = [size, undefined];
  try {
    randomFillSync(target, offset, size);
  } catch (error) {
    process.nextTick(callback, error);
    return;
  }
  process.nextTick(callback, null, target);
}

function getRandomValues(array) {
  if (!ArrayBuffer.isView(array) || array instanceof Float32Array || array instanceof Float64Array || array instanceof DataView) {
    throw new (globalThis.DOMException || TypeError)("The data argument must be an integer-type TypedArray", "TypeMismatchError");
  }
  if (array.byteLength > 65536) {
    throw new (globalThis.DOMException || RangeError)("The ArrayBufferView's byte length exceeds the number of bytes of entropy available", "QuotaExceededError");
  }
  return randomFillSync(array);
}

function randomUUID() {
  const bytes = nativeCrypto.randomBytes(16);
  bytes[6] = (bytes[6] & 0x0f) | 0x40;
  bytes[8] = (bytes[8] & 0x3f) | 0x80;
  const hex = Array.from(bytes, (byte) => byte.toString(16).padStart(2, "0")).join("");
  return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${hex.slice(20)}`;
}

function randomInt(min, max, callback) {
  if (max === undefined || typeof max === "function") [callback, max, min] = [max, min, 0];
  if (!Number.isSafeInteger(min) || !Number.isSafeInteger(max) || max <= min) {
    throw Object.assign(new RangeError(`The value of "max" is out of range. It must be greater than the value of "min" (${min}). Received ${max}`), { code: "ERR_OUT_OF_RANGE" });
  }
  const range = max - min;
  const limit = 2 ** 48 - (2 ** 48 % range);
  let value;
  do {
    value = Buffer.from(nativeCrypto.randomBytes(6).buffer).readUIntBE(0, 6);
  } while (value >= limit);
  const result = min + (value % range);
  if (typeof callback === "function") {
    process.nextTick(callback, null, result);
    return undefined;
  }
  return result;
}

function timingSafeEqual(a, b) {
  if (!ArrayBuffer.isView(a) && !(a instanceof ArrayBuffer)) throw invalidArg("buf1", "an instance of ArrayBuffer, Buffer, TypedArray, or DataView", a);
  if (!ArrayBuffer.isView(b) && !(b instanceof ArrayBuffer)) throw invalidArg("buf2", "an instance of ArrayBuffer, Buffer, TypedArray, or DataView", b);
  const left = internal.bytesToLatin1(a);
  const right = internal.bytesToLatin1(b);
  if (left.length !== right.length) {
    throw Object.assign(new RangeError("Input buffers must have the same byte length"), { code: "ERR_CRYPTO_TIMING_SAFE_EQUAL_LENGTH" });
  }
  let difference = 0;
  for (let i = 0; i < left.length; i++) difference |= left.charCodeAt(i) ^ right.charCodeAt(i);
  return difference === 0;
}

function pbkdf2Sync(password, salt, iterations, keylen, digest) {
  const bytes = nativeCrypto.pbkdf2(toLatin1(password, "utf8", "password"), toLatin1(salt, "utf8", "salt"), iterations, keylen, String(digest));
  return Buffer.from(bytes.buffer);
}

function pbkdf2(password, salt, iterations, keylen, digest, callback) {
  let result;
  try {
    result = pbkdf2Sync(password, salt, iterations, keylen, digest);
  } catch (error) {
    process.nextTick(callback, error);
    return;
  }
  process.nextTick(callback, null, result);
}

function hash(algorithm, data, outputEncoding = "hex") {
  return new Hash(algorithm).update(data).digest(outputEncoding);
}

class KeyObject {
  constructor(type, bytes) {
    this.type = type;
    this._bytes = bytes;
  }
  get symmetricKeySize() {
    return this._bytes.length;
  }
  export() {
    return Buffer.from(this._bytes, "latin1");
  }
  equals(other) {
    return other instanceof KeyObject && other._bytes === this._bytes;
  }
}

function unsupported(name) {
  return function unsupportedCrypto() {
    throw internal.unsupported(`crypto.${name}`);
  };
}

const subtle = new Proxy({}, {
  get(target, name) {
    if (typeof name === "symbol") return undefined;
    return unsupported(`subtle.${name}`);
  },
});
const webcrypto = { getRandomValues, randomUUID, subtle, CryptoKey: class CryptoKey {} };

module.exports = {
  createHash: (algorithm, options) => new Hash(algorithm, options),
  createHmac: (algorithm, key, options) => new Hmac(algorithm, key, options),
  createSecretKey: (key, encoding) => new KeyObject("secret", toLatin1(key, encoding, "key")),
  Hash,
  Hmac,
  KeyObject,
  hash,
  getHashes: () => HASHES.slice(),
  getCiphers: () => [],
  getCurves: () => [],
  randomBytes,
  pseudoRandomBytes: randomBytes,
  randomFill,
  randomFillSync,
  randomInt,
  randomUUID,
  getRandomValues,
  timingSafeEqual,
  pbkdf2,
  pbkdf2Sync,
  webcrypto,
  subtle,
  constants: {},
  getFips: () => 0,
  setFips: unsupported("setFips"),
};
for (const name of [
  "createCipheriv", "createDecipheriv", "createSign", "createVerify", "createDiffieHellman", "createDiffieHellmanGroup",
  "createECDH", "generateKeyPair", "generateKeyPairSync", "generateKey", "generateKeySync", "createPrivateKey", "createPublicKey",
  "publicEncrypt", "privateDecrypt", "privateEncrypt", "publicDecrypt", "sign", "verify", "scrypt", "scryptSync", "hkdf",
  "hkdfSync", "diffieHellman", "generatePrime", "generatePrimeSync", "checkPrime", "checkPrimeSync", "secureHeapUsed",
]) {
  module.exports[name] = unsupported(name);
}
