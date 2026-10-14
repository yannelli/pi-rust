// node:http server side: Server, ServerResponse, and the per-connection request loop.
const net = require("net");
const { Buffer } = require("buffer");
const { codeError, invalidArgValue, validate } = require("internal/errors");
const { STATUS_CODES, IncomingMessage, Parser, UPGRADE, checkInvalidHeaderChar, validateHeaderName, validateHeaderValue } = require("internal/http_common");
const { OutgoingMessage, kHeaders, CRLF } = require("internal/http_outgoing");

const kConnections = Symbol("kConnections");

function ServerResponse(req, options) {
  if (!(this instanceof ServerResponse)) return new ServerResponse(req, options);
  OutgoingMessage.call(this);
  Object.assign(this, { req, statusCode: 200, statusMessage: undefined, sendDate: true, _sent100: false, _expect_continue: false });
  if (req.method === "HEAD") this._hasBody = false;
  if (req.httpVersionMajor < 1 || req.httpVersionMinor < 1) {
    this.useChunkedEncodingByDefault = /(?:^|\W)chunked(?:$|\W)/i.test(req.headers.te || "");
    this.shouldKeepAlive = false;
  }
  this._rejectBody = !!(options && options.rejectNonStandardBodyWrites);
}
Object.setPrototypeOf(ServerResponse.prototype, OutgoingMessage.prototype);
Object.setPrototypeOf(ServerResponse, OutgoingMessage);

Object.assign(ServerResponse.prototype, {
  assignSocket(socket) {
    if (socket._httpMessage) throw codeError(Error, "ERR_HTTP_SOCKET_ASSIGNED", "ServerResponse has an already assigned socket");
    socket._httpMessage = this;
    this.socket = socket;
    this.emit("socket", socket);
    this._flushOutput(socket);
  },
  detachSocket(socket) {
    if (socket._httpMessage === this) socket._httpMessage = null;
    this.socket = null;
  },
  _writeRaw(text, callback) {
    if (this.socket && this.socket._httpMessage === this && !this._pending.length) this.socket.write(text, "latin1", callback);
    else this._pending.push([text, "latin1", callback]);
  },
  writeContinue(callback) {
    this._writeRaw(`HTTP/1.1 100 Continue${CRLF}${CRLF}`, callback);
    this._sent100 = true;
  },
  writeProcessing(callback) {
    this._writeRaw(`HTTP/1.1 102 Processing${CRLF}${CRLF}`, callback);
  },
  writeEarlyHints(hints, callback) {
    validate.object(hints, "hints");
    if (hints.link === null || hints.link === undefined) return;
    const links = [].concat(hints.link);
    if (!links.length || links.some((link) => typeof link !== "string" || checkInvalidHeaderChar(link))) throw invalidArgValue("hints.link", hints.link, "must be an array or string of format \"</styles.css>; rel=preload; as=style\"");
    let head = `HTTP/1.1 103 Early Hints${CRLF}Link: ${links.join(", ")}${CRLF}`;
    for (const [name, value] of Object.entries(hints)) {
      if (name === "link") continue;
      validateHeaderName(name);
      validateHeaderValue(name, value);
      head += `${name}: ${value}${CRLF}`;
    }
    this._writeRaw(head + CRLF, callback);
  },
  _implicitHeader() {
    this.writeHead(this.statusCode);
  },
  writeHead(statusCode, reason, headers) {
    if (this._header) throw codeError(Error, "ERR_HTTP_HEADERS_SENT", "Cannot write headers after they are sent to the client");
    const original = statusCode;
    statusCode |= 0;
    if (statusCode < 100 || statusCode > 999) throw codeError(RangeError, "ERR_HTTP_INVALID_STATUS_CODE", `Invalid status code: ${original}`);
    if (typeof reason === "string") this.statusMessage = reason;
    else {
      headers = reason;
      this.statusMessage = this.statusMessage || STATUS_CODES[statusCode] || "unknown";
    }
    this.statusCode = statusCode;
    let merged = this[kHeaders];
    if (headers) {
      if (merged) {
        const list = Array.isArray(headers) ? headers : Object.entries(headers);
        if (Array.isArray(headers) && !Array.isArray(headers[0])) {
          for (let index = 0; index < headers.length; index += 2) this.appendHeader(headers[index], headers[index + 1]);
        } else for (const [name, value] of list) if (name) this.setHeader(name, value);
        merged = this[kHeaders];
      } else merged = headers;
    }
    if (checkInvalidHeaderChar(this.statusMessage)) throw codeError(TypeError, "ERR_INVALID_CHAR", "Invalid character in statusMessage");
    if (statusCode === 204 || statusCode === 304 || (statusCode >= 100 && statusCode <= 199)) {
      this._hasBody = false;
      this._bodyNotAllowed = this._rejectBody;
    }
    if (this._expect_continue && !this._sent100) this.shouldKeepAlive = false;
    this._storeHeader(`HTTP/1.1 ${statusCode} ${this.statusMessage}${CRLF}`, merged);
    return this;
  },
});
ServerResponse.prototype.writeHeader = ServerResponse.prototype.writeHead;

