// node:tls: TLSSocket, Server, and SecureContext over the native net handles with rustls.
const { Buffer } = require("buffer");
const net = require("net");
const { codeError, invalidArgType, validate } = require("internal/errors");
const BUFFER_TYPES = "an instance of Buffer, TypedArray, or DataView";
const { isIP } = require("internal/net_address");
const { Socket, kAccepted, normalizeArgs } = require("internal/net_socket");
const wrapper = require("internal/tls_wrap");
const nativeTls = native.tls;

const OPENSSL_NAMES = {
  TLS_AES_128_GCM_SHA256: "TLS_AES_128_GCM_SHA256",
  TLS_AES_256_GCM_SHA384: "TLS_AES_256_GCM_SHA384",
  TLS_CHACHA20_POLY1305_SHA256: "TLS_CHACHA20_POLY1305_SHA256",
  TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256: "ECDHE-ECDSA-AES128-GCM-SHA256",
  TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384: "ECDHE-ECDSA-AES256-GCM-SHA384",
  TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256: "ECDHE-ECDSA-CHACHA20-POLY1305",
  TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256: "ECDHE-RSA-AES128-GCM-SHA256",
  TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384: "ECDHE-RSA-AES256-GCM-SHA384",
  TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256: "ECDHE-RSA-CHACHA20-POLY1305",
};
const VERSIONS = ["TLSv1", "TLSv1.1", "TLSv1.2", "TLSv1.3"];
let rootCertificates;

function pemList(value, name) {
  if (value === undefined || value === null) return undefined;
  const list = Array.isArray(value) ? value : [value];
  return list.map((item, index) => {
    if (typeof item === "string") return item;
    if (ArrayBuffer.isView(item)) return Buffer.from(item.buffer, item.byteOffset, item.byteLength).toString("latin1");
    if (item && typeof item === "object" && item.pem !== undefined) return pemList(item.pem, name)[0];
    throw invalidArgType(Array.isArray(value) ? `options.${name}[${index}]` : `options.${name}`, "of type string or an instance of Buffer, TypedArray, or DataView", item);
  });
}

function alpnList(protocols) {
  if (protocols === undefined || protocols === null) return [];
  if (Array.isArray(protocols)) return protocols.map((item) => (typeof item === "string" ? item : Buffer.from(item).toString("latin1")));
  if (ArrayBuffer.isView(protocols)) {
    const bytes = Buffer.from(protocols.buffer, protocols.byteOffset, protocols.byteLength);
    const list = [];
    for (let index = 0; index < bytes.length; index += bytes[index] + 1) list.push(bytes.subarray(index + 1, index + 1 + bytes[index]).toString("latin1"));
    return list;
  }
  throw invalidArgType("options.ALPNProtocols", "an instance of Array, Buffer, TypedArray, or DataView", protocols);
}

function convertALPNProtocols(protocols, out) {
  const list = alpnList(protocols);
  out.ALPNProtocols = Buffer.concat(list.map((name) => Buffer.concat([Buffer.from([name.length]), Buffer.from(name, "latin1")])));
}

function SecureContext(options = {}) {
  if (!(this instanceof SecureContext)) return new SecureContext(options);
  if (options.pfx !== undefined) throw internal.unsupported("tls pfx option");
  for (const name of ["minVersion", "maxVersion"]) {
    if (options[name] !== undefined && !VERSIONS.includes(options[name])) {
      throw codeError(TypeError, "ERR_TLS_INVALID_PROTOCOL_VERSION", `${JSON.stringify(options[name])} is not a valid ${name.slice(0, 3)} TLS protocol version`);
    }
  }
  const keys = pemList(options.key, "key");
  this.context = {};
  this._options = {
    ca: pemList(options.ca, "ca"),
    cert: pemList(options.cert, "cert") && pemList(options.cert, "cert").join("\n"),
    key: keys && keys.map((pem, index) => plainKey(pem, keyPassphrase(options, index)))[0],
    minVersion: options.minVersion || tlsExports.DEFAULT_MIN_VERSION,
    maxVersion: options.maxVersion || tlsExports.DEFAULT_MAX_VERSION,
  };
}

// The passphrase of key `index`, as Node's setKey checks it: its own `{ pem, passphrase }` in a
// key array, else `options.passphrase`.
function keyPassphrase(options, index) {
  const item = Array.isArray(options.key) ? options.key[index] : undefined;
  const own = item && typeof item === "object" && !ArrayBuffer.isView(item) ? item.passphrase : undefined;
  const passphrase = own !== undefined ? own : options.passphrase;
  if (passphrase !== undefined && passphrase !== null) validate.string(passphrase, "options.passphrase");
  return passphrase;
}

