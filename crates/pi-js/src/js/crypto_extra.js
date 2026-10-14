// node:crypto AEAD ciphers, Diffie-Hellman, ECDH, scrypt, HKDF, and primes.
const { Buffer } = require("buffer");
const { Transform } = require("stream");
const { StringDecoder } = require("string_decoder");
const { KeyObject, helpers } = require("internal/crypto_keys");
const { codeError, invalidArg, invalidValue, outOfRange, validateObject, validateString, validateFunction, validateInteger, isBytes, latin1, toBuffer, encodeOut, bigToLatin1, latin1ToBig, maybeAsync } = helpers;
const nativeCrypto = native.crypto;
const released = new FinalizationRegistry((id) => nativeCrypto.drop(id));

const CIPHERS = ["aes-128-gcm", "aes-256-gcm", "chacha20-poly1305", "id-aes128-gcm", "id-aes256-gcm"];
// OpenSSL cipher families that would need another crate (aes, cbc, ctr, des, ...).
const MISSING_CIPHERS = /^(aes|id-aes|aria|camellia|des|bf|blowfish|cast|idea|rc2|rc4|rc5|seed|sm4|chacha20|id-smime)/;

function invalidState(operation) {
  return codeError(Error, "ERR_CRYPTO_INVALID_STATE", `Invalid state for operation ${operation}`);
}

function secretBytes(key, name) {
  if (key instanceof KeyObject) {
    if (key.type !== "secret") throw codeError(TypeError, "ERR_CRYPTO_INVALID_KEY_OBJECT_TYPE", `Invalid key object type ${key.type}, expected secret.`);
    return key._bytes;
  }
  return latin1(key, undefined, name);
}

function CipherBase(decrypt, cipher, key, iv, options) {
  validateString(cipher, "cipher");
  const name = cipher.toLowerCase();
  if (!CIPHERS.includes(name)) {
    if (MISSING_CIPHERS.test(name)) throw internal.unsupported(`crypto cipher ${cipher}`);
    throw codeError(Error, "ERR_CRYPTO_UNKNOWN_CIPHER", "Unknown cipher");
  }
  const keyBytes = secretBytes(key, "key");
  const ivBytes = iv === null || iv === undefined ? "" : latin1(iv, undefined, "iv");
  if (options !== undefined) validateObject(options, "options");
  const tagLength = options && options.authTagLength;
  if (tagLength !== undefined) validateInteger(tagLength, "options.authTagLength", 0, 2 ** 31 - 1);
  Transform.call(this, options);
  this._id = nativeCrypto.cipherNew(name, keyBytes, ivBytes, decrypt, tagLength);
  this._decrypt = decrypt;
  this._finalized = false;
  this._started = false;
  this._tag = undefined;
  this._decoder = undefined;
  released.register(this, this._id, this);
}
Object.setPrototypeOf(CipherBase.prototype, Transform.prototype);
CipherBase.prototype._output = function _output(bytes, encoding, end) {
  const buffer = toBuffer(bytes);
  if (!encoding || encoding === "buffer") return buffer;
  this._decoder = this._decoder || new StringDecoder(encoding);
  return end ? this._decoder.end(buffer) : this._decoder.write(buffer);
};
CipherBase.prototype.update = function update(data, inputEncoding, outputEncoding) {
  if (this._finalized) throw invalidState("update");
  if (typeof data !== "string" && !ArrayBuffer.isView(data)) throw invalidArg("data", "of type string or an instance of Buffer, TypedArray, or DataView", data);
  this._started = true;
  return this._output(nativeCrypto.cipherUpdate(this._id, latin1(data, inputEncoding, "data")), outputEncoding, false);
};
CipherBase.prototype.final = function final(outputEncoding) {
  if (this._finalized) throw invalidState("final");
  this._finalized = true;
  released.unregister(this);
  const tag = nativeCrypto.cipherFinal(this._id);
  if (!this._decrypt) this._tag = toBuffer(tag);
  return this._output(new Uint8Array(0), outputEncoding, true);
};
CipherBase.prototype.setAAD = function setAAD(buffer, options) {
  if (this._finalized || this._started) throw invalidState("setAAD");
  if (!ArrayBuffer.isView(buffer)) throw invalidArg("buffer", "an instance of Buffer, TypedArray, or DataView", buffer);
  nativeCrypto.cipherAad(this._id, latin1(buffer));
  return this;
};
CipherBase.prototype.setAutoPadding = function setAutoPadding() {
  return this;
};
CipherBase.prototype._transform = function _transform(chunk, encoding, callback) {
  this.push(this.update(chunk, encoding));
  callback();
};
CipherBase.prototype._flush = function _flush(callback) {
  try {
    this.push(this.final());
  } catch (error) {
    callback(error);
    return;
  }
  callback();
};

