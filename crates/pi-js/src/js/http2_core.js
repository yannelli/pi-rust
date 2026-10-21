// node:http2 sessions and streams. Frames go through native h2 sessions fed by a net or TLS socket.
const EventEmitter = require("events");
const { Duplex } = require("stream");
const { Buffer } = require("buffer");
const { codeError, invalidArgType, validate } = require("internal/errors");
const nativeH2 = native.http2;

const ERROR_NAMES = ["NGHTTP2_NO_ERROR", "NGHTTP2_PROTOCOL_ERROR", "NGHTTP2_INTERNAL_ERROR", "NGHTTP2_FLOW_CONTROL_ERROR", "NGHTTP2_SETTINGS_TIMEOUT", "NGHTTP2_STREAM_CLOSED", "NGHTTP2_FRAME_SIZE_ERROR", "NGHTTP2_REFUSED_STREAM", "NGHTTP2_CANCEL", "NGHTTP2_COMPRESSION_ERROR", "NGHTTP2_CONNECT_ERROR", "NGHTTP2_ENHANCE_YOUR_CALM", "NGHTTP2_INADEQUATE_SECURITY", "NGHTTP2_HTTP_1_1_REQUIRED"];
const DEFAULT_SETTINGS = { headerTableSize: 4096, enablePush: true, initialWindowSize: 65535, maxFrameSize: 16384, maxConcurrentStreams: 4294967295, maxHeaderListSize: 65535, maxHeaderSize: 65535, enableConnectProtocol: false };
const SETTING_RANGES = { headerTableSize: [0, 2 ** 32 - 1], initialWindowSize: [0, 2 ** 31 - 1], maxFrameSize: [16384, 2 ** 24 - 1], maxConcurrentStreams: [0, 2 ** 32 - 1], maxHeaderListSize: [0, 2 ** 32 - 1], maxHeaderSize: [0, 2 ** 32 - 1] };
const CONNECTION_HEADERS = new Set(["connection", "upgrade", "http2-settings", "te", "transfer-encoding", "keep-alive", "proxy-connection"]);
const NO_PAYLOAD = new Set(["GET", "HEAD", "DELETE"]);
const kNative = Symbol("kNative");

const streamError = (code) => codeError(Error, "ERR_HTTP2_STREAM_ERROR", `Stream closed with error code ${ERROR_NAMES[code] || code}`);

const MAX_CUSTOM_SETTINGS = 10;
const tooManyCustom = () => codeError(Error, "ERR_HTTP2_TOO_MANY_CUSTOM_SETTINGS", "Number of custom settings exceeds MAX_ADDITIONAL_SETTINGS");
const invalidSetting = (name, value) => codeError(RangeError, "ERR_HTTP2_INVALID_SETTING_VALUE", `Invalid value for setting "${name}": ${value}`);

function validateCustomSettings(custom) {
  if (custom === undefined) return;
  if (custom === null || typeof custom !== "object" || Array.isArray(custom)) throw invalidArgType("customSettings", "an instance of Number", custom);
  const entries = Object.entries(custom);
  if (entries.length > MAX_CUSTOM_SETTINGS) throw tooManyCustom();
  for (const [key, value] of entries) {
    if (Number(key) < 0 || Number(key) > 0xffff) throw invalidSetting("customSettings:id", Number(key));
    if (Number(value) < 0 || Number(value) > 2 ** 32 - 1) throw invalidSetting("customSettings:value", Number(value));
  }
}

// The entries of a customSettings object for a SETTINGS frame, as flat id and value pairs. Like
// Node, it skips values that are not numbers; ids of the standard settings are left out.
function customEntries(custom, known) {
  const list = [];
  for (const key in custom) {
    const value = custom[key];
    if (typeof value !== "number") continue;
    const id = Number(key);
    if (!(id > 0 && id <= 0xffff)) throw invalidSetting("Range Error", id);
    if (!(value > 0 && value <= 2 ** 32 - 1)) throw invalidSetting("Range Error", value);
    if (!known[id]) list.push(id, value);
  }
  return list;
}

// remoteCustomSettings: the ids of the peer's custom settings that remoteSettings reports.
function validateRemoteCustomSettings(list) {
  if (list === undefined) return;
  if (!Array.isArray(list)) throw invalidArgType("options.remoteCustomSettings", "an instance of Array", list);
  if (list.length > MAX_CUSTOM_SETTINGS) throw tooManyCustom();
}

