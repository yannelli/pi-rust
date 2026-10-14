// The http2 compatibility API: Http2ServerRequest and Http2ServerResponse over an Http2Stream.
const Stream = require("stream");
const { codeError, invalidArgType, validate } = require("internal/errors");
const { validateHeaderName, validateHeaderValue } = require("internal/http_common");

const CONNECTION_HEADERS = new Set(["connection", "upgrade", "http2-settings", "transfer-encoding", "keep-alive", "proxy-connection"]);

class Http2ServerRequest extends Stream.Readable {
  constructor(stream, headers, options, rawHeaders) {
    super({ autoDestroy: false, ...options });
    Object.assign(this, { stream, headers, rawHeaders, trailers: {}, rawTrailers: [], complete: false, aborted: false });
    stream.on("data", (chunk) => {
      if (!this.push(chunk)) stream.pause();
    });
    stream.on("end", () => {
      this.complete = true;
      this.push(null);
    });
    stream.on("trailers", (trailers, flags, raw) => {
      this.trailers = trailers;
      this.rawTrailers = raw;
    });
    stream.on("close", () => {
      if (!this.complete) {
        this.aborted = true;
        this.emit("aborted");
      }
      this.emit("close");
    });
    stream.on("error", (error) => this.listenerCount("error") && this.emit("error", error));
  }

  _read() {
    this.stream.resume();
  }

  get method() { return this.headers[":method"]; }
  get authority() { return this.headers[":authority"]; }
  get scheme() { return this.headers[":scheme"]; }
  get url() { return this._url !== undefined ? this._url : this.headers[":path"]; }
  set url(value) { this._url = value; }
  get httpVersion() { return "2.0"; }
  get httpVersionMajor() { return 2; }
  get httpVersionMinor() { return 0; }
  get socket() { return this.stream.session ? this.stream.session.socket : null; }
  get connection() { return this.socket; }

  setTimeout(msecs, callback) {
    this.stream.setTimeout(msecs, callback);
    return this;
  }
}

class Http2ServerResponse extends Stream {
  constructor(stream, headers, req) {
    super();
    Object.assign(this, { stream, req, statusCode: 200, statusMessage: "", sendDate: true, finished: false, writable: true });
    this._headers = { __proto__: null };
    this._trailers = {};
    stream.on("wantTrailers", () => stream.sendTrailers(this._trailers));
    stream.on("drain", () => this.emit("drain"));
    stream.on("finish", () => this.emit("finish"));
    stream.on("close", () => this.emit("close"));
    stream.on("error", (error) => this.listenerCount("error") && this.emit("error", error));
  }

  get headersSent() { return this.stream.headersSent; }
  get socket() { return this.stream.session ? this.stream.session.socket : null; }
  get connection() { return this.socket; }
  get writableEnded() { return this.finished; }
  get writableFinished() { return this.stream.writableFinished; }
  get writableCorked() { return this.stream.writableCorked; }
  get writableHighWaterMark() { return this.stream.writableHighWaterMark; }
  get writableLength() { return this.stream.writableLength; }

  setHeader(name, value) {
    if (this.headersSent) throw codeError(Error, "ERR_HTTP2_HEADERS_SENT", "Response has already been initiated.");
    validateHeaderName(name);
    validateHeaderValue(name, value);
    name = name.trim().toLowerCase();
    if (name[0] === ":") throw codeError(TypeError, "ERR_HTTP2_PSEUDOHEADER_NOT_ALLOWED", "Cannot set HTTP/2 pseudo-headers");
    if (!CONNECTION_HEADERS.has(name)) this._headers[name] = value;
    return this;
  }

  appendHeader(name, value) {
    const current = this.getHeader(name);
    return this.setHeader(name, current === undefined ? value : [].concat(current, value));
  }

  getHeader(name) {
    if (typeof name !== "string") throw invalidArgType("name", "of type string", name);
    return this._headers[name.trim().toLowerCase()];
  }