// rustls takes no encrypted keys, so an encrypted key is decrypted here, where Node fails too.
function plainKey(pem, passphrase) {
  if (!/-----BEGIN ENCRYPTED PRIVATE KEY-----|Proc-Type:\s*4,ENCRYPTED/.test(pem)) return pem;
  return nativeTls.decryptKey(pem, passphrase === undefined || passphrase === null ? undefined : String(passphrase));
}

function createSecureContext(options) {
  return new SecureContext(options);
}

function contextOptions(options) {
  const context = options.secureContext instanceof SecureContext ? options.secureContext : new SecureContext(options);
  return { ...context._options };
}

function unfqdn(host) {
  return host.replace(/[.]$/, "");
}

function splitHost(host) {
  return unfqdn(host).replace(/[A-Z]+/g, (letters) => letters.toLowerCase()).split(".");
}

function checkPattern(hostParts, pattern, wildcards) {
  if (!pattern) return false;
  const patternParts = splitHost(pattern);
  if (hostParts.length !== patternParts.length || patternParts.includes("")) return false;
  if (patternParts.some((part) => /[^!-\u007F]/u.test(part))) return false;
  for (let index = hostParts.length - 1; index > 0; index -= 1) if (hostParts[index] !== patternParts[index]) return false;
  const hostSubdomain = hostParts[0];
  const patternSubdomain = patternParts[0];
  const pieces = patternSubdomain.split("*", 3);
  if (pieces.length === 1 || patternSubdomain.includes("xn--")) return hostSubdomain === patternSubdomain;
  if (!wildcards || pieces.length > 2 || patternParts.length <= 2) return false;
  const [prefix, suffix] = pieces;
  return prefix.length + suffix.length <= hostSubdomain.length && hostSubdomain.startsWith(prefix) && hostSubdomain.endsWith(suffix);
}

function canonicalIP(ip) {
  if (isIP(ip) !== 6) return ip;
  return new net.SocketAddress({ address: ip, family: "ipv6" }).address.toLowerCase();
}

function checkServerIdentity(hostname, cert) {
  const subject = cert.subject;
  const altNames = cert.subjectaltname;
  const dnsNames = [];
  const ips = [];
  hostname = `${hostname}`;
  if (altNames) {
    for (const name of altNames.split(", ")) {
      if (name.startsWith("DNS:")) dnsNames.push(name.slice(4));
      else if (name.startsWith("IP Address:")) ips.push(canonicalIP(name.slice(11)));
    }
  }
  let valid = false;
  let reason = "Unknown reason";
  hostname = unfqdn(hostname);
  if (isIP(hostname)) {
    valid = ips.includes(canonicalIP(hostname));
    if (!valid) reason = `IP: ${hostname} is not in the cert's list: ${ips.join(", ")}`;
  } else if (dnsNames.length > 0 || (subject && subject.CN)) {
    const hostParts = splitHost(hostname);
    const wildcard = (pattern) => checkPattern(hostParts, pattern, true);
    if (dnsNames.length > 0) {
      valid = dnsNames.some(wildcard);
      if (!valid) reason = `Host: ${hostname}. is not in the cert's altnames: ${altNames}`;
    } else {
      const cn = subject.CN;
      valid = Array.isArray(cn) ? cn.some(wildcard) : wildcard(cn);
      if (!valid) reason = `Host: ${hostname}. is not cert's CN: ${cn}`;
    }
  } else {
    reason = "Cert does not contain a DNS name";
  }
  if (valid) return undefined;
  return Object.assign(new Error(`Hostname/IP does not match certificate's altnames: ${reason}`), { code: "ERR_TLS_CERT_ALTNAME_INVALID", reason, host: hostname, cert });
}

function certificate(entry, chain, detailed) {
  if (!entry) return {};
  const cert = { ...entry, raw: Buffer.from(entry.raw, "hex") };
  if (detailed) {
    const next = chain[chain.indexOf(entry) + 1];
    cert.issuerCertificate = next ? certificate(next, chain, true) : cert;
  }
  return cert;
}

function sessionHex(session) {
  if (session === undefined || session === null) return undefined;
  if (!ArrayBuffer.isView(session)) throw invalidArgType("session", "an instance of Buffer, TypedArray, or DataView", session);
  return Buffer.from(session.buffer, session.byteOffset, session.byteLength).toString("hex");
}

