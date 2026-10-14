// http.ClientRequest: an outgoing request over an Agent's socket, and its response parser.
const net = require("net");
const { Buffer } = require("buffer");
const { codeError, invalidArgType } = require("internal/errors");
const { IncomingMessage, Parser, SKIP_BODY, UPGRADE, checkIsHttpToken } = require("internal/http_common");
const { OutgoingMessage, kHeaders } = require("internal/http_outgoing");

const kOptions = Symbol("kRequestOptions");
const NO_BODY = new Set(["GET", "HEAD", "DELETE", "OPTIONS", "TRACE", "CONNECT"]);
const hangUp = (message = "socket hang up") => Object.assign(new Error(message), { code: "ECONNRESET" });

function validateHost(host, name) {
  if (host !== null && host !== undefined && typeof host !== "string") throw invalidArgType(`options.${name}`, "of type string", host);
  return host;
}

function ClientRequest(input, options, callback) {
  if (!(this instanceof ClientRequest)) return new ClientRequest(input, options, callback);
  OutgoingMessage.call(this);
  if (typeof input === "string") input = require("url").urlToHttpOptions(new URL(input));
  else if (input instanceof URL) input = require("url").urlToHttpOptions(input);
  else {
    callback = options;
    options = input;
    input = null;
  }
  if (typeof options === "function") {
    callback = options;
    options = input || {};
  } else options = { ...input, ...options };
  const defaultAgent = options._defaultAgent || require("http").globalAgent;
  let agent = options.agent;
  if (agent === false) agent = new defaultAgent.constructor();
  else if (agent === null || agent === undefined) {
    if (typeof options.createConnection !== "function") agent = defaultAgent;
  } else if (typeof agent.addRequest !== "function") {
    throw invalidArgType("options.agent", "of type Agent-like Object, undefined, or false", agent);
  }
  this.agent = agent;
  const protocol = options.protocol || defaultAgent.protocol;
  const expected = (agent && agent.protocol) || defaultAgent.protocol;
  if (options.path && /[\u0000- ]/.test(String(options.path))) throw codeError(TypeError, "ERR_UNESCAPED_CHARACTERS", "Request path contains unescaped characters");
  if (protocol !== expected) throw codeError(TypeError, "ERR_INVALID_PROTOCOL", `Protocol "${protocol}" not supported. Expected "${expected}"`);
  const defaultPort = options.defaultPort || (agent && agent.defaultPort);
  const port = (options.port = options.port || defaultPort || 80);
  const host = (options.host = validateHost(options.hostname, "hostname") || validateHost(options.host, "host") || "localhost");
  const setHost = options.setHost === undefined || !!options.setHost;
  this.socketPath = options.socketPath;
  if (options.timeout !== undefined) {
    if (typeof options.timeout !== "number") throw invalidArgType("timeout", "of type number", options.timeout);
    this.timeout = options.timeout;
  }
  if (options.signal) require("stream").addAbortSignal(options.signal, this);
  let method = options.method;
  if (method !== null && method !== undefined && typeof method !== "string") throw invalidArgType("options.method", "of type string", method);
  if (method) {
    if (!checkIsHttpToken(method)) throw codeError(TypeError, "ERR_INVALID_HTTP_TOKEN", `Method must be a valid HTTP token ["${method}"]`);
    method = method.toUpperCase();
  } else method = "GET";
  Object.assign(this, { method, path: options.path || "/", host, protocol, maxHeaderSize: options.maxHeaderSize, joinDuplicateHeaders: options.joinDuplicateHeaders });
  Object.assign(this, { _ended: false, res: null, aborted: false, upgradeOrConnect: false, parser: null, maxHeadersCount: null, reusedSocket: false, _closed: false });
  if (callback) this.once("response", callback);
  this.useChunkedEncodingByDefault = !NO_BODY.has(method);
  if (agent) {
    if (!agent.keepAlive && !Number.isFinite(agent.maxSockets)) {
      this._last = true;
      this.shouldKeepAlive = false;
    } else {
      this._last = false;
      this.shouldKeepAlive = true;
    }
  }
  if (!Array.isArray(options.headers)) {
    if (options.headers) for (const key of Object.keys(options.headers)) this.setHeader(key, options.headers[key]);
    if (host && !this.getHeader("host") && setHost) {
      let hostHeader = host;
      if (hostHeader.indexOf(":") !== -1 && hostHeader.includes(":", hostHeader.indexOf(":") + 1) && hostHeader[0] !== "[") hostHeader = `[${hostHeader}]`;
      if (port && +port !== defaultPort) hostHeader += `:${port}`;
      this.setHeader("Host", hostHeader);
    }
    if (options.auth && !this.getHeader("Authorization")) this.setHeader("Authorization", `Basic ${Buffer.from(options.auth).toString("base64")}`);
    if (this.getHeader("expect")) this._storeHeader(`${method} ${this.path} HTTP/1.1\r\n`, this[kHeaders]);
  } else this._storeHeader(`${method} ${this.path} HTTP/1.1\r\n`, options.headers);
  const connectOptions = { ...options };
  delete connectOptions.signal;
  this[kOptions] = connectOptions;
  if (agent) agent.addRequest(this, connectOptions);
  else {
    this._last = true;
    this.shouldKeepAlive = false;
    if (typeof options.createConnection === "function") {
      let done = false;
      const oncreate = (error, socket) => {
        if (done) return;
        done = true;
        if (error) process.nextTick(() => this.emit("error", error));
        else this.onSocket(socket);
      };
      try {
        const socket = options.createConnection(connectOptions, oncreate);
        if (socket) oncreate(null, socket);
      } catch (error) {
        oncreate(error);
      }
    } else this.onSocket(net.createConnection(connectOptions));
  }
}
Object.setPrototypeOf(ClientRequest.prototype, OutgoingMessage.prototype);
Object.setPrototypeOf(ClientRequest, OutgoingMessage);