function validateSettings(settings) {
  if (settings === undefined) return {};
  validate.object(settings, "settings");
  validateCustomSettings(settings.customSettings);
  for (const [name, [min, max]] of Object.entries(SETTING_RANGES)) {
    const value = settings[name];
    if (value !== undefined && (typeof value !== "number" || !Number.isInteger(value) || value < min || value > max)) {
      throw codeError(RangeError, "ERR_HTTP2_INVALID_SETTING_VALUE", `Invalid value for setting "${name}": ${value}`);
    }
  }
  for (const name of ["enablePush", "enableConnectProtocol"]) {
    if (settings[name] !== undefined && typeof settings[name] !== "boolean") throw codeError(TypeError, "ERR_HTTP2_INVALID_SETTING_VALUE", `Invalid value for setting "${name}": ${settings[name]}`);
  }
  return { ...settings };
}

// Turn a flat [name, value, ...] list into Node's header object.
function toHeaders(raw) {
  const headers = { __proto__: null };
  for (let index = 0; index < raw.length; index += 2) {
    const name = raw[index];
    const value = name === ":status" ? Number(raw[index + 1]) : raw[index + 1];
    if (!(name in headers)) headers[name] = name === "set-cookie" ? [value] : value;
    else if (name === "set-cookie") headers[name].push(value);
    else if (name[0] !== ":") headers[name] += name === "cookie" ? `; ${value}` : `, ${value}`;
  }
  return headers;
}

// Validate a header object and flatten it for the native session, pseudo-headers first.
function flatten(headers, allowPseudo) {
  if (headers === undefined || headers === null) return [];
  if (typeof headers !== "object" || Array.isArray(headers)) throw invalidArgType("headers", "of type object", headers);
  const pseudo = [];
  const rest = [];
  for (const key of Object.keys(headers)) {
    const value = headers[key];
    if (value === undefined) continue;
    const name = key.toLowerCase();
    if (name[0] === ":") {
      if (!allowPseudo.includes(name)) throw codeError(TypeError, "ERR_HTTP2_INVALID_PSEUDOHEADER", `"${name}" is an invalid pseudoheader or is used incorrectly`);
      if (Array.isArray(value)) throw codeError(TypeError, "ERR_HTTP2_HEADER_SINGLE_VALUE", `Header field "${name}" must only have a single value`);
      pseudo.push(name, String(value));
    } else if (CONNECTION_HEADERS.has(name) && !(name === "te" && String(value).toLowerCase() === "trailers")) {
      throw codeError(TypeError, "ERR_HTTP2_INVALID_CONNECTION_HEADERS", `HTTP/1 Connection specific headers are forbidden: "${name}"`);
    } else {
      for (const item of [].concat(value)) rest.push(name, String(item));
    }
  }
  return pseudo.concat(rest);
}

class Http2Stream extends Duplex {
  constructor(session, key, options) {
    super({ allowHalfOpen: true, decodeStrings: false, autoDestroy: false, ...options });
    const maybeDestroy = () => {
      if (this.readableEnded && this.writableFinished) setImmediate(() => this.destroy());
    };
    this.once("end", maybeDestroy);
    this.once("finish", maybeDestroy);
    Object.assign(this, { session, _key: key, id: undefined, rstCode: undefined, sentHeaders: undefined, sentTrailers: undefined, sentInfoHeaders: undefined });
    Object.assign(this, { headersSent: false, aborted: false, _closed: false, endAfterHeaders: false, _endSent: false, _waitForTrailers: false, _timer: null, _timeout: 0 });
    this.once("close", () => {
      this._closed = true;
      if (this._timer) clearTimeout(this._timer);
      session._streams.delete(key);
      if (session[kNative] !== null) nativeH2.forget(session[kNative], key);
      session._maybeFinishClose();
    });
  }

  get pending() { return this.id === undefined; }
  get closed() { return this._closed; }
  get bufferSize() { return this.writableLength; }
  get pushAllowed() {
    const session = this.session;
    return !this.destroyed && !this.closed && !session.closed && !session.destroyed && !!session.remoteSettings.enablePush;
  }
  get state() {
    return { localWindowSize: 65535, state: this.closed ? 7 : 2, localClose: this.writableFinished ? 1 : 0, remoteClose: this.readableEnded ? 1 : 0, sumDependencyWeight: 0, weight: 16 };
  }

  _touch() {
    if (!this._timeout) return;
    if (this._timer) clearTimeout(this._timer);
    this._timer = setTimeout(() => this.emit("timeout"), this._timeout);
    if (this._timer.unref) this._timer.unref();
  }