function Cipheriv(cipher, key, iv, options) {
  if (!(this instanceof Cipheriv)) return new Cipheriv(cipher, key, iv, options);
  CipherBase.call(this, false, cipher, key, iv, options);
}
Object.setPrototypeOf(Cipheriv.prototype, CipherBase.prototype);
Cipheriv.prototype.getAuthTag = function getAuthTag() {
  if (!this._tag) throw invalidState("getAuthTag");
  return this._tag;
};

function Decipheriv(cipher, key, iv, options) {
  if (!(this instanceof Decipheriv)) return new Decipheriv(cipher, key, iv, options);
  CipherBase.call(this, true, cipher, key, iv, options);
}
Object.setPrototypeOf(Decipheriv.prototype, CipherBase.prototype);
Decipheriv.prototype.setAuthTag = function setAuthTag(tag, encoding) {
  if (this._finalized) throw invalidState("setAuthTag");
  nativeCrypto.cipherTag(this._id, latin1(tag, encoding, "buffer"));
  return this;
};

function randomBelow(limit) {
  const bytes = internal.bytesToLatin1(nativeCrypto.randomBytes(Math.ceil(limit.toString(16).length / 2) + 8));
  return latin1ToBig(bytes) % limit;
}

function numberBytes(value) {
  return bigToLatin1(BigInt(value));
}

// Finite-field Diffie-Hellman over a given or generated prime.
function DiffieHellman(sizeOrKey, keyEncoding, generator, genEncoding) {
  if (!(this instanceof DiffieHellman)) return new DiffieHellman(sizeOrKey, keyEncoding, generator, genEncoding);
  if (typeof sizeOrKey !== "number" && typeof sizeOrKey !== "string" && !isBytes(sizeOrKey)) {
    throw invalidArg("sizeOrKey", "of type number or string or an instance of ArrayBuffer, Buffer, TypedArray, or DataView", sizeOrKey);
  }
  if (keyEncoding && typeof keyEncoding !== "string") [genEncoding, generator, keyEncoding] = [generator, keyEncoding, undefined];
  if (typeof sizeOrKey === "number") {
    validateInteger(sizeOrKey, "sizeOrKey", 2, 2 ** 31 - 1);
    this._prime = internal.bytesToLatin1(nativeCrypto.generatePrime(sizeOrKey, true, "", ""));
  } else {
    this._prime = latin1(sizeOrKey, keyEncoding, "sizeOrKey");
  }
  if (generator === undefined) generator = 2;
  this._generator = typeof generator === "number" ? numberBytes(generator) : latin1(generator, genEncoding, "generator");
  if (latin1ToBig(this._generator) < 2n) throw codeError(Error, "ERR_OSSL_DH_BAD_GENERATOR", "error:028000B7:Diffie-Hellman routines::bad generator");
  this._privateKey = undefined;
  this._publicKey = undefined;
  this._verifyError = undefined;
}
Object.defineProperty(DiffieHellman.prototype, "verifyError", {
  get() {
    if (this._verifyError === undefined) this._verifyError = nativeCrypto.dhCheck(this._prime, this._generator);
    return this._verifyError;
  },
});
DiffieHellman.prototype.generateKeys = function generateKeys(encoding) {
  if (this._privateKey === undefined) this._privateKey = bigToLatin1(randomBelow(latin1ToBig(this._prime) - 3n) + 2n);
  this._publicKey = internal.bytesToLatin1(nativeCrypto.modPow(this._generator, this._privateKey, this._prime));
  return this.getPublicKey(encoding);
};
DiffieHellman.prototype.computeSecret = function computeSecret(otherPublicKey, inputEncoding, outputEncoding) {
  if (this._privateKey === undefined) throw invalidState("computeSecret");
  const other = latin1(otherPublicKey, inputEncoding, "otherPublicKey");
  const value = latin1ToBig(other);
  if (value <= 1n) throw codeError(Error, "ERR_CRYPTO_INVALID_KEYLEN", "Supplied key is too small");
  if (value >= latin1ToBig(this._prime) - 1n) throw codeError(Error, "ERR_CRYPTO_INVALID_KEYLEN", "Supplied key is too large");
  return encodeOut(nativeCrypto.modPow(other, this._privateKey, this._prime), outputEncoding);
};
for (const [method, field, message] of [
  ["getPrime", "_prime"],
  ["getGenerator", "_generator"],
  ["getPublicKey", "_publicKey", "No public key - did you forget to generate one?"],
  ["getPrivateKey", "_privateKey", "No private key - did you forget to generate one?"],
]) {
  DiffieHellman.prototype[method] = function getter(encoding) {
    if (this[field] === undefined) throw codeError(Error, "ERR_CRYPTO_INVALID_STATE", message);
    return encodeOut(Buffer.from(this[field], "latin1"), encoding);
  };
}
DiffieHellman.prototype.setPublicKey = function setPublicKey(key, encoding) {
  this._publicKey = latin1(key, encoding, "key");
  return this;
};
DiffieHellman.prototype.setPrivateKey = function setPrivateKey(key, encoding) {
  this._privateKey = latin1(key, encoding, "key");
  this._publicKey = internal.bytesToLatin1(nativeCrypto.modPow(this._generator, this._privateKey, this._prime));
  return this;
};

