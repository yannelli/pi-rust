// node:crypto X509Certificate over the natives in node/crypto/x509. The native side keeps no
// state; each instance holds its DER as a latin1 string and caches what it derives.
const { Buffer } = require("buffer");
const keys = require("internal/crypto_keys");
const { invalidArg, invalidValue, validateObject, validateString } = keys.helpers;
const nativeCrypto = native.crypto;
const kDer = Symbol("kDer");
const kCache = Symbol("kCache");
const kIssuer = Symbol("kIssuer");
const customInspect = Symbol.for("nodejs.util.inspect.custom");
const SUBJECT_FLAGS = { __proto__: null, default: 0, always: 0x1, never: 0x20 };

function getFlags(options = {}) {
  validateObject(options, "options");
  const { subject = "default", wildcards = true, partialWildcards = true, multiLabelWildcards = false, singleLabelSubdomains = false } = { ...options };
  validateString(subject, "options.subject");
  for (const [name, value] of Object.entries({ wildcards, partialWildcards, multiLabelWildcards, singleLabelSubdomains })) {
    if (typeof value !== "boolean") throw invalidArg(`options.${name}`, "of type boolean", value);
  }
  let flags = SUBJECT_FLAGS[subject];
  if (flags === undefined) throw invalidValue("options.subject", subject);
  if (!wildcards) flags |= 0x2;
  if (!partialWildcards) flags |= 0x4;
  if (multiLabelWildcards) flags |= 0x8;
  if (singleLabelSubdomains) flags |= 0x10;
  return flags;
}

function cached(cert, name, compute) {
  const cache = cert[kCache];
  if (!cache.has(name)) cache.set(name, compute());
  return cache.get(name);
}

function info(cert) {
  return cached(cert, "info", () => JSON.parse(nativeCrypto.x509Info(cert[kDer])));
}

function field(name) {
  return function () {
    const value = info(this)[name];
    return value === null ? undefined : value;
  };
}

function nameObject(pairs) {
  const object = { __proto__: null };
  for (const [key, value] of pairs) {
    if (value === null) continue;
    if (!(key in object)) object[key] = value;
    else if (Array.isArray(object[key])) object[key].push(value);
    else object[key] = [object[key], value];
  }
  return object;
}

// translatePeerCertificate from Node's lib/internal/tls/common.js.
function accessObject(text) {
  const object = { __proto__: null };
  text.replace(/([^\n:]*):([^\n]*)(?:\n|$)/g, (all, key, value) => {
    if (value.charCodeAt(0) === 0x22) value = JSON.parse(value);
    if (key in object) object[key].push(value);
    else object[key] = [value];
  });
  return object;
}

function keyObject(value, type) {
  if (!(value instanceof keys.KeyObject)) throw invalidArg("pkey", "an instance of KeyObject", value);
  if (value.type !== type) throw invalidValue("pkey", value);
  return value;
}

function latin1Bytes(data) {
  return internal.bytesToLatin1(Buffer.from(data.buffer, data.byteOffset, data.byteLength));
}

class X509Certificate {
  constructor(buffer) {
    if (typeof buffer === "string") buffer = Buffer.from(buffer);
    if (!ArrayBuffer.isView(buffer)) throw invalidArg("buffer", "of type string or an instance of Buffer, TypedArray, or DataView", buffer);
    Object.defineProperty(this, kDer, { value: nativeCrypto.x509Parse(latin1Bytes(buffer)) });
    Object.defineProperty(this, kCache, { value: new Map() });
  }

  get issuerCertificate() {
    return this[kIssuer];
  }

  get validFromDate() {
    return cached(this, "validFromDate", () => new Date(info(this).validFromTime ?? NaN));
  }

  get validToDate() {
    return cached(this, "validToDate", () => new Date(info(this).validToTime ?? NaN));
  }

  get raw() {
    return cached(this, "raw", () => Buffer.from(this[kDer], "latin1"));
  }

  get publicKey() {
    return cached(this, "publicKey", () => keys.createPublicKey({ key: Buffer.from(info(this).spki, "latin1"), format: "der", type: "spki" }));
  }

  toString() {
    return cached(this, "pem", () => {
      const lines = this.raw.toString("base64").match(/.{1,64}/g) || [];
      return `-----BEGIN CERTIFICATE-----\n${lines.join("\n")}\n-----END CERTIFICATE-----\n`;
    });
  }

  toJSON() {
    return this.toString();
  }

  checkHost(name, options) {
    validateString(name, "name");
    return nativeCrypto.x509Check(this[kDer], "host", name, getFlags(options)) ?? undefined;
  }

  checkEmail(email, options) {
    validateString(email, "email");
    return nativeCrypto.x509Check(this[kDer], "email", email, getFlags(options)) ?? undefined;
  }

  checkIP(ip, options) {
    validateString(ip, "ip");
    return nativeCrypto.x509Check(this[kDer], "ip", ip, getFlags(options)) ?? undefined;
  }

  checkIssued(otherCert) {
    if (!(otherCert instanceof X509Certificate)) throw invalidArg("otherCert", "an instance of X509Certificate", otherCert);
    return nativeCrypto.x509Issued(this[kDer], otherCert[kDer]);
  }

  checkPrivateKey(pkey) {
    return nativeCrypto.x509CheckPrivateKey(this[kDer], keyObject(pkey, "private")._der);
  }

  verify(pkey) {
    return nativeCrypto.x509Verify(this[kDer], keyObject(pkey, "public")._der);
  }

  toLegacyObject() {
    const data = info(this);
    const object = { subject: nameObject(data.legacySubject), issuer: nameObject(data.legacyIssuer) };
    if (data.subjectAltName !== null) object.subjectaltname = data.subjectAltName;
    if (data.infoAccess !== null) object.infoAccess = accessObject(data.infoAccess);
    object.ca = data.ca;
    for (const [key, value] of data.legacyKey) object[key] = key === "pubkey" ? Buffer.from(value, "latin1") : value;
    Object.assign(object, { valid_from: data.validFrom, valid_to: data.validTo, fingerprint: data.fingerprint, fingerprint256: data.fingerprint256, fingerprint512: data.fingerprint512 });
    if (data.keyUsage !== null) object.ext_key_usage = data.keyUsage.slice();
    object.serialNumber = data.serialNumber;
    object.raw = Buffer.from(this.raw);
    return object;
  }

  [customInspect](depth, options, inspect) {
    if (depth < 0) return this;
    const fields = {};
    for (const name of ["subject", "subjectAltName", "issuer", "infoAccess", "validFrom", "validTo", "validFromDate", "validToDate", "fingerprint", "fingerprint256", "fingerprint512", "keyUsage", "serialNumber"]) {
      fields[name] = this[name];
    }
    return `X509Certificate ${inspect(fields, { ...options, depth: options.depth == null ? null : options.depth - 1 })}`;
  }
}

for (const name of ["subject", "subjectAltName", "issuer", "infoAccess", "validFrom", "validTo", "fingerprint", "fingerprint256", "fingerprint512", "keyUsage", "serialNumber", "ca"]) {
  Object.defineProperty(X509Certificate.prototype, name, { get: field(name), configurable: true, enumerable: false });
}

// The peer chain of a TLS socket, each certificate linked to the next as its issuerCertificate.
function peerCertificate(chain) {
  let issuer;
  for (let index = chain.length - 1; index >= 0; index--) {
    const cert = new X509Certificate(Buffer.from(chain[index], "hex"));
    if (issuer !== undefined) Object.defineProperty(cert, kIssuer, { value: issuer });
    issuer = cert;
  }
  return issuer;
}

module.exports = { X509Certificate, peerCertificate };
