// node:net: Socket and Server over native TCP, Unix socket, and named pipe handles.
const EventEmitter = require("events");
const { codeError, invalidArgValue, validate } = require("internal/errors");
const { isIP, isIPv4, isIPv6, BlockList, SocketAddress } = require("internal/net_address");
const { Socket, kAccepted, defaults, normalizeArgs, validatePort } = require("internal/net_socket");
const nativeNet = native.net;

function createConnection(...args) {
  const [options, callback] = normalizeArgs(args);
  if (options.timeout) validate.integer(options.timeout, "options.timeout", 0);
  const socket = new Socket(options);
  return socket.connect(options, callback);
}

function Server(options, connectionListener) {
  if (!(this instanceof Server)) return new Server(options, connectionListener);
  EventEmitter.call(this);
  if (typeof options === "function") {
    connectionListener = options;
    options = {};
  } else if (options === undefined || options === null) options = {};
  else validate.object(options, "options");
  if (connectionListener) this.on("connection", connectionListener);
  Object.assign(this, { _connections: 0, _handle: null, _address: null, _unref: false, _closing: false, maxConnections: undefined });
  this.allowHalfOpen = !!options.allowHalfOpen;
  this.pauseOnConnect = !!options.pauseOnConnect;
  this.noDelay = !!options.noDelay;
  this.keepAlive = !!options.keepAlive;
  this.keepAliveInitialDelay = options.keepAliveInitialDelay || 0;
}
Object.setPrototypeOf(Server.prototype, EventEmitter.prototype);
Object.setPrototypeOf(Server, EventEmitter);

Object.defineProperty(Server.prototype, "listening", { configurable: true, get() { return this._handle !== null; } });
Object.defineProperty(Server.prototype, "connections", {
  configurable: true,
  get() { return this._connections; },
  set(value) { this._connections = value; },
});

Server.prototype._makeSocket = function (id, info) {
  return new Socket({ [kAccepted]: { id, info, deferRead: true }, allowHalfOpen: this.allowHalfOpen });
};

Server.prototype._onNative = function (event, value, id) {
  if (event === "connection") {
    if (this.maxConnections !== undefined && this._connections >= this.maxConnections) {
      nativeNet.close(id);
      const local = value.local || {};
      const remote = value.remote || {};
      this.emit("drop", { localAddress: local.address, localPort: local.port, localFamily: local.family, remoteAddress: remote.address, remotePort: remote.port, remoteFamily: remote.family });
      return;
    }
    this._addConnection(this._makeSocket(id, value));
  } else if (event === "clientError") {
    this.emit("tlsClientError", value, null);
  } else if (event === "error") {
    this.emit("error", value);
  }
};

// Count and emit a connection; cluster workers also call this for sockets from the primary.
Server.prototype._addConnection = function (socket) {
  socket.server = socket._server = this;
  if (this.noDelay) socket.setNoDelay(true);
  if (this.keepAlive) socket.setKeepAlive(true, this.keepAliveInitialDelay);
  if (this.pauseOnConnect) socket.pause();
  this._connections++;
  socket.once("close", () => {
    this._connections--;
    this._emitCloseIfDrained();
  });
  this._emitConnection(socket);
};

// Listen on a TCP listener received over IPC.
Server.prototype._adoptHandle = function (handleId) {
  const result = nativeNet.adoptServer(handleId, (event, value, id) => this._onNative(event, value, id));
  this._handle = result.id;
  this._address = result.address;
  process.nextTick(() => this.emit("listening"));
  return this;
};

Server.prototype._emitConnection = function (socket) {
  this.emit("connection", socket);
};

Server.prototype._emitCloseIfDrained = function () {
  if (this._handle !== null || this._connections > 0 || !this._closing) return;
  this._closing = false;
  process.nextTick(() => this.emit("close"));
};

function listenArgs(args) {
  let [options, callback] = normalizeArgs(args);
  if (typeof options.port === "object" && options.port !== null) options = options.port;
  options = { ...options };
  if (typeof args[0] !== "object" || args[0] === null) {
    const skip = options.path !== undefined ? 1 : typeof args[1] === "string" ? 2 : 1;
    const backlog = args.slice(skip).find((arg) => typeof arg === "number");
    if (backlog !== undefined) options.backlog = backlog;
  }
  return [options, callback];
}

