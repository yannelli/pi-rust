// http.Agent: socket pooling and keep-alive reuse for ClientRequest.
const EventEmitter = require("events");
const net = require("net");
const { validate } = require("internal/errors");

function Agent(options) {
  if (!(this instanceof Agent)) return new Agent(options);
  EventEmitter.call(this);
  this.defaultPort = 80;
  this.protocol = "http:";
  this.options = { __proto__: null, ...options };
  if (this.options.noDelay === undefined) this.options.noDelay = true;
  this.options.path = null;
  Object.assign(this, { requests: { __proto__: null }, sockets: { __proto__: null }, freeSockets: { __proto__: null } });
  this.keepAliveMsecs = this.options.keepAliveMsecs || 1000;
  this.keepAlive = this.options.keepAlive || false;
  this.maxSockets = this.options.maxSockets || Agent.defaultMaxSockets;
  this.maxFreeSockets = this.options.maxFreeSockets || 256;
  this.scheduling = this.options.scheduling || "lifo";
  this.maxTotalSockets = this.options.maxTotalSockets;
  this.totalSocketCount = 0;
  if (this.scheduling !== "fifo" && this.scheduling !== "lifo") {
    throw require("internal/errors").invalidArgValue("scheduling", this.scheduling, "must be one of: 'fifo', 'lifo'");
  }
  if (this.maxTotalSockets !== undefined) validate.integer(this.maxTotalSockets, "maxTotalSockets", 1);
  else this.maxTotalSockets = Infinity;
  this.on("free", (socket, options) => this._onFree(socket, options));
}
Object.setPrototypeOf(Agent.prototype, EventEmitter.prototype);
Object.setPrototypeOf(Agent, EventEmitter);
Agent.defaultMaxSockets = Infinity;

const remove = (list, item) => {
  const index = list ? list.indexOf(item) : -1;
  if (index !== -1) list.splice(index, 1);
  return index !== -1;
};

