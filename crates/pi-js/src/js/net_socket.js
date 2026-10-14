// net.Socket: a Duplex over a native TCP, Unix socket, or named pipe handle.
const { Duplex } = require("stream");
const { Buffer } = require("buffer");
const { codeError, invalidArgType, invalidArgValue, validate } = require("internal/errors");
const { isIP } = require("internal/net_address");
const nativeNet = native.net;

const kAccepted = Symbol("kAccepted");
const defaults = { autoSelectFamily: true, attemptTimeout: 250 };

const toNumber = (value) => ((value = Number(value)) >= 0 ? value : false);
const isPipeName = (value) => typeof value === "string" && toNumber(value) === false;

function badPort(name, port) {
  const shown = typeof port === "string" ? `type string ('${port}')` : `type ${typeof port} (${String(port)})`;
  return codeError(RangeError, "ERR_SOCKET_BAD_PORT", `${name} should be >= 0 and < 65536. Received ${shown}.`);
}

function validatePort(port, name = "Port", allowZero = true) {
  const valid = (typeof port === "number" || (typeof port === "string" && port.trim() !== "")) &&
    +port === +port >>> 0 && +port <= 0xffff && (allowZero || +port !== 0);
  if (!valid) throw badPort(name, port);
  return +port;
}

function normalizeArgs(args) {
  if (args.length === 0) return [{}, null];
  let options = {};
  const first = args[0];
  if (typeof first === "object" && first !== null) options = first;
  else if (isPipeName(first)) options.path = first;
  else {
    options.port = first;
    if (args.length > 1 && typeof args[1] === "string") options.host = args[1];
  }
  const last = args[args.length - 1];
  return [options, typeof last === "function" ? last : null];
}

function Socket(options) {
  if (!(this instanceof Socket)) return new Socket(options);
  options = typeof options === "number" ? { fd: options } : { ...options };
  if (options.fd !== undefined) throw internal.unsupported("net.Socket with fd");
  Duplex.call(this, { ...options, allowHalfOpen: !!options.allowHalfOpen, emitClose: false, autoDestroy: true, decodeStrings: true });
  Object.assign(this, { connecting: false, _handle: null, _hadError: false, _local: null, _remote: null, _path: null });
  Object.assign(this, { _paused: false, _refed: true, _pendingOptions: [], _timeout: 0, _timer: null });
  Object.assign(this, { bytesRead: 0, _bytesWritten: 0, server: null, _server: null });
  if (options[kAccepted]) this._attach(options[kAccepted].id, options[kAccepted].info);
  if (options.signal) {
    const abort = () => this.destroy(Object.assign(new Error("The operation was aborted"), { name: "AbortError", code: "ABORT_ERR", cause: options.signal.reason }));
    if (options.signal.aborted) process.nextTick(abort);
    else options.signal.addEventListener("abort", abort, { once: true });
  }
}
Object.setPrototypeOf(Socket.prototype, Duplex.prototype);
Object.setPrototypeOf(Socket, Duplex);

Socket.prototype._attach = function (id, info) {
  this._handle = id;
  this._applyInfo(info);
  if (!this._refed) nativeNet.ref(id, false);
  nativeNet.start(id, (event, value) => this._onNative(event, value));
};

Socket.prototype._applyInfo = function (info) {
  if (!info) return;
  this._local = info.local || null;
  this._remote = info.remote || null;
  if (info.path) this._path = info.path;
};

Socket.prototype._onNative = function (event, value) {
  if (event === "data") {
    this.bytesRead += value.length;
    this._touch();
    if (!this.push(Buffer.from(value.buffer, value.byteOffset, value.length)) && this._handle !== null && !this._paused) {
      this._paused = true;
      nativeNet.pause(this._handle);
    }
  } else if (event === "end") {
    this.push(null);
  } else if (event === "connect") {
    this._onConnect(value);
  } else if (event === "error") {
    this._hadError = true;
    this.destroy(value);
  }
};