  getHeaders() {
    return { __proto__: null, ...this._headers };
  }

  getHeaderNames() {
    return Object.keys(this._headers);
  }

  hasHeader(name) {
    return this.getHeader(name) !== undefined;
  }

  removeHeader(name) {
    if (this.headersSent) throw codeError(Error, "ERR_HTTP2_HEADERS_SENT", "Response has already been initiated.");
    delete this._headers[name.trim().toLowerCase()];
  }

  setTrailer(name, value) {
    validateHeaderName(name);
    this._trailers[name.trim().toLowerCase()] = value;
  }

  addTrailers(headers) {
    for (const [name, value] of Object.entries(headers)) this.setTrailer(name, value);
  }

  writeHead(statusCode, statusMessage, headers) {
    if (typeof statusMessage !== "string") headers = statusMessage;
    if (this.headersSent) throw codeError(Error, "ERR_HTTP2_HEADERS_SENT", "Response has already been initiated.");
    this.statusCode = statusCode;
    if (Array.isArray(headers)) {
      for (let index = 0; index < headers.length; index += 2) this.appendHeader(headers[index], headers[index + 1]);
    } else if (headers) {
      for (const name of Object.keys(headers)) this.setHeader(name, headers[name]);
    }
    this._respond();
    return this;
  }

  _respond() {
    if (this.headersSent) return;
    const code = this.statusCode | 0;
    if (code === 101) throw codeError(RangeError, "ERR_HTTP2_STATUS_101", "HTTP status code 101 (Switching Protocols) is forbidden in HTTP/2");
    if (code < 200 || code > 599) throw codeError(RangeError, "ERR_HTTP2_STATUS_INVALID", `Invalid status code: ${this.statusCode}`);
    const headers = { ...this._headers, ":status": code };
    if (this.sendDate && headers.date === undefined) headers.date = new Date().toUTCString();
    this.stream.respond(headers, { waitForTrailers: true });
  }

  flushHeaders() {
    if (!this.finished) this._respond();
  }

  write(chunk, encoding, callback) {
    if (typeof encoding === "function") [callback, encoding] = [encoding, undefined];
    if (this.finished || this.stream.destroyed) {
      const error = codeError(Error, this.finished ? "ERR_STREAM_WRITE_AFTER_END" : "ERR_HTTP2_INVALID_STREAM", this.finished ? "write after end" : "The stream has been destroyed");
      if (typeof callback === "function") process.nextTick(callback, error);
      return false;
    }
    this._respond();
    return this.stream.write(chunk, encoding, callback);
  }

  end(chunk, encoding, callback) {
    if (typeof chunk === "function") [callback, chunk] = [chunk, undefined];
    else if (typeof encoding === "function") [callback, encoding] = [encoding, undefined];
    if (this.finished) {
      if (typeof callback === "function") process.nextTick(callback);
      return this;
    }
    if (chunk !== undefined && chunk !== null) this.write(chunk, encoding);
    this.finished = true;
    this.writable = false;
    this._respond();
    this.stream.end(callback);
    return this;
  }

  writeContinue() {
    this.stream.additionalHeaders({ ":status": 100 });
  }

  writeEarlyHints(hints) {
    this.stream.additionalHeaders({ ...hints, ":status": 103 });
  }

  createPushResponse(headers, callback) {
    validate.function(callback, "callback");
    if (this.stream.closed || this.stream.destroyed) {
      return process.nextTick(callback, codeError(Error, "ERR_HTTP2_INVALID_STREAM", "The stream has been destroyed"));
    }
    this.stream.pushStream(headers, {}, (error, stream, pushHeaders) => {
      if (error) return callback(error);
      callback(null, new Http2ServerResponse(stream, pushHeaders));
    });
  }

  setTimeout(msecs, callback) {
    this.stream.setTimeout(msecs, callback);
    return this;
  }

  destroy(error) {
    this.stream.destroy(error);
  }
}

module.exports = { Http2ServerRequest, Http2ServerResponse };