Object.assign(Agent.prototype, {
  createConnection: net.createConnection,

  getName(options = {}) {
    let name = options.host || "localhost";
    name += ":";
    if (options.port) name += options.port;
    name += ":";
    if (options.localAddress) name += options.localAddress;
    if (options.family === 4 || options.family === 6) name += `:${options.family}`;
    if (options.socketPath) name += `:${options.socketPath}`;
    return name;
  },

  _onFree(socket, options) {
    const name = this.getName(options);
    if (!socket.writable) return socket.destroy();
    const queue = this.requests[name];
    if (queue && queue.length) {
      const req = queue.shift();
      if (!queue.length) delete this.requests[name];
      this.reuseSocket(socket, req);
      return req.onSocket(socket);
    }
    const req = socket._httpMessage;
    if (!req || !req.shouldKeepAlive || !this.keepAlive) return socket.destroy();
    const free = this.freeSockets[name] || [];
    const count = free.length + (this.sockets[name] ? this.sockets[name].length : 0);
    if (this.totalSocketCount > this.maxTotalSockets || count > this.maxSockets || free.length >= this.maxFreeSockets || !this.keepSocketAlive(socket)) {
      return socket.destroy();
    }
    socket._httpMessage = null;
    remove(this.sockets[name], socket);
    if (this.sockets[name] && !this.sockets[name].length) delete this.sockets[name];
    this.freeSockets[name] = free;
    free.push(socket);
  },

  addRequest(req, options, port, localAddress) {
    if (typeof options === "string") options = { __proto__: null, host: options, port, localAddress };
    options = { __proto__: null, ...options, ...this.options };
    if (options.socketPath) options.path = options.socketPath;
    if (!options.servername && options.servername !== "") options.servername = serverName(options, req);
    const name = this.getName(options);
    if (!this.sockets[name]) this.sockets[name] = [];
    const free = this.freeSockets[name];
    let socket;
    if (free) {
      while (free.length && free[0].destroyed) free.shift();
      socket = this.scheduling === "fifo" ? free.shift() : free.pop();
      if (!free.length) delete this.freeSockets[name];
    }
    const count = (free ? free.length : 0) + this.sockets[name].length;
    if (socket) {
      this.reuseSocket(socket, req);
      this.sockets[name].push(socket);
      req.onSocket(socket);
    } else if (count < this.maxSockets && this.totalSocketCount < this.maxTotalSockets) {
      this.createSocket(req, options, (error, created) => (error ? req.onSocket(created, error) : req.onSocket(created)));
    } else {
      if (!this.requests[name]) this.requests[name] = [];
      this.requests[name].push(req);
    }
  },

  createSocket(req, options, callback) {
    options = { __proto__: null, ...options, ...this.options };
    if (options.socketPath) options.path = options.socketPath;
    const name = this.getName(options);
    options._agentKey = name;
    options.encoding = null;
    if (this.keepAlive) options.keepAlive = this.keepAlive;
    options.keepAliveInitialDelay = this.keepAliveMsecs;
    let done = false;
    const oncreate = (error, socket) => {
      if (done) return;
      done = true;
      if (error) return callback(error);
      if (!this.sockets[name]) this.sockets[name] = [];
      this.sockets[name].push(socket);
      this.totalSocketCount++;
      this._install(socket, options);
      callback(null, socket);
    };
    let socket;
    try {
      socket = this.createConnection(options, oncreate);
    } catch (error) {
      return oncreate(error);
    }
    if (socket) oncreate(null, socket);
  },

  _install(socket, options) {
    const onClose = () => {
      this.totalSocketCount--;
      this.removeSocket(socket, options);
    };
    const onTimeout = () => {
      const free = this.freeSockets[this.getName(options)];
      if (free && free.includes(socket)) socket.destroy();
    };
    const onFreeError = () => {
      if (!socket._httpMessage) socket.destroy();
    };
    const onFree = () => this.emit("free", socket, options);
    socket.on("free", onFree);
    socket.on("close", onClose);
    socket.on("timeout", onTimeout);
    socket.on("error", onFreeError);
    socket.once("agentRemove", () => {
      socket.removeListener("free", onFree);
      socket.removeListener("close", onClose);
      socket.removeListener("timeout", onTimeout);
      socket.removeListener("error", onFreeError);
      this.totalSocketCount--;
      this.removeSocket(socket, options);
    });
  },

  removeSocket(socket, options) {
    const name = this.getName(options);
    for (const sets of [this.sockets, this.freeSockets]) {
      if (remove(sets[name], socket) && !sets[name].length) delete sets[name];
    }
    const queue = this.requests[name];
    if (queue && queue.length && (this.sockets[name] || []).length < this.maxSockets) {
      const req = queue.shift();
      if (!queue.length) delete this.requests[name];
      this.createSocket(req, options, (error, created) => (error ? req.onSocket(created, error) : req.onSocket(created)));
    }
  },

  keepSocketAlive(socket) {
    socket.setKeepAlive(true, this.keepAliveMsecs);
    socket.unref();
    let timeout = this.options.timeout || 0;
    let keep = true;
    const res = socket._httpMessage && socket._httpMessage.res;
    const hint = res && /^timeout=(\d+)/.exec(String(res.headers["keep-alive"] || ""));
    if (hint) {
      const serverTimeout = Number(hint[1]) * 1000 - 1000;
      if (serverTimeout <= 0) keep = false;
      else if (serverTimeout < timeout || !timeout) timeout = serverTimeout;
    }
    if (socket.timeout !== timeout) socket.setTimeout(timeout);
    return keep;
  },

  reuseSocket(socket, req) {
    req.reusedSocket = true;
    socket.ref();
  },

  destroy() {
    for (const sets of [this.freeSockets, this.sockets]) {
      for (const name of Object.keys(sets)) for (const socket of sets[name].slice()) socket.destroy();
    }
  },
});

function serverName(options, req) {
  let host = options.host || "localhost";
  const header = req && req.getHeader && req.getHeader("host");
  if (header) host = String(header).startsWith("[") ? String(header).slice(1, String(header).indexOf("]")) : String(header).split(":", 1)[0];
  return net.isIP(host) ? "" : host;
}

const globalAgent = new Agent({ keepAlive: true, scheduling: "lifo", timeout: 5000 });

module.exports = { Agent, globalAgent };
