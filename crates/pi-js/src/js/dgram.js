// node:dgram: UDP sockets over native handles, with Node's implicit bind and lookup.
const EventEmitter = require("events");
const { Buffer } = require("buffer");
const { codeError, invalidArgType, validate } = require("internal/errors");
const { isIP } = require("internal/net_address");
const { validatePort } = require("internal/net_socket");
const nativeDgram = native.dgram;

const notRunning = () => codeError(Error, "ERR_SOCKET_DGRAM_NOT_RUNNING", "Not running");
const notConnected = () => codeError(Error, "ERR_SOCKET_DGRAM_NOT_CONNECTED", "Not connected");

function Socket(type, listener) {
  if (!(this instanceof Socket)) return new Socket(type, listener);
  EventEmitter.call(this);
  const options = typeof type === "string" ? { type } : type;
  if (options === null || typeof options !== "object") throw invalidArgType("options", "of type object", type);
  if (options.type !== "udp4" && options.type !== "udp6") {
    throw codeError(TypeError, "ERR_SOCKET_BAD_TYPE", "Bad socket type specified. Valid types are: udp4, udp6");
  }
  this.type = options.type;
  this._native = { reuseAddr: !!options.reuseAddr, ipv6Only: !!options.ipv6Only, recvBufferSize: options.recvBufferSize, sendBufferSize: options.sendBufferSize };
  this._lookup = options.lookup;
  Object.assign(this, { _handle: null, _bindState: "unbound", _connectState: 0, _queue: [], _refed: true, _closed: false, _address: null });
  if (listener) this.on("message", listener);
  if (options.signal) {
    if (options.signal.aborted) process.nextTick(() => this.close());
    else options.signal.addEventListener("abort", () => !this._closed && this.close(), { once: true });
  }
}
Object.setPrototypeOf(Socket.prototype, EventEmitter.prototype);
Object.setPrototypeOf(Socket, EventEmitter);

Socket.prototype._resolve = function (address, callback) {
  if (isIP(address)) return process.nextTick(callback, null, address);
  const lookup = this._lookup || require("dns").lookup;
  lookup(address, { family: this.type === "udp6" ? 6 : 4 }, (error, ip) => callback(error, ip));
};

Socket.prototype._onNative = function (event, value, info) {
  if (event === "message") this.emit("message", Buffer.from(value.buffer, value.byteOffset, value.length), info);
  else if (event === "error") this.emit("error", value);
};

Socket.prototype._afterBind = function (callback) {
  if (this._bindState === "bound") callback();
  else this._queue.push(callback);
};

Socket.prototype.bind = function (port, address, callback) {
  if (this._closed) throw notRunning();
  if (this._bindState !== "unbound") throw codeError(Error, "ERR_SOCKET_ALREADY_BOUND", "Socket is already bound");
  let options = {};
  const args = [port, address, callback];
  const last = args.filter((arg) => arg !== undefined).pop();
  if (typeof last === "function") this.once("listening", last);
  if (port !== null && typeof port === "object") {
    options = port;
    if (options.fd !== undefined) throw internal.unsupported("dgram socket.bind with fd");
  } else options = { port: typeof port === "function" ? 0 : port, address: typeof address === "function" ? undefined : address };
  const portNumber = options.port === undefined || options.port === null ? 0 : validatePort(options.port, "Port");
  const host = options.address || (this.type === "udp6" ? "::" : "0.0.0.0");
  this._bindState = "binding";
  this._resolve(host, (error, ip) => {
    if (error) {
      this._bindState = "unbound";
      return this.emit("error", error);
    }
    if (this._closed) return;
    try {
      const result = nativeDgram.bind({ ...this._native, address: ip, port: portNumber }, (event, value, info) => this._onNative(event, value, info));
      this._handle = result.id;
      this._address = result.address;
      this._bindState = "bound";
      if (!this._refed) nativeDgram.ref(this._handle, false);
    } catch (bindError) {
      this._bindState = "unbound";
      return this.emit("error", bindError);
    }
    this.emit("listening");
    for (const queued of this._queue.splice(0)) queued();
  });
  return this;
};

