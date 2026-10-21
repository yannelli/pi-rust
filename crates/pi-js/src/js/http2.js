// node:http2: sessions and streams on the native h2 crate, servers on node:net and node:tls.
const net = require("net");
const tls = require("tls");
const { Buffer } = require("buffer");
const { codeError, invalidArgType, validate } = require("internal/errors");
const { METHODS, STATUS_CODES } = require("internal/http_common");
const core = require("internal/http2_core");
const { Http2Session, ServerHttp2Session, ClientHttp2Session, connect } = require("internal/http2_session");
const { Http2ServerRequest, Http2ServerResponse } = require("internal/http2_compat");

const kSessions = Symbol("kSessions");
const HEADERS = ["status", "method", "authority", "scheme", "path", "protocol", "accept-encoding", "accept-language", "accept-ranges", "accept", "access-control-allow-credentials", "access-control-allow-headers", "access-control-allow-methods", "access-control-allow-origin", "access-control-expose-headers", "access-control-request-headers", "access-control-request-method", "age", "authorization", "cache-control", "connection", "content-disposition", "content-encoding", "content-length", "content-type", "cookie", "date", "etag", "forwarded", "host", "if-modified-since", "if-none-match", "if-range", "last-modified", "link", "location", "range", "referer", "server", "set-cookie", "strict-transport-security", "transfer-encoding", "te", "upgrade-insecure-requests", "upgrade", "user-agent", "vary", "x-content-type-options", "x-frame-options", "keep-alive", "proxy-connection", "x-xss-protection", "alt-svc", "content-security-policy", "early-data", "expect-ct", "origin", "purpose", "timing-allow-origin", "x-forwarded-for", "priority", "accept-charset", "access-control-max-age", "allow", "content-language", "content-location", "content-md5", "content-range", "dnt", "expect", "expires", "from", "if-match", "if-unmodified-since", "max-forwards", "prefer", "proxy-authenticate", "proxy-authorization", "refresh", "retry-after", "trailer", "tk", "via", "warning", "www-authenticate", "http2-settings"];
const PSEUDO = new Set(["status", "method", "authority", "scheme", "path", "protocol"]);

const constants = {
  NGHTTP2_ERR_FRAME_SIZE_ERROR: -522, NGHTTP2_SESSION_SERVER: 0, NGHTTP2_SESSION_CLIENT: 1,
  NGHTTP2_STREAM_STATE_IDLE: 1, NGHTTP2_STREAM_STATE_OPEN: 2, NGHTTP2_STREAM_STATE_RESERVED_LOCAL: 3, NGHTTP2_STREAM_STATE_RESERVED_REMOTE: 4,
  NGHTTP2_STREAM_STATE_HALF_CLOSED_LOCAL: 5, NGHTTP2_STREAM_STATE_HALF_CLOSED_REMOTE: 6, NGHTTP2_STREAM_STATE_CLOSED: 7,
  NGHTTP2_FLAG_NONE: 0, NGHTTP2_FLAG_END_STREAM: 1, NGHTTP2_FLAG_END_HEADERS: 4, NGHTTP2_FLAG_ACK: 1, NGHTTP2_FLAG_PADDED: 8, NGHTTP2_FLAG_PRIORITY: 32,
  DEFAULT_SETTINGS_HEADER_TABLE_SIZE: 4096, DEFAULT_SETTINGS_ENABLE_PUSH: 1, DEFAULT_SETTINGS_MAX_CONCURRENT_STREAMS: 4294967295,
  DEFAULT_SETTINGS_INITIAL_WINDOW_SIZE: 65535, DEFAULT_SETTINGS_MAX_FRAME_SIZE: 16384, DEFAULT_SETTINGS_MAX_HEADER_LIST_SIZE: 65535, DEFAULT_SETTINGS_ENABLE_CONNECT_PROTOCOL: 0,
  MAX_MAX_FRAME_SIZE: 16777215, MIN_MAX_FRAME_SIZE: 16384, MAX_INITIAL_WINDOW_SIZE: 2147483647, NGHTTP2_DEFAULT_WEIGHT: 16,
  NGHTTP2_SETTINGS_HEADER_TABLE_SIZE: 1, NGHTTP2_SETTINGS_ENABLE_PUSH: 2, NGHTTP2_SETTINGS_MAX_CONCURRENT_STREAMS: 3, NGHTTP2_SETTINGS_INITIAL_WINDOW_SIZE: 4,
  NGHTTP2_SETTINGS_MAX_FRAME_SIZE: 5, NGHTTP2_SETTINGS_MAX_HEADER_LIST_SIZE: 6, NGHTTP2_SETTINGS_ENABLE_CONNECT_PROTOCOL: 8,
  PADDING_STRATEGY_NONE: 0, PADDING_STRATEGY_ALIGNED: 1, PADDING_STRATEGY_MAX: 2, PADDING_STRATEGY_CALLBACK: 1,
};
core.ERROR_NAMES.forEach((name, code) => (constants[name] = code));
for (const name of HEADERS) constants[`HTTP2_HEADER_${name.toUpperCase().replace(/-/g, "_")}`] = PSEUDO.has(name) ? `:${name}` : name;
for (const method of METHODS) constants[`HTTP2_METHOD_${method.replace(/-/g, "_")}`] = method;
for (const [code, text] of Object.entries(STATUS_CODES)) {
  constants[`HTTP_STATUS_${code === "418" ? "TEAPOT" : text.toUpperCase().replace(/[^A-Z0-9]+/g, "_")}`] = Number(code);
}

const SETTING_IDS = [["headerTableSize", 1], ["enablePush", 2], ["maxConcurrentStreams", 3], ["initialWindowSize", 4], ["maxFrameSize", 5], ["maxHeaderListSize", 6], ["enableConnectProtocol", 8]];

