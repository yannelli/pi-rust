// Http2Session over a net or TLS socket: client and server sessions and http2.connect.
const net = require("net");
const { Buffer } = require("buffer");
const { codeError, invalidArgType, validate } = require("internal/errors");
const core = require("internal/http2_core");
const { ServerHttp2Stream, ClientHttp2Stream, kNative, toHeaders, flatten, validateSettings, DEFAULT_SETTINGS, NO_PAYLOAD, nativeH2, EventEmitter } = core;

const sessionError = (message, code = "ERR_HTTP2_INVALID_SESSION") => codeError(Error, code, message);

class Http2Session extends EventEmitter {
  constructor(type, options, socket) {
    super();
    const settings = validateSettings(options.settings);
    Object.assign(this, { type, socket, _streams: new Map(), _nextKey: 1, closed: false, destroyed: false, connecting: true, _closeCallback: null, _goawaySent: false });
    this.localSettings = { ...DEFAULT_SETTINGS, ...settings };
    this.remoteSettings = { ...DEFAULT_SETTINGS };
    this.pendingSettingsAck = true;
    this.encrypted = !!socket.encrypted;
    this.alpnProtocol = socket.encrypted ? socket.alpnProtocol || "h2" : "h2c";
    this.originSet = undefined;
    this[kNative] = nativeH2.session(type === 0, settings, (event, key, value, end, id) => this._onNative(event, key, value, end, id));
    this._onData = (chunk) => this[kNative] !== null && nativeH2.feed(this[kNative], chunk);
    this._onEnd = () => this[kNative] !== null && nativeH2.endInput(this[kNative]);
    this._onClose = () => this.destroy();
    this._onError = (error) => this.destroy(error);
    this._onTimeout = () => this.emit("timeout");
    socket.on("data", this._onData);
    socket.on("end", this._onEnd);
    socket.on("close", this._onClose);
    socket.on("error", this._onError);
    socket.on("timeout", this._onTimeout);
  }

  _onNative(event, key, value, end, id) {
    if (this.destroyed) return;
    if (event === "out") {
      if (!this.socket.destroyed) this.socket.write(Buffer.from(value.buffer, value.byteOffset, value.length));
    } else if (event === "ready") {
      this.remoteSettings = { ...DEFAULT_SETTINGS, maxConcurrentStreams: value.maxConcurrentStreams };
      this.pendingSettingsAck = false;
      this._ready();
      this.emit("localSettings", { ...this.localSettings });
      this.emit("remoteSettings", { ...this.remoteSettings });
    } else if (event === "closed") this._onClosed(value);
    else if (event === "stream") this._onStream(key, value, end, id);
    else {
      const stream = this._streams.get(key);
      if (stream) stream._onNative(event, value, end);
    }
  }

  _ready() {
    this.connecting = false;
  }

  _onClosed(info) {
    if (info && info[1]) this.emit("goaway", info[0], 0, Buffer.alloc(0));
    else if (!info && this.type === 1 && !this.closed) this.emit("goaway", 0, 0, Buffer.alloc(0));
    const error = info && info[0] !== 0 && !info[1] ? codeError(Error, "ERR_HTTP2_SESSION_ERROR", `Session closed with error code ${info[0]}`) : undefined;
    this.closed = true;
    setImmediate(() => this.destroy(error));
  }

  get state() {
    return { effectiveLocalWindowSize: 65535, effectiveRecvDataLength: 0, nextStreamID: this._nextKey * 2 - 1, localWindowSize: 65535, lastProcStreamID: 0, remoteWindowSize: 65535, outboundQueueSize: 1, deflateDynamicTableSize: 0, inflateDynamicTableSize: 0 };
  }