ClientRequest.prototype._implicitHeader = function () {
  if (this._header) throw codeError(Error, "ERR_HTTP_HEADERS_SENT", "Cannot render headers after they are sent to the client");
  this._storeHeader(`${this.method} ${this.path} HTTP/1.1\r\n`, this[kHeaders]);
};

ClientRequest.prototype.abort = function () {
  if (this.aborted) return;
  this.aborted = true;
  process.nextTick(() => this.emit("abort"));
  this.destroy();
};

ClientRequest.prototype.destroy = function (error) {
  if (this.destroyed) return this;
  this.destroyed = true;
  if (this.res) this.res._dump();
  this._destroyError = error;
  if (this.socket) this.socket.destroy(error);
  return this;
};

ClientRequest.prototype.onSocket = function (socket, error) {
  process.nextTick(() => {
    if (error) return this._fail(socket, error);
    if (this.destroyed) {
      this._fail(socket, this._destroyError);
      return;
    }
    tickOnSocket(this, socket);
  });
};

ClientRequest.prototype._fail = function (socket, error) {
  if (socket && !socket.destroyed) socket.destroy();
  if (!this._hadError) {
    this._hadError = true;
    this.emit("error", error || hangUp());
  }
  this._closed = true;
  this.emit("close");
};

function deferToConnect(req, apply) {
  if (req.socket && !req.socket.connecting) apply(req.socket);
  else req.once("socket", (socket) => (socket.connecting ? socket.once("connect", () => apply(socket)) : apply(socket)));
}
ClientRequest.prototype.setNoDelay = function (noDelay) {
  deferToConnect(this, (socket) => socket.setNoDelay(noDelay));
};
ClientRequest.prototype.setSocketKeepAlive = function (enable, delay) {
  deferToConnect(this, (socket) => socket.setKeepAlive(enable, delay));
};
ClientRequest.prototype.setTimeout = function (msecs, callback) {
  if (callback) this.once("timeout", callback);
  this.timeout = msecs;
  if (this.socket) this.socket.setTimeout(msecs);
  return this;
};
ClientRequest.prototype.clearTimeout = function (callback) {
  this.setTimeout(0, callback);
};