Socket.prototype._onConnect = function (info) {
  this.connecting = false;
  this._applyInfo(info);
  for (const apply of this._pendingOptions.splice(0)) apply();
  this._touch();
  this.emit("connect");
  this.emit("ready");
};

Socket.prototype._read = function () {
  if (this._paused && this._handle !== null) {
    this._paused = false;
    nativeNet.resume(this._handle);
  }
};

Socket.prototype._whenConnected = function (callback) {
  if (!this.connecting) return callback();
  const ready = () => {
    this.removeListener("connect", ready);
    this.removeListener("close", ready);
    callback();
  };
  this.once("connect", ready);
  this.once("close", ready);
};

Socket.prototype._write = function (chunk, encoding, callback) {
  this._whenConnected(() => {
    if (this._handle === null || this.destroyed) return callback(codeError(Error, "ERR_SOCKET_CLOSED", "Socket is closed"));
    nativeNet.write(this._handle, chunk).then(() => {
      this._bytesWritten += chunk.length;
      this._touch();
      callback();
    }, callback);
  });
};

Socket.prototype._final = function (callback) {
  this._whenConnected(() => {
    if (this._handle === null) return callback();
    nativeNet.shutdown(this._handle).then(() => callback(), (error) => (error.code === "ENOTCONN" ? callback() : callback(error)));
  });
};

Socket.prototype._destroy = function (error, callback) {
  this.connecting = false;
  this._clearTimer();
  if (this._handle !== null) {
    nativeNet.close(this._handle);
    this._handle = null;
  }
  callback(error);
  process.nextTick(() => this.emit("close", !!error || this._hadError));
};

Socket.prototype._clearTimer = function () {
  if (this._timer) clearTimeout(this._timer);
  this._timer = null;
};

Socket.prototype._touch = function () {
  if (!this._timeout) return;
  this._clearTimer();
  this._timer = setTimeout(() => this.emit("timeout"), this._timeout);
  if (this._timer.unref) this._timer.unref();
};

Socket.prototype.setTimeout = function (timeout, callback) {
  if (typeof timeout !== "number" || !Number.isFinite(timeout) || timeout < 0) {
    throw codeError(RangeError, "ERR_OUT_OF_RANGE", `The value of "msecs" is out of range. It must be a non-negative finite number. Received ${timeout}`);
  }
  this._timeout = timeout;
  if (timeout === 0) {
    this._clearTimer();
    if (callback) this.removeListener("timeout", callback);
  } else {
    if (callback) this.once("timeout", callback);
    this._touch();
  }
  return this;
};

Socket.prototype._socketOption = function (apply) {
  if (this.connecting || this._handle === null) this._pendingOptions.push(apply);
  else apply();
  return this;
};

Socket.prototype.setNoDelay = function (noDelay = true) {
  return this._socketOption(() => this._handle !== null && nativeNet.setNoDelay(this._handle, !!noDelay));
};

Socket.prototype.setKeepAlive = function (enable = false, initialDelay = 0) {
  return this._socketOption(() => this._handle !== null && nativeNet.setKeepAlive(this._handle, !!enable, Math.floor(Math.max(0, initialDelay) / 1000)));
};

Socket.prototype.ref = function () {
  this._refed = true;
  if (this._handle !== null) nativeNet.ref(this._handle, true);
  return this;
};

Socket.prototype.unref = function () {
  this._refed = false;
  if (this._handle !== null) nativeNet.ref(this._handle, false);
  return this;
};

Socket.prototype.address = function () {
  return this._local ? { ...this._local } : {};
};

Socket.prototype.resetAndDestroy = function () {
  return this.destroy(Object.assign(new Error("read ECONNRESET"), { code: "ECONNRESET" }));
};

Socket.prototype.destroySoon = function () {
  if (this.writable) this.end();
  if (this.writableFinished) this.destroy();
  else this.once("finish", () => this.destroy());
};

