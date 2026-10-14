// OutgoingMessage: the header store and body writer under ServerResponse and ClientRequest.
const Stream = require("stream");
const { Buffer } = require("buffer");
const { codeError, invalidArgType } = require("internal/errors");
const { validateHeaderName, validateHeaderValue } = require("internal/http_common");

const kHeaders = Symbol("kOutHeaders");
const CRLF = "\r\n";

function headersSentError(action) {
  return codeError(Error, "ERR_HTTP_HEADERS_SENT", `Cannot ${action} headers after they are sent to the client`);
}

function OutgoingMessage() {
  Stream.call(this);
  Object.assign(this, { [kHeaders]: null, _header: null, _last: false, chunkedEncoding: false, shouldKeepAlive: true, useChunkedEncodingByDefault: true });
  Object.assign(this, { sendDate: false, _removedConnection: false, _removedContLen: false, _removedTE: false, _contentLength: null, _hasBody: true });
  Object.assign(this, { _trailer: "", finished: false, _headerSent: false, socket: null, _pending: [], _pendingBytes: 0, _corked: 0 });
  Object.assign(this, { writable: true, destroyed: false, errored: null, _finishedWriting: false, strictContentLength: false, _needDrain: false });
}
Object.setPrototypeOf(OutgoingMessage.prototype, Stream.prototype);
Object.setPrototypeOf(OutgoingMessage, Stream);

Object.assign(OutgoingMessage.prototype, {
  setHeader(name, value) {
    if (this._header) throw headersSentError("set");
    validateHeaderName(name);
    validateHeaderValue(name, value);
    (this[kHeaders] || (this[kHeaders] = Object.create(null)))[name.toLowerCase()] = [name, value];
    return this;
  },
  setHeaders(headers) {
    if (this._header) throw headersSentError("set");
    if (!headers || typeof headers !== "object" || typeof headers.entries !== "function") {
      throw invalidArgType("headers", "an instance of Headers or Map", headers);
    }
    for (const [name, value] of headers.entries()) this.setHeader(name, value);
    return this;
  },
  appendHeader(name, value) {
    if (this._header) throw headersSentError("append");
    validateHeaderName(name);
    validateHeaderValue(name, value);
    const key = name.toLowerCase();
    const store = this[kHeaders] || (this[kHeaders] = Object.create(null));
    if (!store[key]) store[key] = [name, value];
    else store[key][1] = [].concat(store[key][1], value);
    return this;
  },
  getHeader(name) {
    if (typeof name !== "string") throw invalidArgType("name", "of type string", name);
    const entry = this[kHeaders] && this[kHeaders][name.toLowerCase()];
    return entry && entry[1];
  },
  getHeaderNames() {
    return this[kHeaders] ? Object.keys(this[kHeaders]) : [];
  },
  getRawHeaderNames() {
    return this[kHeaders] ? Object.values(this[kHeaders]).map((entry) => entry[0]) : [];
  },
  getHeaders() {
    const result = Object.create(null);
    for (const key of this.getHeaderNames()) result[key] = this[kHeaders][key][1];
    return result;
  },
  hasHeader(name) {
    if (typeof name !== "string") throw invalidArgType("name", "of type string", name);
    return !!(this[kHeaders] && this[kHeaders][name.toLowerCase()]);
  },
  removeHeader(name) {
    if (typeof name !== "string") throw invalidArgType("name", "of type string", name);
    if (this._header) throw headersSentError("remove");
    const key = name.toLowerCase();
    if (key === "connection") this._removedConnection = true;
    else if (key === "content-length") this._removedContLen = true;
    else if (key === "transfer-encoding") this._removedTE = true;
    else if (key === "date") this.sendDate = false;
    if (this[kHeaders]) delete this[kHeaders][key];
  },
  addTrailers(headers) {
    const list = Array.isArray(headers) ? headers : Object.entries(headers);
    for (const [name, value] of list) {
      validateHeaderName(name, "Trailer name");
      if (checkValue(value)) throw codeError(TypeError, "ERR_INVALID_CHAR", `Invalid character in trailer content ["${name}"]`);
      this._trailer += `${name}: ${value}${CRLF}`;
    }
  },
});

const checkValue = (value) => /[^\t\x20-\x7e\x80-\xff]/.test(String(value));