// Native options for a server-side handshake, from Server or TLSSocket settings.
function serverTlsOptions(settings) {
  const rejectUnauthorized = settings.rejectUnauthorized !== false;
  return { ...contextOptions(settings), requestCert: !!settings.requestCert, rejectUnauthorized, alpn: alpnList(settings.ALPNProtocols) };
}

function TLSSocket(socket, options) {
  if (!(this instanceof TLSSocket)) return new TLSSocket(socket, options);
  options = { ...options };
  const wrapped = socket !== undefined && socket !== null && !options[kAccepted];
  this._tlsSettings = options;
  this._tls = this._tls || null;
  this._sessions = [];
  Socket.call(this, wrapped ? { allowHalfOpen: options.allowHalfOpen } : options);
  this.encrypted = true;
  this.authorized = this._tls ? !!this._tls.authorized : false;
  this.authorizationError = null;
  this.alpnProtocol = this._tls ? this._tls.alpnProtocol || false : null;
  this.servername = this._tls ? this._tls.servername || false : null;
  this._secureEstablished = !!this._tls;
  if (wrapped) {
    wrapper.attach(this, socket);
    // A client starts from tls.connect; a server waits for the ClientHello at once.
    if (options.isServer) wrapper.start(this, () => serverTlsOptions(this._tlsSettings), true);
  }
}
Object.setPrototypeOf(TLSSocket.prototype, Socket.prototype);
Object.setPrototypeOf(TLSSocket, Socket);

TLSSocket.prototype._start = function () {
  if (this._wrap) wrapper.start(this, () => this._tlsOptions(this._tlsSettings), false);
};

TLSSocket.prototype._onNative = function (event, value, error) {
  if (event === "wire" || event === "wireEnd") return wrapper.wireOut(this, event, value);
  if (event === "session") {
    const session = Buffer.from(value.buffer, value.byteOffset, value.byteLength);
    this._session = session;
    // Node reports sessions after secureConnect, also those from a TLS 1.2 handshake.
    if (this._secureEstablished) this.emit("session", session);
    else this._sessions.push(session);
    return;
  }
  return Socket.prototype._onNative.call(this, event, value, error);
};

TLSSocket.prototype._whenConnected = function (callback) {
  if (!this._wrap || this._wrap.ready) return Socket.prototype._whenConnected.call(this, callback);
  wrapper.whenSecure(this, callback);
};

TLSSocket.prototype._destroy = function (error, callback) {
  Socket.prototype._destroy.call(this, error, callback);
  if (this._wrap) wrapper.release(this);
};

TLSSocket.prototype._applyInfo = function (info) {
  Socket.prototype._applyInfo.call(this, info);
  if (info && info.tls) this._tls = info.tls;
};

TLSSocket.prototype._tlsOptions = function (options) {
  const host = options.host || "localhost";
  const servername = options.servername || host;
  const rejectUnauthorized = options.rejectUnauthorized !== undefined ? options.rejectUnauthorized !== false : process.env.NODE_TLS_REJECT_UNAUTHORIZED !== "0";
  this._tlsSettings = { ...this._tlsSettings, ...options, rejectUnauthorized, servername: options.servername || (isIP(host) ? undefined : host) };
  const session = sessionHex(options.session === undefined ? this._tlsSettings.session : options.session);
  return { ...contextOptions(options), servername, rejectUnauthorized, alpn: alpnList(options.ALPNProtocols), session };
};

TLSSocket.prototype._onConnect = function (info) {
  const wrap = this._wrap;
  if (wrap) info = wrapper.parentInfo(this, info);
  this._applyInfo(info);
  const tls = this._tls || {};
  const settings = this._tlsSettings;
  this.alpnProtocol = tls.alpnProtocol || false;
  this.servername = settings.isServer ? tls.servername || false : settings.servername || false;
  this._secureEstablished = true;
  if (wrap) {
    // The parent's 'connect' already came; the handshake only ends the connecting state.
    wrap.ready = true;
    this.connecting = false;
    this._touch();
  } else {
    Socket.prototype._onConnect.call(this, info);
  }
  if (settings.isServer) {
    this.authorized = !!tls.authorized;
    return this.emit("secure");
  }
  let verifyError = null;
  if (tls.authorizationError) {
    verifyError = codeError(Error, tls.authorizationError[0], tls.authorizationError[1]);
  } else {
    const check = settings.checkServerIdentity || checkServerIdentity;
    const hostname = settings.servername || settings.host || "localhost";
    verifyError = check(hostname, this.getPeerCertificate(true)) || null;
  }
  // tls.connect checks the peer in the first 'secure' listener, so secureConnect comes first.
  if (verifyError) {
    this.authorized = false;
    this.authorizationError = verifyError.code || verifyError.message;
    if (settings.rejectUnauthorized) {
      this.destroy(verifyError);
      return this.emit("secure");
    }
  } else {
    this.authorized = true;
  }
  this.emit("secureConnect");
  this.emit("secure");
  this._flushSessions();
};

