const crypto = require("node:crypto");
const { X509Certificate } = crypto;
const pems = { ca: __CA__, leaf: __LEAF__, pss: __PSS__, ec: __EC__, ed: __ED__, wild: __WILD__ };
const keys = { leaf: crypto.createPrivateKey(__LEAF_KEY__), ec: crypto.createPrivateKey(__EC_KEY__) };
const certs = Object.fromEntries(Object.entries(pems).map(([name, pem]) => [name, new X509Certificate(pem)]));
const failure = (error) => [error.name, error.code ?? null, error.message];
const attempt = (f) => {
  try {
    return f();
  } catch (error) {
    return failure(error);
  }
};
const shown = (value) => (value === undefined ? "<undefined>" : value);
const b64 = (value) => (Buffer.isBuffer(value) ? value.toString("base64") : value);
const plain = (value) => (value && typeof value === "object" && !Array.isArray(value) ? { proto: Object.getPrototypeOf(value) === null ? null : "object", ...value } : value);
const out = {};

for (const [name, cert] of Object.entries(certs)) {
  const legacy = cert.toLegacyObject();
  for (const key of Object.keys(legacy)) legacy[key] = plain(b64(legacy[key]));
  out[name] = {
    subject: cert.subject,
    issuer: cert.issuer,
    subjectAltName: shown(cert.subjectAltName),
    infoAccess: shown(cert.infoAccess),
    validFrom: cert.validFrom,
    validTo: cert.validTo,
    validFromDate: cert.validFromDate.toISOString(),
    validToDate: cert.validToDate.toISOString(),
    fingerprint: cert.fingerprint,
    fingerprint256: cert.fingerprint256,
    fingerprint512: cert.fingerprint512,
    keyUsage: shown(cert.keyUsage),
    serialNumber: cert.serialNumber,
    ca: cert.ca,
    issuerCertificate: shown(cert.issuerCertificate),
    raw: [Buffer.isBuffer(cert.raw), cert.raw.toString("base64") === pems[name].replace(/-----[^-]+-----|\s/g, "")],
    pem: cert.toString() === pems[name] && cert.toJSON() === pems[name],
    publicKey: [cert.publicKey.type, cert.publicKey.asymmetricKeyType, cert.publicKey.export({ format: "der", type: "spki" }).toString("base64")],
    legacyKeys: Object.keys(legacy),
    legacy,
    sameObject: cert.publicKey === cert.publicKey && cert.subject === cert.subject,
  };
}