Socket.prototype._ensureBound = function () {
  if (this._bindState === "unbound") this.bind({ port: 0 });
};

function toBuffer(buffer) {
  if (typeof buffer === "string") return Buffer.from(buffer);
  if (Buffer.isBuffer(buffer)) return buffer;
  if (ArrayBuffer.isView(buffer)) return Buffer.from(buffer.buffer, buffer.byteOffset, buffer.byteLength);
  if (Array.isArray(buffer)) return Buffer.concat(buffer.map(toBuffer));
  throw invalidArgType("buffer", "of type string or an instance of Buffer, TypedArray, or DataView", buffer);
}

Socket.prototype.send = function (buffer, offset, length, port, address, callback) {
  if (this._closed) throw notRunning();
  let bytes = toBuffer(buffer);
  if (address || (port && typeof port !== "function")) {
    bytes = bytes.subarray(offset >>> 0, (offset >>> 0) + (length >>> 0));
  } else {
    callback = port;
    port = offset;
    address = length;
  }
  if (typeof port === "function") {
    callback = port;
    port = undefined;
  }
  if (typeof address === "function") {
    callback = address;
    address = undefined;
  }
  const connected = this._connectState === 2;
  if (connected && port !== undefined && port !== null) {
    throw codeError(Error, "ERR_SOCKET_DGRAM_IS_CONNECTED", "Already connected");
  }
  if (!connected) port = validatePort(port, "Port", false);
  if (address !== undefined && address !== null) validate.string(address, "address");
  const report = (error, sent) => {
    if (callback) callback(error, sent);
    else if (error) this.emit("error", error);
  };
  this._ensureBound();
  this._afterBind(() => {
    const deliver = (ip) => (ip === undefined ? nativeDgram.send(this._handle, bytes) : nativeDgram.send(this._handle, bytes, ip, port)).then((sent) => report(null, sent), (error) => report(error));
    if (connected || this._connectState === 2) return deliver(undefined);
    this._resolve(address || (this.type === "udp6" ? "::1" : "127.0.0.1"), (error, ip) => (error ? report(error) : deliver(ip)));
  });
};

Socket.prototype.connect = function (port, address, callback) {
  port = validatePort(port, "Port", false);
  if (typeof address === "function") {
    callback = address;
    address = undefined;
  }
  if (this._connectState !== 0) throw codeError(Error, "ERR_SOCKET_DGRAM_IS_CONNECTED", "Already connected");
  this._connectState = 1;
  if (callback) this.once("connect", callback);
  this._ensureBound();
  this._afterBind(() => {
    this._resolve(address || (this.type === "udp6" ? "::1" : "127.0.0.1"), (error, ip) => {
      const fail = (failure) => {
        this._connectState = 0;
        this.emit("error", failure);
      };
      if (error) return fail(error);
      nativeDgram.connect(this._handle, ip, port).then(() => {
        this._connectState = 2;
        this.emit("connect");
      }, fail);
    });
  });
};

Socket.prototype.disconnect = function () {
  if (this._closed) throw notRunning();
  if (this._connectState !== 2) throw notConnected();
  nativeDgram.disconnect(this._handle);
  this._connectState = 0;
};

Socket.prototype.remoteAddress = function () {
  if (this._closed) throw notRunning();
  if (this._connectState !== 2) throw notConnected();
  return nativeDgram.remoteAddress(this._handle);
};

Socket.prototype.address = function () {
  if (this._closed) throw notRunning();
  if (this._bindState !== "bound") throw Object.assign(new Error("getsockname EBADF"), { errno: -9, code: "EBADF", syscall: "getsockname" });
  return { ...this._address };
};