TLSSocket.prototype._flushSessions = function () {
  for (const session of this._sessions.splice(0)) this.emit("session", session);
};

Object.assign(TLSSocket.prototype, {
  getPeerCertificate(detailed) {
    const chain = (this._tls && this._tls.peerChain) || [];
    return certificate(chain[0], chain, detailed);
  },
  getCertificate() {
    return null;
  },
  getProtocol() {
    return this._tls ? this._tls.protocol : null;
  },
  getCipher() {
    if (!this._tls || !this._tls.cipher) return undefined;
    const { standardName, version } = this._tls.cipher;
    return { name: OPENSSL_NAMES[standardName] || standardName, standardName, version: version === "TLSv1.3" ? "TLSv1.3" : "TLSv1.2" };
  },
  getSharedSigalgs() {
    return [];
  },
  getEphemeralKeyInfo() {
    return {};
  },
  getSession() {
    if (this._handle === null || !this._tls || !this._tls.session) return undefined;
    return this._session || Buffer.from(this._tls.session, "hex");
  },
  getTLSTicket() {
    return undefined;
  },
  isSessionReused() {
    return !!(this._tls && this._tls.reused);
  },
  getFinished() {
    return undefined;
  },
  getPeerFinished() {
    return undefined;
  },
  getX509Certificate() {
    return undefined;
  },
  getPeerX509Certificate() {
    return undefined;
  },
  setServername(name) {
    validate.string(name, "name");
    this._tlsSettings.servername = name;
  },
  setSession(session) {
    sessionHex(session);
    this._tlsSettings.session = session;
  },
  setMaxSendFragment() {
    return true;
  },
  disableRenegotiation() {},
  enableTrace() {},
  renegotiate(options, callback) {
    if (callback) process.nextTick(callback, codeError(Error, "ERR_TLS_RENEGOTIATION_DISABLED", "TLS session renegotiation disabled for this socket"));
    return false;
  },
  exportKeyingMaterial(length, label, context) {
    validate.integer(length, "length", 1, 4294967295);
    validate.string(label, "label");
    if (context !== undefined && !ArrayBuffer.isView(context)) throw invalidArgType("context", BUFFER_TYPES, context);
    if (this._handle === null || !this._tls) {
      throw codeError(Error, "ERR_TLS_INVALID_STATE", "TLS socket connection must be securely established");
    }
    const bytes = context === undefined ? undefined : new Uint8Array(context.buffer, context.byteOffset, context.byteLength);
    return Buffer.from(nativeTls.exportKeyingMaterial(this._handle, length, label, bytes));
  },
});

function connect(...args) {
  let [options, callback] = normalizeArgs(args);
  if (typeof args[0] !== "object" && args.length > 1) {
    const extra = args.find((arg, index) => index > 0 && arg && typeof arg === "object");
    if (extra) options = { ...extra, ...options };
  }
  options = { ...options };
  if (options.socket) {
    const socket = new TLSSocket(options.socket, { ...options, isServer: false });
    if (callback) socket.once("secureConnect", callback);
    if (options.timeout) socket.setTimeout(options.timeout);
    socket._start();
    return socket;
  }
  if (options.host === undefined && options.path === undefined) options.host = "localhost";
  const socket = new TLSSocket(null, options);
  if (callback) socket.once("secureConnect", callback);
  if (options.timeout) socket.setTimeout(options.timeout);
  socket.connect(options);
  return socket;
}