const { ca, leaf, pss, ec, ed, wild } = certs;
out.verify = {
  leafByCa: leaf.verify(ca.publicKey),
  pssByCa: pss.verify(ca.publicKey),
  caBySelf: ca.verify(ca.publicKey),
  ecBySelf: ec.verify(ec.publicKey),
  edBySelf: ed.verify(ed.publicKey),
  wildBySelf: wild.verify(wild.publicKey),
  leafByLeaf: leaf.verify(leaf.publicKey),
  leafByEc: leaf.verify(ec.publicKey),
  edByEc: ed.verify(ec.publicKey),
  privateKey: attempt(() => leaf.verify(keys.leaf)).slice(0, 2),
  notKey: attempt(() => leaf.verify(ca)),
  secret: attempt(() => leaf.verify(crypto.createSecretKey(Buffer.alloc(8)))).slice(0, 2),
};
out.checkIssued = {
  leafByCa: leaf.checkIssued(ca),
  pssByCa: pss.checkIssued(ca),
  caBySelf: ca.checkIssued(ca),
  caByLeaf: ca.checkIssued(leaf),
  leafByLeaf: leaf.checkIssued(leaf),
  ecBySelf: ec.checkIssued(ec),
  edBySelf: ed.checkIssued(ed),
  edByCa: ed.checkIssued(ca),
  wildBySelf: wild.checkIssued(wild),
  wildByEd: wild.checkIssued(ed),
  notCert: attempt(() => leaf.checkIssued({})),
};
out.checkPrivateKey = {
  leaf: leaf.checkPrivateKey(keys.leaf),
  pss: pss.checkPrivateKey(keys.leaf),
  ec: ec.checkPrivateKey(keys.ec),
  mismatch: ca.checkPrivateKey(keys.leaf),
  otherType: leaf.checkPrivateKey(keys.ec),
  publicKey: attempt(() => leaf.checkPrivateKey(leaf.publicKey)).slice(0, 2),
  notKey: attempt(() => leaf.checkPrivateKey("x")),
};
const host = (cert, name, options) => attempt(() => cert.checkHost(name, options) ?? null);
out.checkHost = [
  host(leaf, "example.com"),
  host(leaf, "EXAMPLE.com"),
  host(leaf, "www.example.org"),
  host(leaf, "a.b.example.org"),
  host(leaf, "a.b.example.org", { multiLabelWildcards: true }),
  host(leaf, "www.example.org", { wildcards: false }),
  host(leaf, "example.org"),
  host(leaf, "leaf.example.com"),
  host(leaf, "leaf.example.com", { subject: "always" }),
  host(leaf, ".example.com"),
  host(leaf, ".example.org"),
  host(leaf, "127.0.0.1"),
  host(ed, "ed25519.example"),
  host(ed, "ed25519.example", { subject: "never" }),
  host(ed, "ED25519.EXAMPLE."),
  host(ca, "Pi Test Root CA"),
  host(ec, "ec.example.net"),
  host(ec, "weird,name.example"),
  host(ec, "a\0b"),
  host(ec, "x", { subject: "sometimes" }),
  host(ec, "x", { wildcards: "no" }),
  host(ec, "x", null),
  host(ec, 1),
  host(wild, "foo.example.com"),
  host(wild, "foo.example.com", { partialWildcards: false }),
  host(wild, "f.example.com"),
  host(wild, "x.xn--bcher-kva.example"),
  host(wild, "xn--abc.a.example.com"),
  host(wild, "b.c.a.example.com"),
  host(wild, "b.c.a.example.com", { multiLabelWildcards: true }),
  host(wild, ".sub.example.net"),
  host(wild, ".example.net"),
  host(wild, ".example.net", { singleLabelSubdomains: true }),
  host(wild, ".sub.example.net", { singleLabelSubdomains: true }),
  host(wild, "x.cn.example"),
  host(wild, "x.cn.example", { subject: "always" }),
  host(wild, "x.cn.example", { subject: "always", wildcards: false }),
  host(wild, "zoë"),
];
const email = (cert, name, options) => attempt(() => cert.checkEmail(name, options) ?? null);
out.checkEmail = [
  email(leaf, "admin@example.com"),
  email(leaf, "admin@EXAMPLE.COM"),
  email(leaf, "Admin@example.com"),
  email(leaf, "other@example.com"),
  email(ec, "quoted@example.com"),
  email(ec, "a@b.c"),
  email(ec, "a@b.c", { subject: "always" }),
  email(ca, "a@b.c"),
  email(ec, "a\0b"),
  email(ec, 5),
  email(wild, "Mixed@example.com"),
  email(wild, "mixed@example.com"),
];
const ip = (cert, name, options) => attempt(() => cert.checkIP(name, options) ?? null);
out.checkIP = [
  ip(leaf, "127.0.0.1"),
  ip(leaf, "2001:db8::1"),
  ip(leaf, "2001:DB8:0:0:0:0:0:1"),
  ip(leaf, "127.0.0.2"),
  ip(leaf, "::ffff:127.0.0.1"),
  ip(ec, "10.0.0.1"),
  ip(ec, "010.0.0.1"),
  ip(ec, "not an ip"),
  ip(ec, "1.2.3"),
  ip(ec, "::"),
  ip(ec, ":::"),
  ip(ec, "10.0.0.1", { subject: "x" }),
  ip(ec, null),
  ip(wild, "::ffff:10.1.2.3"),
  ip(wild, "0:0:0:0:0:FFFF:a01:203"),
  ip(wild, "1::2::3"),
  ip(wild, "1:2:3:4:5:6:7:8:9"),
  ip(wild, "::1.2.3.4"),
  ip(wild, "1:2:3:4:5:6:7::"),
  ip(wild, "1.2.3.4\0junk"),
];