Socket.prototype.close = function (callback) {
  if (this._closed) throw notRunning();
  if (typeof callback === "function") this.once("close", callback);
  this._closed = true;
  if (this._handle !== null) nativeDgram.close(this._handle);
  this._handle = null;
  this._queue.length = 0;
  process.nextTick(() => this.emit("close"));
  return this;
};

function checkNumber(value, name) {
  if (typeof value !== "number") throw invalidArgType(name, "of type number", value);
}

function handleOf(socket) {
  if (socket._closed) throw notRunning();
  return socket._handle === null ? 0 : socket._handle;
}

function sourceMembership(socket, sourceAddress, groupAddress, interfaceAddress, join) {
  const handle = handleOf(socket);
  validate.string(sourceAddress, "sourceAddress");
  validate.string(groupAddress, "groupAddress");
  const iface = typeof interfaceAddress === "string" ? interfaceAddress : undefined;
  nativeDgram.sourceMembership(handle, sourceAddress, groupAddress, iface, join);
}

Object.assign(Socket.prototype, {
  setBroadcast(flag) {
    nativeDgram.setBroadcast(handleOf(this), !!flag);
  },
  setTTL(ttl) {
    checkNumber(ttl, "ttl");
    nativeDgram.setTTL(handleOf(this), ttl, this.type === "udp6");
    return ttl;
  },
  setMulticastTTL(ttl) {
    checkNumber(ttl, "ttl");
    nativeDgram.setMulticastTTL(handleOf(this), ttl, this.type === "udp6");
    return ttl;
  },
  setMulticastLoopback(flag) {
    nativeDgram.setMulticastLoopback(handleOf(this), !!flag, this.type === "udp6");
    return flag;
  },
  setMulticastInterface(interfaceAddress) {
    validate.string(interfaceAddress, "interfaceAddress");
    nativeDgram.setMulticastInterface(handleOf(this), interfaceAddress);
  },
  addMembership(multicastAddress, interfaceAddress) {
    if (!multicastAddress) throw codeError(TypeError, "ERR_MISSING_ARGS", 'The "multicastAddress" argument must be specified');
    nativeDgram.addMembership(handleOf(this), multicastAddress, interfaceAddress);
  },
  dropMembership(multicastAddress, interfaceAddress) {
    if (!multicastAddress) throw codeError(TypeError, "ERR_MISSING_ARGS", 'The "multicastAddress" argument must be specified');
    nativeDgram.dropMembership(handleOf(this), multicastAddress, interfaceAddress);
  },
  addSourceSpecificMembership(sourceAddress, groupAddress, interfaceAddress) {
    sourceMembership(this, sourceAddress, groupAddress, interfaceAddress, true);
  },
  dropSourceSpecificMembership(sourceAddress, groupAddress, interfaceAddress) {
    sourceMembership(this, sourceAddress, groupAddress, interfaceAddress, false);
  },
  getRecvBufferSize() {
    return nativeDgram.bufferSize(handleOf(this), true);
  },
  getSendBufferSize() {
    return nativeDgram.bufferSize(handleOf(this), false);
  },
  setRecvBufferSize(size) {
    nativeDgram.bufferSize(handleOf(this), true, size);
  },
  setSendBufferSize(size) {
    nativeDgram.bufferSize(handleOf(this), false, size);
  },
  getSendQueueSize() {
    return 0;
  },
  getSendQueueCount() {
    return 0;
  },
  ref() {
    this._refed = true;
    if (this._handle !== null) nativeDgram.ref(this._handle, true);
    return this;
  },
  unref() {
    this._refed = false;
    if (this._handle !== null) nativeDgram.ref(this._handle, false);
    return this;
  },
  [Symbol.asyncDispose]() {
    return this._closed ? Promise.resolve() : new Promise((resolve) => this.close(resolve));
  },
});

module.exports = {
  Socket,
  createSocket: (type, listener) => new Socket(type, listener),
};