function Server(options, listener) {
  if (!(this instanceof Server)) return new Server(options, listener);
  if (typeof options === "function") {
    listener = options;
    options = {};
  }
  options = { ...options };
  net.Server.call(this, options);
  if (listener) this.on("secureConnection", listener);
  this._tlsSettings = options;
  this._contexts = [];
  if (options.SNICallback !== undefined && options.SNICallback !== null && typeof options.SNICallback !== "function") {
    throw invalidArgType("options.SNICallback", "of type function", options.SNICallback);
  }
  this._sniFlag = null;
}
Object.setPrototypeOf(Server.prototype, net.Server.prototype);
Object.setPrototypeOf(Server, net.Server);

Object.assign(Server.prototype, {
  _tlsListenOptions() {
    // Asking JavaScript for a context costs a hop per handshake, so only servers with SNI use it.
    this._sniFlag ??= nativeTls.sniFlag(false);
    nativeTls.setSni(this._sniFlag, !!this._tlsSettings.SNICallback || this._contexts.length > 0);
    return { ...serverTlsOptions(this._tlsSettings), sniFlag: this._sniFlag };
  },
  _onNative(event, value, id) {
    if (event !== "sni") return net.Server.prototype._onNative.call(this, event, value, id);
    let answered = false;
    const reply = (error, context) => {
      if (answered) return;
      answered = true;
      const options = context ? { ...serverTlsOptions(this._tlsSettings), ...(context instanceof SecureContext ? context : new SecureContext(context))._options } : undefined;
      nativeTls.sniDone(id, error || null, options);
    };
    try {
      (this._tlsSettings.SNICallback || defaultSNICallback).call(this, value, reply);
    } catch (error) {
      reply(error);
    }
  },
  _makeSocket(id, info) {
    const socket = new TLSSocket(null, { [kAccepted]: { id, info }, allowHalfOpen: this.allowHalfOpen });
    socket.authorized = !!(info.tls && info.tls.authorized);
    if (!socket.authorized && this._tlsSettings.requestCert) socket.authorizationError = "UNABLE_TO_GET_ISSUER_CERT";
    return socket;
  },
  _emitConnection(socket) {
    this.emit("secureConnection", socket);
  },
  setSecureContext(options) {
    this._tlsSettings = { ...this._tlsSettings, ...options };
  },
  addContext(servername, context) {
    if (!servername) throw codeError(Error, "ERR_TLS_REQUIRED_SERVER_NAME", '"servername" is required parameter for Server.addContext');
    const pattern = new RegExp(`^${servername.replace(/([.^$+?\-\\[\]{}])/g, "\\$1").replaceAll("*", "[^.]*")}$`);
    this._contexts.push([pattern, context instanceof SecureContext ? context : createSecureContext(context)]);
    if (this._sniFlag !== null) nativeTls.setSni(this._sniFlag, true);
  },
  getTicketKeys() {
    return Buffer.alloc(48);
  },
  setTicketKeys() {},
});

// Node's default SNICallback: the most recently added context whose name pattern matches.
function defaultSNICallback(servername, callback) {
  for (let index = this._contexts.length - 1; index >= 0; index--) {
    if (this._contexts[index][0].test(servername)) return callback(null, this._contexts[index][1]);
  }
  callback(null, undefined);
}

const tlsExports = {
  CLIENT_RENEG_LIMIT: 3,
  CLIENT_RENEG_WINDOW: 600,
  DEFAULT_CIPHERS: "TLS_AES_256_GCM_SHA384:TLS_CHACHA20_POLY1305_SHA256:TLS_AES_128_GCM_SHA256:ECDHE-RSA-AES128-GCM-SHA256:ECDHE-ECDSA-AES128-GCM-SHA256:ECDHE-RSA-AES256-GCM-SHA384:ECDHE-ECDSA-AES256-GCM-SHA384:ECDHE-RSA-CHACHA20-POLY1305:ECDHE-ECDSA-CHACHA20-POLY1305",
  DEFAULT_ECDH_CURVE: "auto",
  DEFAULT_MIN_VERSION: "TLSv1.2",
  DEFAULT_MAX_VERSION: "TLSv1.3",
  SecureContext,
  TLSSocket,
  Server,
  checkServerIdentity,
  connect,
  convertALPNProtocols,
  createSecureContext,
  createServer: (options, listener) => new Server(options, listener),
  getCiphers: () => Object.values(OPENSSL_NAMES).map((name) => name.toLowerCase()).sort(),
};
Object.defineProperty(tlsExports, "rootCertificates", {
  enumerable: true,
  get() {
    if (!rootCertificates) rootCertificates = Object.freeze(nativeTls.rootCertificates());
    return rootCertificates;
  },
});
module.exports = tlsExports;