// Build the head from the first line and a header object or flat array, as Node's _storeHeader does.
OutgoingMessage.prototype._storeHeader = function (firstLine, headers) {
  const state = { connection: false, contLen: false, te: false, date: false, expect: false, trailer: false, upgrade: false };
  let head = firstLine;
  const add = (name, value) => {
    validateHeaderName(name);
    validateHeaderValue(name, value);
    if (Array.isArray(value)) {
      if (value.length < 2 || !/^(content-length|content-type|user-agent|referer|host|authorization|proxy-authorization|if-modified-since|if-unmodified-since|from|location|max-forwards)$/i.test(name)) {
        for (const item of value) add(name, item);
        return;
      }
      value = value.join("; ");
    }
    head += `${name}: ${value}${CRLF}`;
    const key = name.toLowerCase();
    if (key === "connection") {
      state.connection = true;
      if (/(^|,)\s*close\s*(,|$)/i.test(String(value))) this._last = true;
      else this.shouldKeepAlive = true;
    } else if (key === "content-length") state.contLen = true;
    else if (key === "transfer-encoding") {
      state.te = true;
      if (/(^|,)\s*chunked\s*$/i.test(String(value))) this.chunkedEncoding = true;
    } else if (key === "date") state.date = true;
    else if (key === "expect") state.expect = true;
    else if (key === "trailer") state.trailer = true;
    else if (key === "upgrade") state.upgrade = true;
  };
  if (Array.isArray(headers)) {
    if (headers.length && Array.isArray(headers[0])) for (const [name, value] of headers) add(name, value);
    else for (let index = 0; index < headers.length; index += 2) add(headers[index], headers[index + 1]);
  } else if (headers) {
    for (const key of Object.keys(headers)) {
      const entry = headers[key];
      if (Array.isArray(entry) && entry.length === 2 && this[kHeaders] === headers) add(entry[0], entry[1]);
      else add(key, entry);
    }
  }
  if (this.sendDate && !state.date) head += `Date: ${new Date().toUTCString()}${CRLF}`;
  if (!state.connection) {
    const keepAlive = this.shouldKeepAlive && (state.contLen || this.useChunkedEncodingByDefault || this.agent);
    if (keepAlive && this.shouldKeepAlive && !this._removedConnection) {
      head += `Connection: keep-alive${CRLF}`;
      if (this._keepAliveTimeout && this._defaultKeepAlive) head += `Keep-Alive: timeout=${Math.floor(this._keepAliveTimeout / 1000)}${CRLF}`;
    } else if (!this._removedConnection) {
      this._last = true;
      head += `Connection: close${CRLF}`;
    } else this._last = !this.shouldKeepAlive;
  }
  if (!state.contLen && !state.te) {
    if (!this._hasBody) this.chunkedEncoding = false;
    else if (!this.useChunkedEncodingByDefault) this._last = this._last || !this.shouldKeepAlive;
    else if (!state.trailer && !this._removedContLen && typeof this._contentLength === "number") head += `Content-Length: ${this._contentLength}${CRLF}`;
    else if (!this._removedTE) {
      head += `Transfer-Encoding: chunked${CRLF}`;
      this.chunkedEncoding = true;
    } else this._last = true;
  }
  if (this.chunkedEncoding !== true && state.trailer) throw codeError(Error, "ERR_HTTP_TRAILER_INVALID", "Trailers are invalid with this transfer encoding");
  this._header = head + CRLF;
  this._headerSent = false;
  if (state.expect) this._send("");
};

// Hand bytes to the socket, or queue them until a socket is assigned and active.
OutgoingMessage.prototype._send = function (data, encoding, callback) {
  if (!this._headerSent && this._header) {
    data = typeof data === "string" ? this._header + data : Buffer.concat([Buffer.from(this._header, "latin1"), data]);
    if (typeof data === "string") encoding = encoding || "latin1";
    this._headerSent = true;
  }
  const socket = this.socket;
  if (socket && socket._httpMessage === this && !this._corked && socket.writable) {
    for (const [pendingData, pendingEncoding, pendingCallback] of this._pending.splice(0)) socket.write(pendingData, pendingEncoding, pendingCallback);
    this._pendingBytes = 0;
    return socket.write(data, encoding, callback);
  }
  this._pending.push([data, encoding, callback]);
  this._pendingBytes += data.length;
  return false;
};

OutgoingMessage.prototype._flushOutput = function (socket) {
  const ok = this._pending.length === 0;
  for (const [data, encoding, callback] of this._pending.splice(0)) socket.write(data, encoding, callback);
  this._pendingBytes = 0;
  return ok;
};

OutgoingMessage.prototype.flush = OutgoingMessage.prototype.flushHeaders = function () {
  if (!this._header) this._implicitHeader();
  this._send("");
};

function writeError(message, error, callback) {
  process.nextTick(() => {
    if (!message.destroyed) message.emit("error", error);
    if (typeof callback === "function") callback(error);
  });
}

OutgoingMessage.prototype.write = function (chunk, encoding, callback) {
  if (typeof encoding === "function") [callback, encoding] = [encoding, null];
  const result = this._writeChunk(chunk, encoding, callback, false);
  if (!result) this._needDrain = true;
  return result;
};