  ping(payload, callback) {
    if (typeof payload === "function") [callback, payload] = [payload, undefined];
    if (payload !== undefined && !ArrayBuffer.isView(payload)) throw invalidArgType("payload", "an instance of Buffer, TypedArray, or DataView", payload);
    if (payload !== undefined && payload.byteLength !== 8) throw codeError(RangeError, "ERR_HTTP2_PING_LENGTH", "HTTP2 ping payload must be 8 bytes");
    validate.function(callback, "callback");
    if (this.destroyed) throw sessionError("The session has been destroyed");
    const echoed = payload ? Buffer.from(payload.buffer, payload.byteOffset, 8) : Buffer.alloc(8);
    nativeH2.ping(this[kNative]).then((duration) => callback(null, duration, echoed), (error) => callback(error, 0, echoed));
    return true;
  }

  settings(settings, callback) {
    if (this.destroyed) throw sessionError("The session has been destroyed");
    settings = validateSettings(settings);
    if (callback !== undefined) validate.function(callback, "callback");
    Object.assign(this.localSettings, settings);
    if (settings.initialWindowSize !== undefined) nativeH2.windowSize(this[kNative], settings.initialWindowSize);
    this.pendingSettingsAck = true;
    process.nextTick(() => {
      this.pendingSettingsAck = false;
      this.emit("localSettings", { ...this.localSettings });
      if (callback) callback(null, { ...this.localSettings }, 0);
    });
  }

  setLocalWindowSize(size) {
    validate.integer(size, "windowSize", 0, 2 ** 31 - 1);
  }

  goaway(code = 0, lastStreamID = 0, opaqueData) {
    if (this.destroyed) throw sessionError("The session has been destroyed");
    validate.integer(code, "code", 0, 2 ** 32 - 1);
    validate.integer(lastStreamID, "lastStreamID", 0, 2 ** 31 - 1);
    if (opaqueData !== undefined && !ArrayBuffer.isView(opaqueData)) throw invalidArgType("opaqueData", "an instance of Buffer, TypedArray, or DataView", opaqueData);
    this._goawaySent = true;
    nativeH2.goaway(this[kNative], code);
  }

  close(callback) {
    if (this.closed || this.destroyed) return;
    this.closed = true;
    if (typeof callback === "function") this.once("close", callback);
    if (this.type === 0 && !this._goawaySent) nativeH2.goaway(this[kNative], 0);
    this._maybeFinishClose();
  }

  _maybeFinishClose() {
    if (this.closed && !this.destroyed && this._streams.size === 0) process.nextTick(() => this.destroy());
  }

  destroy(error, code) {
    if (this.destroyed) return;
    if (typeof error === "number") [code, error] = [error, undefined];
    this.destroyed = true;
    this.closed = true;
    for (const stream of this._streams.values()) stream.destroy(error);
    this._streams.clear();
    const id = this[kNative];
    this[kNative] = null;
    if (id !== null) {
      if (code) nativeH2.goaway(id, code);
      setTimeout(() => nativeH2.close(id), 0);
    }
    const socket = this.socket;
    socket.removeListener("data", this._onData);
    socket.removeListener("end", this._onEnd);
    socket.removeListener("close", this._onClose);
    socket.removeListener("timeout", this._onTimeout);
    socket.on("error", () => {});
    socket.removeListener("error", this._onError);
    if (!socket.destroyed) socket.end(() => socket.destroy());
    process.nextTick(() => {
      if (error) this.emit("error", error);
      this.emit("close");
    });
  }

  setTimeout(msecs, callback) {
    if (callback) this.once("timeout", callback);
    this.socket.setTimeout(msecs);
    return this;
  }

  ref() {
    this.socket.ref();
  }

  unref() {
    this.socket.unref();
  }
}

class ServerHttp2Session extends Http2Session {
  constructor(options, socket, server) {
    super(0, options, socket);
    this.server = server;
  }

  _onStream(key, raw, end, id) {
    const stream = new ServerHttp2Stream(this, key, {});
    stream.id = id;
    this._streams.set(key, stream);
    const headers = toHeaders(raw);
    stream._isHead = headers[":method"] === "HEAD";
    if (end) stream.push(null);
    this.emit("stream", stream, headers, end ? 5 : 4, raw);
  }