function DiffieHellmanGroup(name) {
  if (!(this instanceof DiffieHellmanGroup)) return new DiffieHellmanGroup(name);
  validateString(name, "name");
  this._prime = internal.bytesToLatin1(nativeCrypto.dhGroup(name));
  this._generator = "\x02";
  this._privateKey = undefined;
  this._publicKey = undefined;
  this._verifyError = 0;
}
Object.setPrototypeOf(DiffieHellmanGroup.prototype, DiffieHellman.prototype);
DiffieHellmanGroup.prototype.setPublicKey = undefined;
DiffieHellmanGroup.prototype.setPrivateKey = undefined;

const CURVES = ["prime256v1", "secp256k1", "secp384r1", "secp521r1"];
const CURVE_ALIASES = ["P-256", "P-384", "P-521", "secp256r1"];
const FORMATS = { compressed: 2, uncompressed: 4, hybrid: 6 };

function checkCurve(curve) {
  validateString(curve, "curve");
  if (!CURVES.includes(curve) && !CURVE_ALIASES.includes(curve)) throw codeError(TypeError, "ERR_CRYPTO_INVALID_CURVE", "Invalid EC curve name");
}

function pointForm(format) {
  if (format === undefined) return 4;
  if (!FORMATS[format]) throw codeError(TypeError, "ERR_CRYPTO_ECDH_INVALID_FORMAT", `Invalid ECDH format: ${format}`);
  return FORMATS[format];
}