function getPackedSettings(settings = {}) {
  settings = core.validateSettings(settings);
  const entries = SETTING_IDS.filter(([name]) => settings[name] !== undefined);
  const buffer = Buffer.alloc(entries.length * 6);
  entries.forEach(([name, id], index) => {
    buffer.writeUInt16BE(id, index * 6);
    buffer.writeUInt32BE(Number(settings[name]), index * 6 + 2);
  });
  return buffer;
}

function getUnpackedSettings(buffer, options = {}) {
  if (!ArrayBuffer.isView(buffer)) throw invalidArgType("buf", "an instance of Buffer or TypedArray", buffer);
  if (buffer.byteLength % 6 !== 0) throw codeError(RangeError, "ERR_HTTP2_INVALID_PACKED_SETTINGS_LENGTH", "Packed settings length must be a multiple of six");
  const bytes = Buffer.from(buffer.buffer, buffer.byteOffset, buffer.byteLength);
  const settings = {};
  for (let offset = 0; offset < bytes.length; offset += 6) {
    const id = bytes.readUInt16BE(offset);
    const value = bytes.readUInt32BE(offset + 2);
    const entry = SETTING_IDS.find(([, known]) => known === id);
    if (!entry) (settings.customSettings || (settings.customSettings = {}))[id] = value;
    else settings[entry[0]] = entry[0] === "enablePush" || entry[0] === "enableConnectProtocol" ? value !== 0 : value;
  }
  if (options.validate) core.validateSettings(settings);
  return settings;
}

function onStream(server, stream, headers, flags, rawHeaders) {
  server.emit("stream", stream, headers, flags, rawHeaders);
  if (server.listenerCount("request") === 0) return;
  const Request = server._h2options.Http2ServerRequest || Http2ServerRequest;
  const Response = server._h2options.Http2ServerResponse || Http2ServerResponse;
  const request = new Request(stream, headers, undefined, rawHeaders);
  const response = new Response(stream, headers, request);
  if (headers.expect !== undefined) {
    if (/^100-continue$/i.test(headers.expect)) {
      if (server.listenerCount("checkContinue")) return server.emit("checkContinue", request, response);
      response.writeContinue();
    } else if (server.listenerCount("checkExpectation")) return server.emit("checkExpectation", request, response);
    else {
      response.statusCode = 417;
      return response.end();
    }
  }
  server.emit("request", request, response);
}

function startSession(server, socket) {
  const session = new ServerHttp2Session(server._h2options, socket, server);
  server[kSessions].add(session);
  session.on("close", () => server[kSessions].delete(session));
  session.on("stream", (stream, headers, flags, raw) => onStream(server, stream, headers, flags, raw));
  session.on("error", (error) => server.emit("sessionError", error, session));
  if (server.timeout) {
    session.setTimeout(server.timeout, () => {
      if (!server.emit("timeout", session)) session.destroy();
    });
  }
  server.emit("session", session);
}

function setupServer(server, options, onRequest) {
  core.validateRemoteCustomSettings(options?.remoteCustomSettings);
  server._h2options = { ...options };
  server.timeout = 0;
  server[kSessions] = new Set();
  if (onRequest) server.on("request", onRequest);
}

const serverMethods = {
  setTimeout(msecs, callback) {
    this.timeout = msecs;
    if (callback) this.on("timeout", callback);
    return this;
  },
  updateSettings(settings) {
    this._h2options.settings = { ...this._h2options.settings, ...core.validateSettings(settings) };
  },
  close(callback) {
    for (const session of this[kSessions]) session.close();
    return net.Server.prototype.close.call(this, callback);
  },
};

class Http2Server extends net.Server {
  constructor(options, onRequest) {
    super({ ...options });
    setupServer(this, options, onRequest);
    this.on("connection", (socket) => startSession(this, socket));
  }
}
Object.assign(Http2Server.prototype, serverMethods);

class Http2SecureServer extends tls.Server {
  constructor(options, onRequest) {
    options = { ...options };
    const alpn = options.ALPNProtocols || (options.allowHTTP1 ? ["h2", "http/1.1"] : ["h2"]);
    super({ ...options, ALPNProtocols: alpn });
    setupServer(this, options, onRequest);
    if (options.allowHTTP1) require("internal/http_server").setupServer(this, options);
    this.on("secureConnection", (socket) => {
      if (socket.alpnProtocol === "h2") return startSession(this, socket);
      if (options.allowHTTP1) return require("internal/http_server").connectionListener.call(this, socket);
      if (!this.emit("unknownProtocol", socket)) socket.destroy();
    });
  }
}
Object.assign(Http2SecureServer.prototype, serverMethods);

const sensitiveHeaders = Symbol("nodejs.http2.sensitiveHeaders");

module.exports = {
  connect,
  constants,
  createServer: (options, onRequest) => {
    if (typeof options === "function") [onRequest, options] = [options, {}];
    if (options !== undefined) validate.object(options, "options");
    return new Http2Server(options || {}, onRequest);
  },
  createSecureServer: (options, onRequest) => {
    if (typeof options === "function") [onRequest, options] = [options, {}];
    if (options !== undefined) validate.object(options, "options");
    return new Http2SecureServer(options || {}, onRequest);
  },
  getDefaultSettings: () => ({ ...core.DEFAULT_SETTINGS }),
  getPackedSettings,
  getUnpackedSettings,
  performServerHandshake() {
    throw internal.unsupported("http2.performServerHandshake");
  },
  sensitiveHeaders,
  Http2ServerRequest,
  Http2ServerResponse,
  Http2Session,
  ServerHttp2Session,
  ClientHttp2Session,
};