OutgoingMessage.prototype._writeChunk = function (chunk, encoding, callback, fromEnd) {
  if (this.finished && !fromEnd) {
    writeError(this, codeError(Error, "ERR_STREAM_WRITE_AFTER_END", "write after end"), callback);
    return true;
  }
  if (this.destroyed) {
    writeError(this, codeError(Error, "ERR_STREAM_DESTROYED", "Cannot call write after a stream was destroyed"), callback);
    return false;
  }
  if (chunk === null) throw codeError(TypeError, "ERR_STREAM_NULL_VALUES", "May not write null values to stream");
  if (typeof chunk !== "string" && !(chunk instanceof Uint8Array)) throw invalidArgType("chunk", "of type string or an instance of Buffer, TypedArray, or DataView", chunk);
  if (!this._header) {
    if (fromEnd) this._contentLength = typeof chunk === "string" ? Buffer.byteLength(chunk, encoding || "utf8") : chunk.byteLength;
    this._implicitHeader();
  }
  if (!this._hasBody) {
    if (this._bodyNotAllowed) throw codeError(Error, "ERR_HTTP_BODY_NOT_ALLOWED", "Adding content for this request method or response status is not allowed.");
    if (callback) process.nextTick(callback);
    return true;
  }
  const length = typeof chunk === "string" ? Buffer.byteLength(chunk, encoding || "utf8") : chunk.byteLength;
  if (length === 0) {
    if (callback) process.nextTick(callback);
    return true;
  }
  if (!this.chunkedEncoding) return this._send(chunk, encoding, callback);
  const body = typeof chunk === "string" ? Buffer.from(chunk, encoding || "utf8") : Buffer.from(chunk.buffer, chunk.byteOffset, chunk.byteLength);
  return this._send(Buffer.concat([Buffer.from(`${length.toString(16)}${CRLF}`), body, Buffer.from(CRLF)]), null, callback);
};

OutgoingMessage.prototype.end = function (chunk, encoding, callback) {
  if (typeof chunk === "function") [callback, chunk, encoding] = [chunk, null, null];
  else if (typeof encoding === "function") [callback, encoding] = [encoding, null];
  if (this.finished) {
    if (typeof callback === "function") {
      if (this.writableFinished) process.nextTick(callback, codeError(Error, "ERR_STREAM_ALREADY_FINISHED", "Cannot call end after a stream was finished"));
      else this.once("finish", callback);
    }
    return this;
  }
  if (chunk !== null && chunk !== undefined) this._writeChunk(chunk, encoding, null, true);
  else if (!this._header) {
    this._contentLength = 0;
    this._implicitHeader();
  }
  if (typeof callback === "function") this.once("finish", callback);
  const finish = () => this._onFinish();
  if (this.chunkedEncoding && this._hasBody) this._send(`0${CRLF}${this._trailer}${CRLF}`, "latin1", finish);
  else if (!this._headerSent || this._pending.length) this._send("", "latin1", finish);
  else if (this.socket && this.socket._httpMessage === this && this.socket.writable) this.socket.write("", finish);
  else this._pending.push(["", "latin1", finish]);
  this.finished = true;
  this.writable = false;
  this.emit("prefinish");
  return this;
};

OutgoingMessage.prototype._onFinish = function () {
  if (this._finishedWriting) return;
  this._finishedWriting = true;
  this.emit("finish");
};

OutgoingMessage.prototype.cork = function () {
  this._corked++;
};

OutgoingMessage.prototype.uncork = function () {
  if (this._corked > 0 && --this._corked === 0 && this.socket && this.socket._httpMessage === this) this._flushOutput(this.socket);
};

OutgoingMessage.prototype.destroy = function (error) {
  if (this.destroyed) return this;
  this.destroyed = true;
  this.errored = error || null;
  if (this.socket) this.socket.destroy(error);
  else this.once("socket", (socket) => socket.destroy(error));
  return this;
};

OutgoingMessage.prototype.setTimeout = function (msecs, callback) {
  if (callback) this.on("timeout", callback);
  if (this.socket) this.socket.setTimeout(msecs);
  else this.once("socket", (socket) => socket.setTimeout(msecs));
  return this;
};

const getters = {
  connection() { return this.socket; },
  headersSent() { return !!this._header; },
  writableEnded() { return this.finished; },
  writableFinished() { return this._finishedWriting; },
  writableObjectMode() { return false; },
  writableCorked() { return this._corked; },
  writableHighWaterMark() { return this.socket ? this.socket.writableHighWaterMark : 16384; },
  writableLength() { return this._pendingBytes + (this.socket ? this.socket.writableLength : 0); },
  writableNeedDrain() { return this._needDrain; },
};
for (const [name, get] of Object.entries(getters)) Object.defineProperty(OutgoingMessage.prototype, name, { configurable: true, get });

module.exports = { OutgoingMessage, kHeaders, CRLF };
