// internal/fetch: Request, Response, and fetch() on the native reqwest client.
const { Buffer } = require("buffer");
const { ReadableStream } = require("stream/web");
const { Headers, FormData, BodyMixin, kBody, extractBody } = require("internal/fetch_body");
const nativeFetch = native.fetch;

const kMeta = Symbol("kMeta");
const METHODS = ["DELETE", "GET", "HEAD", "OPTIONS", "POST", "PUT", "PATCH"];
const REDIRECT_STATUSES = [301, 302, 303, 307, 308];

function parseUrl(input) {
  try {
    return new URL(String(input));
  } catch (cause) {
    throw Object.assign(new TypeError(`Failed to parse URL from ${input}`), { cause });
  }
}

function attachBody(target, init, headers) {
  const extracted = extractBody(init);
  if (extracted.type && !headers.has("content-type")) headers.set("content-type", extracted.type);
  target[kBody] = { source: extracted.source, stream: null, used: false };
}

class Request extends BodyMixin {
  constructor(input, init = {}) {
    super();
    const base = input instanceof Request ? input : null;
    const url = base ? base.url : parseUrl(input).href;
    let method = init.method === undefined ? (base ? base.method : "GET") : String(init.method);
    if (METHODS.includes(method.toUpperCase())) method = method.toUpperCase();
    const headers = new Headers(init.headers === undefined && base ? base.headers : init.headers);
    this[kMeta] = {
      url,
      method,
      headers,
      redirect: init.redirect || (base ? base.redirect : "follow"),
      signal: init.signal || (base ? base.signal : new AbortController().signal),
      credentials: init.credentials || "same-origin",
      mode: init.mode || "cors",
      cache: init.cache || "default",
      keepalive: !!init.keepalive,
      duplex: init.duplex || "half",
    };
    let body = init.body;
    if (body === undefined) body = base && !base.bodyUsed ? base.body : null;
    if (body !== null && body !== undefined && (method === "GET" || method === "HEAD")) {
      throw new TypeError("Request with GET/HEAD method cannot have body.");
    }
    attachBody(this, body, headers);
  }
  get url() {
    return this[kMeta].url;
  }
  get method() {
    return this[kMeta].method;
  }
  get headers() {
    return this[kMeta].headers;
  }
  get redirect() {
    return this[kMeta].redirect;
  }
  get signal() {
    return this[kMeta].signal;
  }
  get credentials() {
    return this[kMeta].credentials;
  }
  get mode() {
    return this[kMeta].mode;
  }
  get cache() {
    return this[kMeta].cache;
  }
  get keepalive() {
    return this[kMeta].keepalive;
  }
  get duplex() {
    return this[kMeta].duplex;
  }
  clone() {
    if (this.bodyUsed) throw new TypeError("Request.clone: Body has already been consumed.");
    const body = this.body ? teeInto(this) : null;
    return new Request(this, { body, headers: new Headers(this.headers) });
  }
  get [Symbol.toStringTag]() {
    return "Request";
  }
}

// Split a body stream so the original and the clone can both read it.
function teeInto(owner) {
  const [left, right] = owner.body.tee();
  owner[kBody].stream = left;
  owner[kBody].source = { stream: left };
  return right;
}

class Response extends BodyMixin {
  constructor(body = null, init = {}) {
    super();
    const status = init.status === undefined ? 200 : Number(init.status);
    if (!Number.isInteger(status) || status < 200 || status > 599) {
      throw new RangeError(`init["status"] must be in the range of 200 to 599, inclusive.`);
    }
    const headers = new Headers(init.headers);
    this[kMeta] = { status, statusText: init.statusText === undefined ? "" : String(init.statusText), headers, type: "default", url: "", redirected: false };
    attachBody(this, body, headers);
  }
  static json(data, init = {}) {
    const headers = new Headers(init.headers);
    if (!headers.has("content-type")) headers.set("content-type", "application/json");
    return new Response(JSON.stringify(data), { ...init, headers });
  }
  static error() {
    const response = new Response(null, { status: 200 });
    Object.assign(response[kMeta], { status: 0, type: "error" });
    return response;
  }
  static redirect(url, status = 302) {
    if (!REDIRECT_STATUSES.includes(status)) throw new RangeError(`Invalid status code ${status}`);
    return new Response(null, { status, headers: { location: parseUrl(url).href } });
  }
  get status() {
    return this[kMeta].status;
  }
  get ok() {
    return this[kMeta].status >= 200 && this[kMeta].status <= 299;
  }
  get statusText() {
    return this[kMeta].statusText;
  }
  get headers() {
    return this[kMeta].headers;
  }
  get type() {
    return this[kMeta].type;
  }
  get url() {
    return this[kMeta].url;
  }
  get redirected() {
    return this[kMeta].redirected;
  }
  clone() {
    if (this.bodyUsed) throw new TypeError("Response.clone: Body has already been consumed.");
    const clone = new Response(this.body ? teeInto(this) : null, { headers: new Headers(this.headers) });
    Object.assign(clone[kMeta], { ...this[kMeta], headers: clone[kMeta].headers });
    return clone;
  }
  get [Symbol.toStringTag]() {
    return "Response";
  }
}