const der = leaf.raw;
const fromDer = new X509Certificate(der);
const view = new Uint8Array(der.buffer, der.byteOffset, der.byteLength);
out.inputs = {
  der: fromDer.fingerprint256 === leaf.fingerprint256 && fromDer.toString() === pems.leaf,
  uint8: new X509Certificate(view).serialNumber,
  dataView: new X509Certificate(new DataView(der.buffer, der.byteOffset, der.byteLength)).serialNumber,
  pemBuffer: new X509Certificate(Buffer.from(pems.ec)).serialNumber,
  keyFirst: new X509Certificate(__LEAF_KEY__ + pems.ed).serialNumber,
  firstOfTwo: new X509Certificate(pems.ed + pems.ca).serialNumber,
  crlf: new X509Certificate(pems.ed.replace(/\n/g, "\r\n")).serialNumber,
  derTrailing: new X509Certificate(Buffer.concat([der, Buffer.from("junk")])).serialNumber,
  x509Label: new X509Certificate(pems.ed.replace(/CERTIFICATE/g, "X509 CERTIFICATE")).serialNumber,
  trustedLabel: new X509Certificate(pems.ed.replace(/CERTIFICATE/g, "TRUSTED CERTIFICATE")).serialNumber,
  textBefore: new X509Certificate("hello\n" + pems.ed).serialNumber,
  spaceAfterBegin: new X509Certificate(pems.ed.replace("CERTIFICATE-----\n", "CERTIFICATE----- \n")).serialNumber,
};
const pem = (bytes) => `-----BEGIN CERTIFICATE-----\n${bytes.toString("base64")}\n-----END CERTIFICATE-----\n`;
const errors = {};
for (const [label, input] of Object.entries({
  undefined: undefined,
  number: 1,
  object: {},
  arrayBuffer: new ArrayBuffer(4),
  empty: "",
  garbage: "garbage",
  garbageBytes: Buffer.from([0x30, 0x03, 1, 2, 3]),
  truncatedDer: der.subarray(0, 100),
  badBase64: "-----BEGIN CERTIFICATE-----\n!!!!\n-----END CERTIFICATE-----\n",
  onlyKey: __LEAF_KEY__,
  emptyPem: "-----BEGIN CERTIFICATE-----\n-----END CERTIFICATE-----\n",
  junkPem: "-----BEGIN CERTIFICATE-----\nMAMCAQE=\n-----END CERTIFICATE-----\n",
  noEnd: pems.ed.replace("-----END CERTIFICATE-----\n", ""),
  otherEnd: pems.ed.replace("END CERTIFICATE", "END FOO"),
  lowercaseLabel: pems.ed.replace(/CERTIFICATE/g, "certificate"),
  commentHeader: pems.ed.replace("-----\n", "-----\nComment: hi\n\n"),
  notEncrypted: pems.ed.replace("-----\n", "-----\nProc-Type: 4,MIC-ONLY\n\n"),
  noDekInfo: pems.ed.replace("-----\n", "-----\nProc-Type: 4,ENCRYPTED\n\n"),
  encrypted: pems.leaf.replace("-----\n", "-----\nProc-Type: 4,ENCRYPTED\nDEK-Info: AES-128-CBC,00112233445566778899AABBCCDDEEFF\n\n"),
  pemTrailing: pem(Buffer.concat([der, Buffer.from([1, 2, 3])])),
  pemTruncated: pem(der.subarray(0, 100)),
  pemOneByte: pem(der.subarray(0, 1)),
  pemPublicKey: pem(leaf.publicKey.export({ format: "der", type: "spki" })),
})) {
  errors[label] = attempt(() => new X509Certificate(input).subject);
}
out.errors = errors;
out.callWithoutNew = attempt(() => X509Certificate(pems.ed)).slice(0, 2);
out.shape = {
  name: X509Certificate.name,
  length: X509Certificate.length,
  instance: leaf instanceof X509Certificate,
  tag: Object.prototype.toString.call(leaf),
  own: Object.keys(leaf),
  methods: Object.getOwnPropertyNames(X509Certificate.prototype).sort(),
  esm: (await import("node:crypto")).X509Certificate === X509Certificate,
};
out.inspect = require("node:util").inspect(ed);
return out;