function ECDH(curve) {
  if (!(this instanceof ECDH)) return new ECDH(curve);
  checkCurve(curve);
  this._curve = curve;
  this._privateKey = undefined;
  this._publicKey = undefined;
}
ECDH.convertKey = function convertKey(key, curve, inputEncoding, outputEncoding, format) {
  checkCurve(curve);
  return encodeOut(nativeCrypto.ecConvert(curve, latin1(key, inputEncoding, "key"), pointForm(format)), outputEncoding);
};
ECDH.prototype.generateKeys = function generateKeys(encoding, format) {
  this._privateKey = internal.bytesToLatin1(nativeCrypto.ecGenerate(this._curve));
  this._publicKey = internal.bytesToLatin1(nativeCrypto.ecPublic(this._curve, this._privateKey));
  return this.getPublicKey(encoding, format);
};
ECDH.prototype.computeSecret = function computeSecret(key, inputEncoding, outputEncoding) {
  if (this._privateKey === undefined) throw invalidState("computeSecret");
  return encodeOut(nativeCrypto.ecCompute(this._curve, this._privateKey, latin1(key, inputEncoding, "key")), outputEncoding);
};
ECDH.prototype.getPrivateKey = function getPrivateKey(encoding) {
  if (this._privateKey === undefined) throw codeError(Error, "ERR_CRYPTO_INVALID_STATE", "Failed to get ECDH private key");
  return encodeOut(Buffer.from(this._privateKey, "latin1"), encoding);
};
ECDH.prototype.getPublicKey = function getPublicKey(encoding, format) {
  if (this._publicKey === undefined) throw codeError(Error, "ERR_CRYPTO_INVALID_STATE", "Failed to get ECDH public key");
  const form = pointForm(format);
  const bytes = form === 4 ? Buffer.from(this._publicKey, "latin1") : nativeCrypto.ecConvert(this._curve, this._publicKey, form);
  return encodeOut(bytes, encoding);
};
ECDH.prototype.setPrivateKey = function setPrivateKey(key, encoding) {
  const privateKey = latin1(key, encoding, "key");
  this._publicKey = internal.bytesToLatin1(nativeCrypto.ecPublic(this._curve, privateKey));
  this._privateKey = privateKey;
  return this;
};
ECDH.prototype.setPublicKey = function setPublicKey(key, encoding) {
  this._publicKey = internal.bytesToLatin1(nativeCrypto.ecConvert(this._curve, latin1(key, encoding, "key"), 4));
  return this;
};

function scryptParams(options) {
  const params = { N: 16384, r: 8, p: 1, maxmem: 32 << 20 };
  if (options !== undefined && options !== null) {
    validateObject(options, "options");
    for (const [short, long] of [["N", "cost"], ["r", "blockSize"], ["p", "parallelization"]]) {
      if (options[short] !== undefined && options[long] !== undefined) {
        throw codeError(TypeError, "ERR_INCOMPATIBLE_OPTION_PAIR", `Option "${short}" cannot be used in combination with option "${long}"`);
      }
      const name = options[short] !== undefined ? short : long;
      if (options[name] !== undefined) {
        validateInteger(options[name], `options.${name}`, 0, 2 ** 32 - 1);
        params[short] = options[name];
      }
    }
    if (options.maxmem !== undefined) {
      validateInteger(options.maxmem, "options.maxmem", 0);
      params.maxmem = options.maxmem;
    }
  }
  const { N, r, p, maxmem } = params;
  if (N < 2 || (N & (N - 1)) !== 0 || r < 1 || p < 1 || p * r > 2 ** 30 - 1) {
    throw codeError(RangeError, "ERR_CRYPTO_INVALID_SCRYPT_PARAMS", "Invalid scrypt params");
  }
  if (128 * r * p + 128 * r * (N + 2) > maxmem) {
    throw codeError(RangeError, "ERR_CRYPTO_INVALID_SCRYPT_PARAMS", "Invalid scrypt params: error:030000AC:digital envelope routines::memory limit exceeded");
  }
  return params;
}

function scryptSync(password, salt, keylen, options) {
  const pw = latin1(password, undefined, "password");
  const s = latin1(salt, undefined, "salt");
  validateInteger(keylen, "keylen", 0, 2 ** 31 - 1);
  const { N, r, p } = scryptParams(options);
  return toBuffer(nativeCrypto.scrypt(pw, s, N, r, p, keylen));
}

function scrypt(password, salt, keylen, options, callback) {
  if (typeof options === "function") [callback, options] = [options, undefined];
  validateFunction(callback, "callback");
  const pw = latin1(password, undefined, "password");
  const s = latin1(salt, undefined, "salt");
  validateInteger(keylen, "keylen", 0, 2 ** 31 - 1);
  const { N, r, p } = scryptParams(options);
  maybeAsync(() => toBuffer(nativeCrypto.scrypt(pw, s, N, r, p, keylen)), callback);
}