Server.prototype.listen = function (...args) {
  const [options, callback] = listenArgs(args);
  if (this._handle !== null) throw codeError(Error, "ERR_SERVER_ALREADY_LISTEN", "Listen method has been called more than once without closing.");
  if (options.handle !== undefined || options.fd !== undefined || options._handle !== undefined) {
    throw internal.unsupported("server.listen with a handle or fd");
  }
  const request = { backlog: options.backlog, ipv6Only: !!options.ipv6Only, reusePort: options.reusePort === true };
  if (options.path !== undefined) validate.string(options.path, "options.path");
  else if (options.port !== undefined && options.port !== null) request.port = validatePort(options.port, "options.port");
  else if (typeof args[0] === "object" && args[0] !== null && !("port" in options) && !("host" in options) && Object.keys(options).length) {
    throw invalidArgValue("options", options, 'must have the property "port" or "path"');
  }
  if (callback) this.once("listening", callback);
  if (options.signal) options.signal.addEventListener("abort", () => this.close(), { once: true });
  const start = () => {
    const clusterWorker = process.env.NODE_UNIQUE_ID !== undefined || internal.clusterWorkerId !== undefined;
    if (clusterWorker && !options.exclusive && !request.reusePort && request.path === undefined && !this._tlsListenOptions) {
      return require("cluster")._getServer(this, request);
    }
    try {
      const tls = this._tlsListenOptions ? this._tlsListenOptions() : undefined;
      const result = nativeNet.listen(request, tls, (event, value, id) => this._onNative(event, value, id));
      this._handle = result.id;
      this._address = result.address;
      if (this._unref) nativeNet.ref(this._handle, false);
      process.nextTick(() => this.emit("listening"));
    } catch (error) {
      process.nextTick(() => this.emit("error", error));
    }
  };
  if (options.path !== undefined) {
    request.path = options.path;
    start();
  } else if (options.host === undefined || options.host === null || isIP(options.host)) {
    if (options.host) request.host = options.host;
    start();
  } else {
    require("dns").lookup(options.host, (error, address) => {
      if (error) return this.emit("error", error);
      request.host = address;
      start();
    });
  }
  return this;
};

Server.prototype.address = function () {
  if (this._handle === null) return null;
  return typeof this._address === "string" ? this._address : { ...this._address };
};

Server.prototype.close = function (callback) {
  if (typeof callback === "function") {
    if (this._handle === null) {
      this.once("close", () => callback(codeError(Error, "ERR_SERVER_NOT_RUNNING", "Server is not running.")));
    } else this.once("close", () => callback());
  }
  if (this._clusterKey !== undefined) require("cluster")._closeServer(this);
  if (this._handle !== null) {
    if (this._handle !== 0) nativeNet.close(this._handle);
    this._handle = null;
  }
  this._closing = true;
  this._emitCloseIfDrained();
  return this;
};

Server.prototype.getConnections = function (callback) {
  process.nextTick(callback, null, this._connections);
  return this;
};

Server.prototype.ref = function () {
  this._unref = false;
  if (this._handle !== null) nativeNet.ref(this._handle, true);
  return this;
};

Server.prototype.unref = function () {
  this._unref = true;
  if (this._handle !== null) nativeNet.ref(this._handle, false);
  return this;
};

Server.prototype[Symbol.asyncDispose] = function () {
  return new Promise((resolve) => (this._handle === null ? resolve() : this.close(() => resolve())));
};

module.exports = {
  Socket,
  Stream: Socket,
  Server,
  BlockList,
  SocketAddress,
  connect: createConnection,
  createConnection,
  createServer: (options, connectionListener) => new Server(options, connectionListener),
  isIP,
  isIPv4,
  isIPv6,
  getDefaultAutoSelectFamily: () => defaults.autoSelectFamily,
  setDefaultAutoSelectFamily(value) {
    validate.boolean(value, "value");
    defaults.autoSelectFamily = value;
  },
  getDefaultAutoSelectFamilyAttemptTimeout: () => defaults.attemptTimeout,
  setDefaultAutoSelectFamilyAttemptTimeout(value) {
    validate.integer(value, "value", 1);
    defaults.attemptTimeout = Math.max(10, value);
  },
  _normalizeArgs: normalizeArgs,
};