function tickOnSocket(req, socket) {
  let res = null;
  const parser = new Parser(false, {
    onHead(head) {
      if (req.res) {
        socket.destroy();
        return 0;
      }
      const message = new IncomingMessage(socket);
      Object.assign(message, { statusCode: head.statusCode, statusMessage: head.statusMessage, httpVersionMajor: 1, httpVersionMinor: head.minor, httpVersion: `1.${head.minor}` });
      message.joinDuplicateHeaders = !!req.joinDuplicateHeaders;
      message.rawHeaders = req.maxHeadersCount ? head.headers.slice(0, req.maxHeadersCount * 2) : head.headers;
      message.req = req;
      if ((req.method === "CONNECT" && head.statusCode >= 200 && head.statusCode < 300) || head.statusCode === 101) {
        message.upgrade = true;
        res = message;
        return UPGRADE;
      }
      if (head.statusCode >= 100 && head.statusCode < 200) {
        if (head.statusCode === 100) req.emit("continue");
        const { statusCode, statusMessage, httpVersion, httpVersionMajor, httpVersionMinor, headers, rawHeaders } = message;
        req.emit("information", { statusCode, statusMessage, httpVersion, httpVersionMajor, httpVersionMinor, headers, rawHeaders });
        return 0;
      }
      if (req.shouldKeepAlive && !head.keepAlive) req.shouldKeepAlive = false;
      res = req.res = message;
      if (req.aborted || !req.emit("response", res)) res._dump();
      return req.method === "HEAD" ? SKIP_BODY : 0;
    },
    onBody(chunk) {
      if (res) res._pushBody(chunk);
    },
    onComplete(trailers) {
      if (!res) return;
      const done = res;
      res = null;
      done.rawTrailers = trailers;
      done.complete = true;
      done.once("end", () => {
        req._ended = true;
        if (!req.shouldKeepAlive) {
          if (socket.writable) socket.destroySoon();
        } else if (req.writableFinished && !req.aborted) keepAlive();
        process.nextTick(() => done.destroy());
      });
      done.push(null);
    },
  });
  if (req.maxHeaderSize) parser.maxHeaderSize = req.maxHeaderSize;
  req.parser = parser;
  req.socket = socket;
  socket._httpMessage = req;
  socket.parser = parser;
  if (req.timeout !== undefined) socket.setTimeout(req.timeout);

  const onData = (chunk) => {
    let rest;
    try {
      rest = parser.execute(chunk);
    } catch (error) {
      detach();
      socket.destroy();
      req._hadError = true;
      return req.emit("error", error);
    }
    if (rest === null || !res || !res.upgrade) return;
    const upgraded = res;
    res = null;
    const event = req.method === "CONNECT" ? "connect" : "upgrade";
    if (req.listenerCount(event) > 0) {
      req.upgradeOrConnect = true;
      detach();
      socket.emit("agentRemove");
      socket._httpMessage = null;
      req.res = upgraded;
      req.emit(event, upgraded, socket, rest);
      req.destroyed = true;
      req._closed = true;
      req.emit("close");
    } else socket.destroy();
  };
  const onEnd = () => {
    if (!parser.finish() && !req.res) {
      req._hadError = true;
      req.emit("error", hangUp());
    }
    if (!socket.allowHalfOpen) socket.end();
  };
  const onError = (error) => {
    if (!req._hadError) {
      req._hadError = true;
      req.emit("error", error);
    }
  };
  const onClose = () => {
    detach();
    req.destroyed = true;
    const current = req.res;
    if (current) {
      if (!current.complete) {
        current.aborted = true;
        current.emit("aborted");
        if (current.listenerCount("error")) current.destroy(hangUp("aborted"));
        else current.destroy();
      }
    } else if (!req._hadError && !req.upgradeOrConnect) {
      req._hadError = true;
      req.emit("error", req._destroyError || hangUp());
    }
    if (!req._closed) {
      req._closed = true;
      req.emit("close");
    }
  };
  const onTimeout = () => req.emit("timeout");
  const onDrain = () => {
    if (req._needDrain) {
      req._needDrain = false;
      req.emit("drain");
    }
  };
  const onFinish = () => {
    if (req._ended && req.shouldKeepAlive) keepAlive();
  };
  function detach() {
    socket.removeListener("data", onData);
    socket.removeListener("end", onEnd);
    socket.removeListener("error", onError);
    socket.removeListener("close", onClose);
    socket.removeListener("timeout", onTimeout);
    socket.removeListener("drain", onDrain);
    req.removeListener("finish", onFinish);
  }
  function keepAlive() {
    detach();
    socket.parser = null;
    req.destroyed = true;
    if (req.timeout !== undefined) socket.setTimeout(0);
    if (req.agent && socket.listenerCount("free")) socket.emit("free");
    else if (req.agent) req.agent.emit("free", socket, req[kOptions]);
    else socket.destroy();
    process.nextTick(() => {
      req._closed = true;
      req.emit("close");
    });
  }
  socket.on("data", onData);
  socket.on("end", onEnd);
  socket.on("error", onError);
  socket.on("close", onClose);
  socket.on("timeout", onTimeout);
  socket.on("drain", onDrain);
  req.on("finish", onFinish);
  req.emit("socket", socket);
  req._flushOutput(socket);
}

module.exports = { ClientRequest };
