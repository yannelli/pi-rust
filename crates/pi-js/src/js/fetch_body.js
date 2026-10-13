// internal/fetch_body: Headers, FormData, and the body handling shared by Request and Response.
const { Buffer, Blob, File } = require("buffer");
const { ReadableStream } = require("stream/web");
const streamState = require("internal/streams_web").kState;

const TOKEN = /^[!#$%&'*+\-.^_`|~0-9A-Za-z]+$/;

function normalizeValue(value) {
  return String(value).replace(/^[\t\n\r ]+|[\t\n\r ]+$/g, "");
}

class Headers {
  #list = [];
  constructor(init) {
    if (init === undefined || init === null) return;
    if (init instanceof Headers) {
      this.#list = init.#list.map((pair) => pair.slice());
    } else if (typeof init[Symbol.iterator] === "function") {
      for (const pair of init) {
        const items = Array.from(pair);
        if (items.length !== 2) throw new TypeError("Headers constructor: expected name/value pair to be length 2");
        this.append(items[0], items[1]);
      }
    } else if (typeof init === "object") {
      for (const key of Object.keys(init)) this.append(key, init[key]);
    } else {
      throw new TypeError("Headers constructor: init must be an object or iterable");
    }
  }
  #name(name) {
    const text = String(name);
    if (!TOKEN.test(text)) throw new TypeError(`Headers.append: "${text}" is an invalid header name.`);
    return text.toLowerCase();
  }
  append(name, value) {
    const text = normalizeValue(value);
    if (/[\0\r\n]/.test(text)) throw new TypeError(`Headers.append: "${text}" is an invalid header value.`);
    this.#list.push([this.#name(name), text]);
  }
  delete(name) {
    const key = this.#name(name);
    this.#list = this.#list.filter(([existing]) => existing !== key);
  }
  get(name) {
    const key = this.#name(name);
    const values = this.#list.filter(([existing]) => existing === key).map(([, value]) => value);
    return values.length ? values.join(", ") : null;
  }
  getSetCookie() {
    return this.#list.filter(([name]) => name === "set-cookie").map(([, value]) => value);
  }
  has(name) {
    const key = this.#name(name);
    return this.#list.some(([existing]) => existing === key);
  }
  set(name, value) {
    const key = this.#name(name);
    const text = normalizeValue(value);
    const index = this.#list.findIndex(([existing]) => existing === key);
    if (index < 0) {
      this.#list.push([key, text]);
      return;
    }
    this.#list[index][1] = text;
    this.#list = this.#list.filter(([existing], position) => existing !== key || position === index);
  }
  forEach(callback, thisArg) {
    for (const [name, value] of this) callback.call(thisArg, value, name, this);
  }
  // Sorted by name with repeated names combined, except set-cookie, as the spec iterates.
  *entries() {
    const names = [...new Set(this.#list.map(([name]) => name))].sort();
    for (const name of names) {
      if (name === "set-cookie") for (const value of this.getSetCookie()) yield [name, value];
      else yield [name, this.get(name)];
    }
  }
  *keys() {
    for (const [name] of this.entries()) yield name;
  }
  *values() {
    for (const [, value] of this.entries()) yield value;
  }
  [Symbol.iterator]() {
    return this.entries();
  }
  get [Symbol.toStringTag]() {
    return "Headers";
  }
}

class FormData {
  #entries = [];
  #value(value, filename) {
    if (value instanceof Blob) {
      if (filename !== undefined || !(value instanceof File)) return new File([value], filename === undefined ? "blob" : String(filename), { type: value.type });
      return value;
    }
    return String(value);
  }
  append(name, value, filename) {
    this.#entries.push([String(name), this.#value(value, filename)]);
  }
  set(name, value, filename) {
    const key = String(name);
    const entry = [key, this.#value(value, filename)];
    const index = this.#entries.findIndex(([existing]) => existing === key);
    this.#entries = this.#entries.filter(([existing]) => existing !== key);
    if (index < 0) this.#entries.push(entry);
    else this.#entries.splice(index, 0, entry);
  }
  get(name) {
    const entry = this.#entries.find(([existing]) => existing === String(name));
    return entry ? entry[1] : null;
  }
  getAll(name) {
    return this.#entries.filter(([existing]) => existing === String(name)).map(([, value]) => value);
  }
  has(name) {
    return this.#entries.some(([existing]) => existing === String(name));
  }
  delete(name) {
    this.#entries = this.#entries.filter(([existing]) => existing !== String(name));
  }
  forEach(callback, thisArg) {
    for (const [name, value] of this.#entries) callback.call(thisArg, value, name, this);
  }
  *entries() {
    yield* this.#entries.map((entry) => entry.slice());
  }
  *keys() {
    for (const [name] of this.#entries) yield name;
  }
  *values() {
    for (const [, value] of this.#entries) yield value;
  }
  [Symbol.iterator]() {
    return this.entries();
  }
  get [Symbol.toStringTag]() {
    return "FormData";
  }
}

function escapeField(text) {
  return text.replace(/\r/g, "%0D").replace(/\n/g, "%0A").replace(/"/g, "%22");
}

async function encodeMultipart(form, boundary) {
  const parts = [];
  for (const [name, value] of form) {
    let head = `--${boundary}\r\nContent-Disposition: form-data; name="${escapeField(name)}"`;
    if (typeof value === "string") {
      parts.push(Buffer.from(`${head}\r\n\r\n${value.replace(/\r(?!\n)|(?<!\r)\n/g, "\r\n")}\r\n`));
      continue;
    }
    head += `; filename="${escapeField(value.name)}"\r\nContent-Type: ${value.type || "application/octet-stream"}\r\n\r\n`;
    parts.push(Buffer.from(head), Buffer.from(await value.arrayBuffer()), Buffer.from("\r\n"));
  }
  parts.push(Buffer.from(`--${boundary}--\r\n`));
  return new Uint8Array(Buffer.concat(parts));
}

function streamOf(produce) {
  return new ReadableStream({
    async start(controller) {
      const bytes = await produce();
      if (bytes.length) controller.enqueue(bytes);
      controller.close();
    },
  });
}

function bytesSource(text, type) {
  return { source: { bytes: new TextEncoder().encode(text) }, type };
}

// A body as `{ source: { bytes } | { stream } | null, type }`, where `type` is the implied
// Content-Type. Blob and FormData bodies become streams because reading them is async.
function extractBody(body) {
  if (body === null || body === undefined) return { source: null, type: null };
  if (typeof body === "string") return bytesSource(body, "text/plain;charset=UTF-8");
  if (body instanceof URLSearchParams) return bytesSource(body.toString(), "application/x-www-form-urlencoded;charset=UTF-8");
  if (body instanceof ArrayBuffer) return { source: { bytes: new Uint8Array(body.slice(0)) }, type: null };
  if (ArrayBuffer.isView(body)) {
    return { source: { bytes: new Uint8Array(body.buffer.slice(body.byteOffset, body.byteOffset + body.byteLength)) }, type: null };
  }
  if (body instanceof Blob) return { source: { stream: body.stream() }, type: body.type || null };
  if (body instanceof FormData) {
    const random = Array.from(crypto.getRandomValues(new Uint8Array(8)), (byte) => byte.toString(16).padStart(2, "0")).join("");
    const boundary = `----formdata-pi-${random}`;
    return { source: { stream: streamOf(() => encodeMultipart(body, boundary)) }, type: `multipart/form-data; boundary=${boundary}` };
  }
  if (body instanceof ReadableStream) return { source: { stream: body }, type: null };
  if (typeof body[Symbol.asyncIterator] === "function") return { source: { stream: ReadableStream.from(body) }, type: null };
  return bytesSource(String(body), "text/plain;charset=UTF-8");
}

const kBody = Symbol("kBody");

// Body methods for Request and Response; `this[kBody]` holds `{ source, stream, used }`.
class BodyMixin {
  get body() {
    const body = this[kBody];
    if (!body.source) return null;
    if (!body.stream) {
      const bytes = body.source.bytes;
      body.stream = body.source.stream || new ReadableStream({
        start(controller) {
          if (bytes.length) controller.enqueue(bytes);
          controller.close();
        },
      });
    }
    return body.stream;
  }
  get bodyUsed() {
    const stream = this[kBody].stream;
    return this[kBody].used || (!!stream && stream[streamState].disturbed);
  }
  async bytes() {
    const body = this[kBody];
    if (this.bodyUsed) throw new TypeError("Body is unusable: Body has already been read");
    body.used = true;
    if (!body.source) return new Uint8Array(0);
    if (body.source.bytes && !body.stream) return body.source.bytes;
    const chunks = [];
    for await (const chunk of this.body) {
      chunks.push(typeof chunk === "string" ? Buffer.from(chunk) : Buffer.from(chunk.buffer, chunk.byteOffset, chunk.byteLength));
    }
    return new Uint8Array(Buffer.concat(chunks));
  }
  async arrayBuffer() {
    const bytes = await this.bytes();
    return bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + bytes.byteLength);
  }
  async text() {
    return new TextDecoder().decode(await this.bytes());
  }
  async json() {
    return JSON.parse(await this.text());
  }
  async blob() {
    return new Blob([await this.bytes()], { type: this.headers.get("content-type") || "" });
  }
  async formData() {
    const type = this.headers.get("content-type") || "";
    if (!type.startsWith("application/x-www-form-urlencoded")) throw internal.unsupported("Body.formData for multipart bodies");
    const form = new FormData();
    for (const [name, value] of new URLSearchParams(await this.text())) form.append(name, value);
    return form;
  }
}

module.exports = { Headers, FormData, BodyMixin, kBody, extractBody };