function Server(options, requestListener) {
  if (!(this instanceof Server)) return new Server(options, requestListener);
  if (typeof options === "function") [requestListener, options] = [options, {}];
  else if (options === undefined || options === null) options = {};
  else validate.object(options, "options");
  net.Server.call(this, { noDelay: true, ...options });
  setupServer(this, options, requestListener);
  this.on("connection", connectionListener);
}
Object.setPrototypeOf(Server.prototype, net.Server.prototype);
Object.setPrototypeOf(Server, net.Server);

function setupServer(server, options, requestListener) {
  for (const name of ["maxHeaderSize", "requestTimeout", "headersTimeout", "keepAliveTimeout", "connectionsCheckingInterval"]) {
    if (options[name] !== undefined) validate.integer(options[name], `options.${name}`, 0);
  }
  Object.assign(server, {
    _IncomingMessage: options.IncomingMessage || IncomingMessage,
    _ServerResponse: options.ServerResponse || ServerResponse,
    maxHeaderSize: options.maxHeaderSize,
    joinDuplicateHeaders: !!options.joinDuplicateHeaders,
    requireHostHeader: options.requireHostHeader !== false,
    rejectNonStandardBodyWrites: !!options.rejectNonStandardBodyWrites,
    highWaterMark: options.highWaterMark,
    timeout: 0,
    maxHeadersCount: null,
    maxRequestsPerSocket: 0,
    headersTimeout: options.headersTimeout ?? 60000,
    requestTimeout: options.requestTimeout ?? 300000,
    keepAliveTimeout: options.keepAliveTimeout ?? 5000,
    [kConnections]: new Set(),
  });
  if (requestListener) server.on("request", requestListener);
}

Object.assign(Server.prototype, {
  setTimeout(msecs, callback) {
    this.timeout = msecs;
    if (callback) this.on("timeout", callback);
    return this;
  },
  close(callback) {
    this.closeIdleConnections();
    return net.Server.prototype.close.call(this, callback);
  },
  closeAllConnections() {
    for (const socket of this[kConnections]) socket.destroy();
  },
  closeIdleConnections() {
    for (const socket of this[kConnections]) if (socket._httpState && socket._httpState.idle()) socket.destroy();
  },
  [Symbol.asyncDispose]() {
    return new Promise((resolve) => this.close(() => resolve()));
  },
});

function rejectRequest(socket, status) {
  if (socket.writable && !socket._httpMessage) socket.write(`HTTP/1.1 ${status} ${STATUS_CODES[status]}${CRLF}Connection: close${CRLF}${CRLF}`);
  socket.destroySoon ? socket.destroySoon() : socket.end();
}

function socketError(server, socket, error) {
  if (server.listenerCount("clientError")) return server.emit("clientError", error, socket);
  if (/^HPE_/.test(error.code || "")) {
    rejectRequest(socket, error.code === "HPE_HEADER_OVERFLOW" ? 431 : 400);
    return;
  }
  socket.destroy();
}