  setTimeout(msecs, callback) {
    this._timeout = msecs;
    if (callback) this.once("timeout", callback);
    if (msecs) this._touch();
    else if (this._timer) clearTimeout(this._timer);
    return this;
  }

  _beforeWrite() {}

  _write(chunk, encoding, callback) {
    if (typeof chunk === "string") chunk = Buffer.from(chunk, encoding);
    this._beforeWrite();
    this._touch();
    nativeH2.write(this.session[kNative], this._key, chunk, false).then(() => callback(), callback);
  }

  _final(callback) {
    this._beforeWrite(true);
    if (this._endSent) return callback();
    this._endSent = true;
    if (this._waitForTrailers) {
      process.nextTick(() => this.emit("wantTrailers"));
      return callback();
    }
    nativeH2.write(this.session[kNative], this._key, Buffer.alloc(0), true).then(() => callback(), callback);
  }

  _read() {}

  _onNative(event, value, end) {
    this._touch();
    if (event === "data") this.push(Buffer.from(value.buffer, value.byteOffset, value.length));
    else if (event === "end") this.push(null);
    else if (event === "trailers") this.emit("trailers", toHeaders(value), 0, value);
    else if (event === "reset") {
      this.rstCode = value;
      if (value !== 0) this.aborted = !this.writableEnded;
      this.destroy(value !== 0 && value !== 8 ? streamError(value) : undefined);
    } else if (event === "streamError") this.destroy(value);
    return end;
  }

  sendTrailers(headers) {
    if (this.destroyed || this.closed) throw codeError(Error, "ERR_HTTP2_INVALID_STREAM", "The stream has been destroyed");
    if (this.sentTrailers) throw codeError(Error, "ERR_HTTP2_TRAILERS_ALREADY_SENT", "Trailing headers have already been sent");
    if (!this._waitForTrailers) throw codeError(Error, "ERR_HTTP2_TRAILERS_NOT_READY", "Trailing headers cannot be sent until after the wantTrailers event is emitted");
    this.sentTrailers = headers;
    nativeH2.trailers(this.session[kNative], this._key, flatten(headers, []));
  }

  priority() {}

  close(code = 0, callback) {
    validate.integer(code, "code", 0, 2 ** 32 - 1);
    if (callback) this.once("close", callback);
    if (this.closed || this.destroyed) return;
    this.rstCode = code;
    if (this.session[kNative] !== null) nativeH2.reset(this.session[kNative], this._key, code);
    this.destroy(code !== 0 && code !== 8 ? streamError(code) : undefined);
  }

  _destroy(error, callback) {
    const done = this.readableEnded && this.writableFinished;
    if (this.rstCode === undefined) this.rstCode = error ? 2 : done ? 0 : 8;
    if (!done && this.session[kNative] !== null) nativeH2.reset(this.session[kNative], this._key, this.rstCode);
    callback(error);
  }
}

class ServerHttp2Stream extends Http2Stream {
  get headersSent() { return this._headersSent; }
  set headersSent(value) { this._headersSent = value; }

  _beforeWrite(ending) {
    if (!this._headersSent) this.respond({ ":status": 200 }, ending ? { endStream: true } : {});
  }

  respond(headers = {}, options = {}) {
    if (this.destroyed || this.closed) throw codeError(Error, "ERR_HTTP2_INVALID_STREAM", "The stream has been destroyed");
    if (this._headersSent) throw codeError(Error, "ERR_HTTP2_HEADERS_SENT", "Response has already been initiated.");
    headers = { ...headers };
    if (headers[":status"] === undefined) headers[":status"] = 200;
    const status = Number(headers[":status"]);
    if (!Number.isInteger(status) || status < 200 || status > 599) throw codeError(RangeError, "ERR_HTTP2_STATUS_INVALID", `Invalid status code: ${headers[":status"]}`);
    const flat = flatten(headers, [":status"]);
    this._headersSent = true;
    this.sentHeaders = headers;
    this._waitForTrailers = !!options.waitForTrailers;
    let end = !!options.endStream || status === 204 || status === 304 || !!this._isHead;
    if (end && this._waitForTrailers) end = false;
    nativeH2.respond(this.session[kNative], this._key, flat, end);
    if (end) {
      this._endSent = true;
      this.end();
    }
  }

