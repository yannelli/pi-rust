// Shared parts of node:http: constants, header validation, IncomingMessage, and the HTTP/1 parser.
const { Readable } = require("stream");
const { Buffer } = require("buffer");
const { codeError } = require("internal/errors");
const nativeHttp = native.http;

const METHODS = ["ACL", "BIND", "CHECKOUT", "CONNECT", "COPY", "DELETE", "GET", "HEAD", "LINK", "LOCK", "M-SEARCH", "MERGE", "MKACTIVITY", "MKCALENDAR", "MKCOL", "MOVE", "NOTIFY", "OPTIONS", "PATCH", "POST", "PROPFIND", "PROPPATCH", "PURGE", "PUT", "QUERY", "REBIND", "REPORT", "SEARCH", "SOURCE", "SUBSCRIBE", "TRACE", "UNBIND", "UNLINK", "UNLOCK", "UNSUBSCRIBE"];
const STATUS_CODES = {
  100: "Continue", 101: "Switching Protocols", 102: "Processing", 103: "Early Hints",
  200: "OK", 201: "Created", 202: "Accepted", 203: "Non-Authoritative Information", 204: "No Content", 205: "Reset Content", 206: "Partial Content", 207: "Multi-Status", 208: "Already Reported", 226: "IM Used",
  300: "Multiple Choices", 301: "Moved Permanently", 302: "Found", 303: "See Other", 304: "Not Modified", 305: "Use Proxy", 307: "Temporary Redirect", 308: "Permanent Redirect",
  400: "Bad Request", 401: "Unauthorized", 402: "Payment Required", 403: "Forbidden", 404: "Not Found", 405: "Method Not Allowed", 406: "Not Acceptable", 407: "Proxy Authentication Required", 408: "Request Timeout", 409: "Conflict", 410: "Gone", 411: "Length Required", 412: "Precondition Failed", 413: "Payload Too Large", 414: "URI Too Long", 415: "Unsupported Media Type", 416: "Range Not Satisfiable", 417: "Expectation Failed", 418: "I'm a Teapot", 421: "Misdirected Request", 422: "Unprocessable Entity", 423: "Locked", 424: "Failed Dependency", 425: "Too Early", 426: "Upgrade Required", 428: "Precondition Required", 429: "Too Many Requests", 431: "Request Header Fields Too Large", 451: "Unavailable For Legal Reasons",
  500: "Internal Server Error", 501: "Not Implemented", 502: "Bad Gateway", 503: "Service Unavailable", 504: "Gateway Timeout", 505: "HTTP Version Not Supported", 506: "Variant Also Negotiates", 507: "Insufficient Storage", 508: "Loop Detected", 509: "Bandwidth Limit Exceeded", 510: "Not Extended", 511: "Network Authentication Required",
};
const settings = { maxHeaderSize: 16384 };