// Runs as the server's 'connection' listener (or 'secureConnection' for https).
function connectionListener(socket) {
  const server = this;
  const incoming = [];
  const outgoing = [];
  let current = null;
  let timer = null;
  let requests = 0;
  const clear = () => {
    if (timer) clearTimeout(timer);
    timer = null;
  };
  const arm = (msecs, status) => {
    clear();
    if (!msecs) return;
    timer = setTimeout(() => {
      timer = null;
      if (server.listenerCount("clientError")) server.emit("clientError", codeError(Error, "ERR_HTTP_REQUEST_TIMEOUT", "Request timeout"), socket);
      else rejectRequest(socket, status);
    }, msecs);
    if (timer.unref) timer.unref();
  };
  const state = { idle: () => !current && outgoing.length === 0 };
  socket._httpState = state;
  server[kConnections].add(socket);
  if (server.timeout) socket.setTimeout(server.timeout);
  const parser = new Parser(true, {
    onHead(head) {
      const req = new server._IncomingMessage(socket);
      Object.assign(req, { method: head.method, url: head.url, httpVersionMajor: 1, httpVersionMinor: head.minor, httpVersion: `1.${head.minor}` });
      req.joinDuplicateHeaders = server.joinDuplicateHeaders;
      req.rawHeaders = server.maxHeadersCount ? head.headers.slice(0, server.maxHeadersCount * 2) : head.headers;
      clear();
      if (server.requestTimeout) arm(server.requestTimeout, 408);
      requests++;
      const upgrade = head.method === "CONNECT" || (head.connectionUpgrade && "upgrade" in req.headers);
      const event = head.method === "CONNECT" ? "connect" : "upgrade";
      if (upgrade && server.listenerCount(event) > 0) {
        req.upgrade = true;
        current = { req, event };
        return UPGRADE;
      }
      req.upgrade = false;
      current = req;
      incoming.push(req);
      if (server.requireHostHeader && head.minor >= 1 && !("host" in req.headers)) {
        rejectRequest(socket, 400);
        return 0;
      }
      const res = new server._ServerResponse(req, { highWaterMark: server.highWaterMark, rejectNonStandardBodyWrites: server.rejectNonStandardBodyWrites });
      res.shouldKeepAlive = res.shouldKeepAlive && head.keepAlive && !(server.maxRequestsPerSocket && requests >= server.maxRequestsPerSocket);
      res._keepAliveTimeout = server.keepAliveTimeout;
      res._defaultKeepAlive = true;
      outgoing.push(res);
      if (outgoing.length === 1) res.assignSocket(socket);
      res.once("finish", () => onFinish(req, res));
      if (socket._httpIdle) {
        socket._httpIdle = false;
        socket.setTimeout(server.timeout || 0);
      }
      if (server.maxRequestsPerSocket && requests > server.maxRequestsPerSocket) {
        res.writeHead(503);
        res.end();
        return 0;
      }
      const expect = req.headers.expect;
      if (expect !== undefined) {
        if (head.minor >= 1 && /(?:^|\W)100-continue(?:$|\W)/i.test(expect)) {
          res._expect_continue = true;
          if (server.listenerCount("checkContinue")) server.emit("checkContinue", req, res);
          else {
            res.writeContinue();
            server.emit("request", req, res);
          }
        } else if (server.listenerCount("checkExpectation")) server.emit("checkExpectation", req, res);
        else {
          res.writeHead(417);
          res.end();
        }
      } else server.emit("request", req, res);
      return 0;
    },
    onBody(chunk) {
      if (current && current.push) current._pushBody(chunk);
    },
    onComplete(trailers) {
      const req = current;
      current = null;
      clear();
      if (!req || !req.push) return;
      incoming.splice(incoming.indexOf(req) >>> 0, 1);
      req.rawTrailers = trailers;
      req.complete = true;
      req.push(null);
    },
  });
  if (server.maxHeaderSize) parser.maxHeaderSize = server.maxHeaderSize;
  if (server.headersTimeout) arm(server.headersTimeout, 408);

  function onFinish(req, res) {
    outgoing.shift();
    if (req.readableFlowing === null && !req.readableEnded) req._dump();
    res.detachSocket(socket);
    process.nextTick(() => {
      req.emit("close");
      res.emit("close");
    });
    if (res._last) {
      socket.destroySoon();
    } else if (outgoing.length) {
      outgoing[0].assignSocket(socket);
    } else if (!current && server.keepAliveTimeout) {
      socket._httpIdle = true;
      socket.setTimeout(server.keepAliveTimeout);
    }
  }

  function onData(chunk) {
    if (!current && !timer && server.headersTimeout && parser.state === "head" && !parser.buffer.length) arm(server.headersTimeout, 408);
    let rest;
    try {
      rest = parser.execute(chunk);
    } catch (error) {
      detach();
      socketError(server, socket, error);
      return;
    }
    if (rest !== null && current && current.event) {
      const { req, event } = current;
      current = null;
      clear();
      detach();
      server[kConnections].delete(socket);
      socket._httpState = null;
      server.emit(event, req, socket, rest);
    }
  }
  function onEnd() {
    if (!parser.finish() && current && current.push) current.destroy();
    if (outgoing.length === 0 && !socket.allowHalfOpen) socket.end();
    else if (outgoing.length) outgoing[outgoing.length - 1]._last = true;
  }
  function onTimeout() {
    if (socket._httpIdle) return socket.destroy();
    const req = current && current.push ? current : null;
    const res = socket._httpMessage;
    const reqTimeout = req && !req.complete && req.emit("timeout", socket);
    const resTimeout = res && res.emit("timeout", socket);
    const serverTimeout = server.emit("timeout", socket);
    if (!reqTimeout && !resTimeout && !serverTimeout) socket.destroy();
  }
  function onError(error) {
    socketError(server, socket, error);
  }
  function onClose() {
    clear();
    server[kConnections].delete(socket);
    for (const req of incoming.splice(0)) {
      if (!req.complete) {
        req.aborted = true;
        req.emit("aborted");
        if (req.listenerCount("error")) req.destroy(Object.assign(new Error("aborted"), { code: "ECONNRESET" }));
        else req.destroy();
      }
    }
    for (const res of outgoing.splice(0)) {
      res.destroyed = true;
      res.socket = null;
      process.nextTick(() => res.emit("close"));
    }
  }
  function onDrain() {
    const res = socket._httpMessage;
    if (res && res._needDrain) {
      res._needDrain = false;
      res.emit("drain");
    }
  }
  function detach() {
    socket.removeListener("data", onData);
    socket.removeListener("end", onEnd);
    socket.removeListener("timeout", onTimeout);
    socket.removeListener("error", onError);
    socket.removeListener("close", onClose);
    socket.removeListener("drain", onDrain);
    clear();
  }
  socket.on("data", onData);
  socket.on("end", onEnd);
  socket.on("timeout", onTimeout);
  socket.on("error", onError);
  socket.on("close", onClose);
  socket.on("drain", onDrain);
}

module.exports = { Server, ServerResponse, setupServer, connectionListener, kConnections };