// A response whose body stream is fed by the native request.
function networkResponse(head, request, stream) {
  const response = new Response(null, { headers: head.headers });
  Object.assign(response[kMeta], {
    status: head.status,
    statusText: head.statusText,
    type: "basic",
    url: head.url,
    redirected: head.url !== request.url && request.redirect === "follow",
  });
  response[kBody] = { source: { stream }, stream, used: false };
  return response;
}

function dataResponse(url) {
  const match = /^data:([^,]*?)(;base64)?,(.*)$/s.exec(url.href);
  if (!match) throw new TypeError("fetch failed", { cause: new Error("invalid data: URL") });
  const payload = match[2] ? Buffer.from(decodeURIComponent(match[3]), "base64") : Buffer.from(decodeURIComponent(match[3]));
  return new Response(new Uint8Array(payload), { headers: { "content-type": match[1] || "text/plain;charset=US-ASCII" } });
}

function abortReason(signal) {
  return signal.reason === undefined ? new DOMException("This operation was aborted", "AbortError") : signal.reason;
}

async function fetch(input, init = undefined) {
  const request = new Request(input, init);
  const signal = request.signal;
  if (signal.aborted) throw abortReason(signal);
  const url = new URL(request.url);
  if (url.protocol === "data:") return dataResponse(url);
  if (url.protocol !== "http:" && url.protocol !== "https:") {
    throw new TypeError("fetch failed", { cause: new Error(`unknown scheme ${url.protocol}`) });
  }
  const headers = new Headers(request.headers);
  if (!headers.has("accept")) headers.set("accept", "*/*");
  if (!headers.has("user-agent")) headers.set("user-agent", "node");
  if (!headers.has("accept-encoding")) headers.set("accept-encoding", url.protocol === "https:" ? "br, gzip, deflate" : "gzip, deflate");
  const body = request.body ? internal.bytesToLatin1(await request.bytes()) : undefined;
  if (signal.aborted) throw abortReason(signal);
  return new Promise((resolve, reject) => {
    let controller = null;
    let id = null;
    let headReceived = false;
    const onabort = () => {
      if (id !== null) nativeFetch.abort(id);
      const reason = abortReason(signal);
      if (headReceived) controller.error(reason);
      else reject(reason);
    };
    const stream = new ReadableStream({
      start(streamController) {
        controller = streamController;
      },
      cancel() {
        nativeFetch.abort(id);
        signal.removeEventListener("abort", onabort);
      },
    }, { highWaterMark: 0 });
    const onEvent = (kind, value) => {
      if (kind === "head") {
        headReceived = true;
        resolve(networkResponse(value, request, stream));
      } else if (kind === "chunk") {
        if (controller.desiredSize !== null) controller.enqueue(value);
      } else if (kind === "end") {
        signal.removeEventListener("abort", onabort);
        if (controller.desiredSize !== null) controller.close();
      } else if (kind === "error") {
        signal.removeEventListener("abort", onabort);
        if (headReceived) controller.error(value);
        else reject(value);
      } else if (kind === "terminated") {
        signal.removeEventListener("abort", onabort);
        controller.error(new TypeError("terminated", { cause: value }));
      }
    };
    const nativeRequest = { url: request.url, method: request.method, headers: [...headers], redirect: request.redirect };
    id = nativeFetch.start(nativeRequest, body, onEvent);
    signal.addEventListener("abort", onabort, { once: true });
  });
}

module.exports = { fetch, Headers, Request, Response, FormData };