const tokenPattern = /^[\^_`a-zA-Z\-0-9!#$%&'*+.|~]+$/;
const invalidHeaderChar = /[^\t\x20-\x7e\x80-\xff]/;
const checkIsHttpToken = (value) => typeof value === "string" && tokenPattern.test(value);
const checkInvalidHeaderChar = (value) => invalidHeaderChar.test(value);

function validateHeaderName(name, label = "Header name") {
  if (typeof name !== "string" || !name || !checkIsHttpToken(name)) {
    throw codeError(TypeError, "ERR_INVALID_HTTP_TOKEN", `${label} must be a valid HTTP token ["${name}"]`);
  }
}

function validateHeaderValue(name, value) {
  if (value === undefined) throw codeError(TypeError, "ERR_HTTP_INVALID_HEADER_VALUE", `Invalid value "${value}" for header "${name}"`);
  if (checkInvalidHeaderChar(String(value))) throw codeError(TypeError, "ERR_INVALID_CHAR", `Invalid character in header content ["${name}"]`);
}

// Headers whose duplicates Node drops instead of joining.
const SINGLE = new Set(["content-type", "content-length", "user-agent", "referer", "host", "authorization", "proxy-authorization", "if-modified-since", "if-unmodified-since", "from", "location", "max-forwards", "retry-after", "etag", "last-modified", "server", "age", "expires"]);

function headerObjects(raw, joinDuplicates) {
  const headers = {};
  const distinct = {};
  for (let index = 0; index < raw.length; index += 2) {
    const name = raw[index].toLowerCase();
    const value = raw[index + 1];
    (distinct[name] || (distinct[name] = [])).push(value);
    if (!(name in headers)) headers[name] = name === "set-cookie" ? [value] : value;
    else if (name === "set-cookie") headers[name].push(value);
    else if (joinDuplicates) headers[name] += `, ${value}`;
    else if (name === "cookie") headers[name] += `; ${value}`;
    else if (!SINGLE.has(name)) headers[name] += `, ${value}`;
  }
  return { headers, distinct };
}

function IncomingMessage(socket) {
  if (!(this instanceof IncomingMessage)) return new IncomingMessage(socket);
  Readable.call(this, { autoDestroy: false });
  Object.assign(this, { socket, httpVersionMajor: null, httpVersionMinor: null, httpVersion: null, complete: false, rawHeaders: [], rawTrailers: [] });
  Object.assign(this, { joinDuplicateHeaders: false, aborted: false, upgrade: null, url: "", method: null, statusCode: null, statusMessage: null });
  Object.assign(this, { _headers: null, _trailers: null, _paused: false, _dumped: false });
}
Object.setPrototypeOf(IncomingMessage.prototype, Readable.prototype);
Object.setPrototypeOf(IncomingMessage, Readable);

for (const [name, raw, part] of [["headers", "rawHeaders", "headers"], ["headersDistinct", "rawHeaders", "distinct"], ["trailers", "rawTrailers", "headers"], ["trailersDistinct", "rawTrailers", "distinct"]]) {
  const cache = `_${name}Cache`;
  Object.defineProperty(IncomingMessage.prototype, name, {
    configurable: true,
    enumerable: true,
    get() {
      if (!this[cache] || this[cache][0] !== this[raw].length) this[cache] = [this[raw].length, headerObjects(this[raw], this.joinDuplicateHeaders)[part]];
      return this[cache][1];
    },
    set(value) { this[cache] = [this[raw].length, value]; },
  });
}
Object.defineProperty(IncomingMessage.prototype, "connection", { configurable: true, get() { return this.socket; }, set(value) { this.socket = value; } });

IncomingMessage.prototype._read = function () {
  if (this._paused && this.socket && !this.socket.destroyed) {
    this._paused = false;
    this.socket.resume();
  }
};

IncomingMessage.prototype._pushBody = function (chunk) {
  if (this._dumped) return;
  if (!this.push(chunk) && this.socket && !this._paused) {
    this._paused = true;
    this.socket.pause();
  }
};

IncomingMessage.prototype._destroy = function (error, callback) {
  if (!this.readableEnded || !this.complete) {
    this.aborted = true;
    this.emit("aborted");
  }
  if (this.socket && !this.socket.destroyed && !this.complete) this.socket.destroy(error);
  callback(error);
};

IncomingMessage.prototype.setTimeout = function (msecs, callback) {
  if (callback) this.on("timeout", callback);
  if (this.socket) this.socket.setTimeout(msecs);
  return this;
};

IncomingMessage.prototype._dump = function () {
  if (this._dumped) return;
  this._dumped = true;
  this.removeAllListeners("data");
  this.resume();
  if (this._paused) this._read();
};

function parseError(error, consumed, raw) {
  return Object.assign(error, { bytesParsed: consumed, rawPacket: raw });
}

// An incremental HTTP/1 parser. `onHead(head)` returns 0, SKIP_BODY, or UPGRADE.
const SKIP_BODY = 1;
const UPGRADE = 2;
class Parser {
  constructor(isRequest, handlers) {
    Object.assign(this, handlers, { isRequest, buffer: Buffer.alloc(0), state: "head", remaining: 0, maxHeaderSize: settings.maxHeaderSize, upgraded: false });
  }

  execute(chunk) {
    this.buffer = this.buffer.length ? Buffer.concat([this.buffer, chunk]) : chunk;
    try {
      while (this.buffer.length && !this.upgraded && this.step());
    } catch (error) {
      this.state = "error";
      throw parseError(error, 0, chunk);
    }
    if (!this.upgraded) return null;
    const rest = this.buffer;
    this.buffer = Buffer.alloc(0);
    return rest;
  }

  take(length) {
    const part = this.buffer.subarray(0, length);
    this.buffer = this.buffer.subarray(length);
    return part;
  }

  step() {
    const state = this.state;
    if (state === "head") return this.readHead();
    if (state === "length") {
      const part = this.take(Math.min(this.remaining, this.buffer.length));
      this.remaining -= part.length;
      this.onBody(part);
      if (this.remaining === 0) this.complete([]);
      return true;
    }
    if (state === "close") {
      this.onBody(this.take(this.buffer.length));
      return false;
    }
    if (state === "chunk-size") {
      const parsed = nativeHttp.chunkSize(this.buffer);
      if (!parsed) return false;
      this.take(parsed[0]);
      this.remaining = parsed[1];
      this.state = parsed[1] === 0 ? "trailers" : "chunk-data";
      return true;
    }
    if (state === "chunk-data") {
      const part = this.take(Math.min(this.remaining, this.buffer.length));
      this.remaining -= part.length;
      if (part.length) this.onBody(part);
      if (this.remaining === 0) this.state = "chunk-end";
      return true;
    }
    if (state === "chunk-end") {
      if (this.buffer.length < 2) return false;
      if (this.buffer[0] !== 13 || this.buffer[1] !== 10) throw codeError(Error, "HPE_STRICT", "Parse Error: Expected LF after chunk data");
      this.take(2);
      this.state = "chunk-size";
      return true;
    }
    if (state === "trailers") {
      if (this.buffer.length >= 2 && this.buffer[0] === 13 && this.buffer[1] === 10) {
        this.take(2);
        this.complete([]);
        return true;
      }
      const head = nativeHttp.parseHead(Buffer.concat([Buffer.from("HTTP/1.1 200 T\r\n"), this.buffer]), false);
      if (!head) return false;
      this.take(head.length - 16);
      this.complete(head.headers);
      return true;
    }
    throw codeError(Error, "HPE_CLOSED_CONNECTION", "Parse Error: Data after `Connection: close`");
  }

  readHead() {
    if (!this.isRequest || this.buffer[0] !== 13) {
      const head = nativeHttp.parseHead(this.buffer, this.isRequest);
      if (!head) {
        if (this.buffer.length > this.maxHeaderSize) throw codeError(Error, "HPE_HEADER_OVERFLOW", "Parse Error: Header overflow");
        return false;
      }
      if (head.length > this.maxHeaderSize) throw codeError(Error, "HPE_HEADER_OVERFLOW", "Parse Error: Header overflow");
      this.take(head.length);
      return this.startBody(head);
    }
    this.take(this.buffer[1] === 10 ? 2 : 1);
    return true;
  }

  startBody(head) {
    let chunked = false;
    let length = null;
    let connection = "";
    for (let index = 0; index < head.headers.length; index += 2) {
      const name = head.headers[index].toLowerCase();
      const value = head.headers[index + 1];
      if (name === "transfer-encoding") chunked = /(^|,)\s*chunked\s*$/i.test(value);
      else if (name === "content-length") {
        if (!/^\s*\d+\s*$/.test(value) || (length !== null && length !== Number(value))) throw codeError(Error, "HPE_UNEXPECTED_CONTENT_LENGTH", "Parse Error: Invalid character in Content-Length");
        length = Number(value);
      } else if (name === "connection") connection += `${value.toLowerCase()},`;
    }
    if (chunked && length !== null && this.isRequest) throw codeError(Error, "HPE_INVALID_TRANSFER_ENCODING", "Parse Error: Transfer-Encoding can't be present with Content-Length");
    const tokens = connection.split(",").map((part) => part.trim());
    head.keepAlive = head.minor >= 1 ? !tokens.includes("close") : tokens.includes("keep-alive");
    head.connectionUpgrade = tokens.includes("upgrade");
    const mode = this.onHead(head);
    if (mode === UPGRADE) {
      this.upgraded = true;
      return false;
    }
    if (!this.isRequest && head.statusCode >= 100 && head.statusCode < 200) return true;
    if (mode === SKIP_BODY || (!chunked && length === 0) || (this.isRequest && !chunked && length === null)) this.complete([]);
    else if (chunked) this.state = "chunk-size";
    else if (length !== null) Object.assign(this, { state: "length", remaining: length });
    else this.state = "close";
    return true;
  }

  complete(trailers) {
    this.state = "head";
    this.onComplete(trailers);
  }

  // The peer closed; a body read until close ends here. Returns false if a message was cut short.
  finish() {
    if (this.state === "close") {
      this.complete([]);
      return true;
    }
    return this.state === "head" && this.buffer.length === 0;
  }
}

module.exports = { METHODS, STATUS_CODES, settings, validateHeaderName, validateHeaderValue, checkIsHttpToken, checkInvalidHeaderChar, headerObjects, IncomingMessage, Parser, SKIP_BODY, UPGRADE };