  altsvc() {}

  origin() {}
}

class ClientHttp2Session extends Http2Session {
  constructor(options, socket, authority, scheme) {
    super(1, options, socket);
    Object.assign(this, { _authority: authority, _scheme: scheme });
    const connected = () => {
      this.connecting = false;
      this.emit("connect", this, socket);
    };
    if (socket.connecting) socket.once(socket.encrypted ? "secureConnect" : "connect", connected);
    else process.nextTick(connected);
  }

  _ready() {}

  request(headers = {}, options = {}) {
    if (this.destroyed) throw sessionError("The session has been destroyed");
    if (this.closed) throw codeError(Error, "ERR_HTTP2_GOAWAY_SESSION", "New streams cannot be created after receiving a GOAWAY");
    validate.object(options, "options");
    headers = { ...headers };
    const method = (headers[":method"] = String(headers[":method"] || "GET").toUpperCase());
    if (method === "CONNECT" && !headers[":protocol"]) {
      if (headers[":authority"] === undefined) headers[":authority"] = this._authority;
      if (headers[":path"] !== undefined || headers[":scheme"] !== undefined) throw codeError(Error, "ERR_HTTP2_CONNECT_PATH", "The :path header is forbidden for CONNECT requests");
    } else {
      if (headers[":authority"] === undefined && headers.host === undefined) headers[":authority"] = this._authority;
      if (headers[":scheme"] === undefined) headers[":scheme"] = this._scheme;
      if (headers[":path"] === undefined) headers[":path"] = "/";
    }
    const flat = flatten(headers, [":method", ":path", ":scheme", ":authority", ":protocol"]);
    const endStream = options.endStream === undefined ? NO_PAYLOAD.has(method) : !!options.endStream;
    const key = this._nextKey++;
    const stream = new ClientHttp2Stream(this, key, {});
    stream.sentHeaders = headers;
    stream._waitForTrailers = !!options.waitForTrailers;
    this._streams.set(key, stream);
    nativeH2.request(this[kNative], key, flat, endStream && !stream._waitForTrailers);
    if (endStream) {
      stream._endSent = !stream._waitForTrailers;
      stream.end();
    }
    if (options.signal) {
      const abort = () => stream.destroy(Object.assign(new Error("The operation was aborted"), { name: "AbortError", code: "ABORT_ERR" }));
      if (options.signal.aborted) process.nextTick(abort);
      else options.signal.addEventListener("abort", abort, { once: true });
    }
    return stream;
  }
}

function connect(authority, options, listener) {
  if (typeof options === "function") [listener, options] = [options, {}];
  options = { ...options };
  if (typeof authority === "string") authority = new URL(authority);
  else if (!(authority instanceof URL)) authority = new URL(`${authority.protocol || "http:"}//${authority.host || authority.hostname || "localhost"}${authority.port ? `:${authority.port}` : ""}`);
  const protocol = authority.protocol || options.protocol || "https:";
  if (protocol !== "http:" && protocol !== "https:") throw codeError(Error, "ERR_HTTP2_UNSUPPORTED_PROTOCOL", `protocol "${protocol}" is unsupported.`);
  const port = `${authority.port !== "" ? authority.port : protocol === "http:" ? 80 : 443}`;
  const host = authority.hostname.replace(/^\[|\]$/g, "") || "localhost";
  let socket;
  if (typeof options.createConnection === "function") socket = options.createConnection(authority, options);
  else if (protocol === "http:") socket = net.connect({ ...options, port, host });
  else {
    const servername = options.servername || (net.isIP(host) ? undefined : host);
    socket = require("tls").connect({ ...options, port, host, servername, ALPNProtocols: ["h2"] });
  }
  const session = new ClientHttp2Session(options, socket, `${options.servername || authority.hostname}:${port}`, protocol.slice(0, -1));
  if (listener) session.once("connect", listener);
  return session;
}

module.exports = { Http2Session, ServerHttp2Session, ClientHttp2Session, connect };