  additionalHeaders(headers) {
    if (this._headersSent) throw codeError(Error, "ERR_HTTP2_HEADERS_AFTER_RESPOND", "Cannot specify additional headers after response initiated");
    const status = Number(headers[":status"]);
    if (!(status >= 100 && status < 200) || status === 101) throw codeError(RangeError, "ERR_HTTP2_INVALID_INFO_STATUS", `Invalid informational status code: ${headers[":status"]}`);
    (this.sentInfoHeaders || (this.sentInfoHeaders = [])).push(headers);
    nativeH2.info(this.session[kNative], this._key, flatten(headers, [":status"]));
  }

  respondWithFile(path, headers, options = {}) {
    require("fs").readFile(path, (error, data) => this._respondWith(error, data, headers, options));
  }

  respondWithFD(fd, headers, options = {}) {
    let data;
    try {
      data = require("fs").readFileSync(typeof fd === "number" ? fd : fd.fd);
    } catch (error) {
      return this._respondWith(error, null, headers, options);
    }
    this._respondWith(null, data, headers, options);
  }

  _respondWith(error, data, headers, options) {
    if (error) {
      if (typeof options.onError === "function") return options.onError(error);
      return this.destroy(error);
    }
    const offset = options.offset || 0;
    const body = data.subarray(offset, options.length >= 0 ? offset + options.length : undefined);
    this.respond({ ...headers, "content-length": body.length }, { waitForTrailers: options.waitForTrailers });
    this.end(body);
  }

  pushStream(headers, options, callback) {
    if (!this.pushAllowed) throw codeError(Error, "ERR_HTTP2_PUSH_DISABLED", "HTTP/2 client has disabled push streams");
    if (this.id % 2 === 0) throw codeError(Error, "ERR_HTTP2_NESTED_PUSH", "A push stream cannot initiate another push stream.");
    if (typeof options === "function") [callback, options] = [options, undefined];
    validate.function(callback, "callback");
    if (options !== undefined) validate.object(options, "options");
    options = { ...options };
    if (headers === null || typeof headers !== "object") throw invalidArgType("headers", "of type object", headers);
    headers = Object.assign({ __proto__: null }, headers);
    if (headers[":method"] === undefined) headers[":method"] = "GET";
    if (headers[":authority"] === undefined && headers.host === undefined) headers[":authority"] = this._authority;
    if (headers[":scheme"] === undefined) headers[":scheme"] = this._scheme;
    if (headers[":path"] === undefined) headers[":path"] = "/";
    const isHead = headers[":method"] === "HEAD";
    if (isHead) options.endStream = true;
    const flat = flatten(headers, [":method", ":path", ":scheme", ":authority"]);
    const session = this.session;
    const key = session._pushKey++;
    const stream = new ServerHttp2Stream(session, key, {});
    Object.assign(stream, { sentHeaders: headers, _isHead: isHead, _authority: this._authority, _scheme: this._scheme });
    // The callback runs once the PUSH_PROMISE has its stream id, or with the error.
    stream._pushCallback = (error) => (error ? callback(error) : callback(null, stream, headers, 0));
    session._streams.set(key, stream);
    stream.push(null);
    if (options.endStream) stream.end();
    nativeH2.push(session[kNative], this._key, key, flat);
  }

  _onNative(event, value, end) {
    const pending = this._pushCallback;
    if (pending && (event === "open" || event === "streamError")) {
      this._pushCallback = null;
      if (event === "streamError") {
        pending(value);
        return this.destroy();
      }
      this.id = value;
      return pending(null);
    }
    return super._onNative(event, value, end);
  }
}

class ClientHttp2Stream extends Http2Stream {
  _onNative(event, value, end) {
    if (event === "pushed") {
      // A pushed stream's response headers.
      this.headersSent = true;
      this.emit("push", toHeaders(value), end ? 5 : 4, value);
      if (end) this.push(null);
    } else if (event === "open") {
      this.id = value;
      this.emit("ready");
    } else if (event === "headers") this.emit("headers", toHeaders(value), 0, value);
    else if (event === "response") {
      this.headersSent = true;
      const headers = toHeaders(value);
      this.endAfterHeaders = !!end;
      this.emit("response", headers, end ? 1 : 4, value);
      if (end) this.push(null);
    } else return super._onNative(event, value, end);
  }
}

module.exports = { Http2Stream, ServerHttp2Stream, ClientHttp2Stream, kNative, toHeaders, flatten, validateSettings, customEntries, validateRemoteCustomSettings, invalidSetting, DEFAULT_SETTINGS, ERROR_NAMES, NO_PAYLOAD, nativeH2, EventEmitter };