const getters = {
  remoteAddress() { return this._remote ? this._remote.address : undefined; },
  remotePort() { return this._remote ? this._remote.port : undefined; },
  remoteFamily() { return this._remote ? this._remote.family : undefined; },
  localAddress() { return this._local ? this._local.address : undefined; },
  localPort() { return this._local ? this._local.port : undefined; },
  localFamily() { return this._local ? this._local.family : undefined; },
  pending() { return this._handle === null || this.connecting; },
  bytesWritten() { return this._bytesWritten; },
  bufferSize() { return this.writableLength; },
  timeout() { return this._timeout || undefined; },
  _connecting() { return this.connecting; },
  readyState() {
    if (this.connecting) return "opening";
    if (this.readable && this.writable) return "open";
    if (this.readable && !this.writable) return "readOnly";
    if (!this.readable && this.writable) return "writeOnly";
    return "closed";
  },
};
for (const [name, get] of Object.entries(getters)) Object.defineProperty(Socket.prototype, name, { configurable: true, get });

// Start the native connection; TLSSocket passes its TLS options as the second argument.
Socket.prototype._nativeConnect = function (request, tls) {
  return nativeNet.connect(request, tls, (event, value) => this._onNative(event, value));
};

Socket.prototype.connect = function (...args) {
  const normalized = Array.isArray(args[0]) && args[0].length === 2 && typeof args[0][0] === "object" ? args[0] : normalizeArgs(args);
  const [options, callback] = normalized;
  if (this.connecting || this._handle !== null) throw codeError(Error, "ERR_SOCKET_CONNECTION_IN_PROGRESS", "Socket connection in progress");
  if (callback) this.once("connect", callback);
  if (options.timeout) this.setTimeout(options.timeout);
  if (options.noDelay) this.setNoDelay(true);
  if (options.keepAlive) this.setKeepAlive(true, options.keepAliveInitialDelay || 0);
  this.connecting = true;
  const tls = this._tlsOptions ? this._tlsOptions(options) : undefined;
  const open = (request) => {
    if (this.destroyed) return;
    try {
      this._handle = this._nativeConnect(request, tls);
      if (!this._refed) nativeNet.ref(this._handle, false);
    } catch (error) {
      process.nextTick(() => this.destroy(error));
    }
  };
  if (options.path !== undefined) {
    validate.string(options.path, "options.path");
    this._path = options.path;
    open({ path: options.path });
    return this;
  }
  const port = options.port;
  if (port === undefined || port === null) throw codeError(TypeError, "ERR_MISSING_ARGS", 'The "options" or "port" or "path" argument must be specified');
  if (typeof port !== "number" && typeof port !== "string") throw invalidArgType("options.port", "of type number or string", port);
  const portNumber = validatePort(port, "Port", true);
  if (options.localAddress !== undefined && !isIP(options.localAddress)) throw invalidArgValue("options.localAddress", options.localAddress);
  const host = options.host || "localhost";
  const autoSelect = options.autoSelectFamily ?? defaults.autoSelectFamily;
  const attemptTimeout = Math.max(10, options.autoSelectFamilyAttemptTimeout ?? defaults.attemptTimeout);
  const request = { localAddress: options.localAddress, localPort: options.localPort, attemptTimeout };
  if (isIP(host)) {
    open({ ...request, addresses: [[host, portNumber]] });
    return this;
  }
  const lookup = options.lookup || require("dns").lookup;
  const family = options.family === undefined ? 0 : options.family;
  lookup(host, { family, hints: options.hints || 0, all: autoSelect && family === 0 }, (error, address, addressFamily) => {
    if (this.destroyed) return;
    const list = Array.isArray(address) ? address : address ? [{ address, family: addressFamily }] : [];
    const first = list[0];
    this.emit("lookup", error || null, first && first.address, first && first.family, host);
    if (error) return this.destroy(error);
    if (!first) return this.destroy(codeError(TypeError, "ERR_INVALID_IP_ADDRESS", `Invalid IP address: ${host}`));
    open({ ...request, addresses: list.map((entry) => [entry.address, portNumber]) });
  });
  return this;
};

module.exports = { Socket, kAccepted, defaults, normalizeArgs, validatePort };