function hkdfArgs(digest, ikm, salt, info, keylen) {
  validateString(digest, "digest");
  const key = secretBytes(ikm, "ikm");
  const saltBytes = latin1(salt, undefined, "salt");
  const infoBytes = latin1(info, undefined, "info");
  validateInteger(keylen, "keylen", 0, 2 ** 31 - 1);
  if (infoBytes.length > 1024) throw outOfRange("info", "<= 1024", infoBytes.length);
  return () => {
    const bytes = nativeCrypto.hkdf(digest, key, saltBytes, infoBytes, keylen);
    return bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + bytes.byteLength);
  };
}

function hkdfSync(digest, ikm, salt, info, keylen) {
  return hkdfArgs(digest, ikm, salt, info, keylen)();
}

function hkdf(digest, ikm, salt, info, keylen, callback) {
  validateFunction(callback, "callback");
  maybeAsync(hkdfArgs(digest, ikm, salt, info, keylen), callback);
}

function bigOrBytes(value, name) {
  if (typeof value === "bigint") return value;
  if (isBytes(value)) return latin1ToBig(latin1(value));
  throw invalidArg(name, "of type bigint or an instance of ArrayBuffer, Buffer, TypedArray, or DataView", value);
}

function primeArgs(size, options) {
  validateInteger(size, "size", 1, 2 ** 31 - 1);
  if (options === undefined) options = {};
  validateObject(options, "options");
  const { safe = false, bigint = false } = options;
  if (typeof safe !== "boolean") throw invalidArg("options.safe", "of type boolean", safe);
  if (typeof bigint !== "boolean") throw invalidArg("options.bigint", "of type boolean", bigint);
  const add = options.add === undefined ? undefined : bigOrBytes(options.add, "options.add");
  const rem = options.rem === undefined ? undefined : bigOrBytes(options.rem, "options.rem");
  if (add !== undefined && add > 1n << BigInt(size)) throw outOfRange("options.add", `<= 2^${size}`, `${add}n`);
  if (add !== undefined && rem !== undefined && rem >= add) throw outOfRange("options.rem", `< ${add}`, `${rem}n`);
  return () => {
    const prime = nativeCrypto.generatePrime(size, safe, add === undefined ? "" : bigToLatin1(add), rem === undefined ? "" : bigToLatin1(rem));
    if (bigint) return latin1ToBig(internal.bytesToLatin1(prime));
    return prime.buffer.slice(prime.byteOffset, prime.byteOffset + prime.byteLength);
  };
}

function generatePrimeSync(size, options) {
  return primeArgs(size, options)();
}

function generatePrime(size, options, callback) {
  if (typeof options === "function") [callback, options] = [options, undefined];
  validateFunction(callback, "callback");
  maybeAsync(primeArgs(size, options), callback);
}

function checkArgs(candidate, options) {
  const value = bigOrBytes(candidate, "candidate");
  if (options === undefined) options = {};
  validateObject(options, "options");
  const checks = options.checks === undefined ? 0 : options.checks;
  validateInteger(checks, "options.checks", 0, 2 ** 31 - 1);
  return () => nativeCrypto.checkPrime(bigToLatin1(value), checks);
}

function checkPrimeSync(candidate, options) {
  return checkArgs(candidate, options)();
}

function checkPrime(candidate, options, callback) {
  if (typeof options === "function") [callback, options] = [options, undefined];
  validateFunction(callback, "callback");
  maybeAsync(checkArgs(candidate, options), callback);
}

module.exports = {
  CIPHERS,
  CURVES,
  Cipheriv,
  Decipheriv,
  DiffieHellman,
  DiffieHellmanGroup,
  ECDH,
  scrypt,
  scryptSync,
  hkdf,
  hkdfSync,
  generatePrime,
  generatePrimeSync,
  checkPrime,
  checkPrimeSync,
};
