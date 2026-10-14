// node:buffer: Buffer as a Uint8Array subclass, plus Blob, atob, and btoa.
const encoding = native.encoding;
const customInspect = Symbol.for("nodejs.util.inspect.custom");
const kMaxLength = 2 ** 32;
const ENCODINGS = ["utf8", "utf-8", "hex", "base64", "base64url", "ascii", "latin1", "binary", "ucs2", "ucs-2", "utf16le", "utf-16le"];

function invalidArg(name, expected, value) {
  const error = new TypeError(`The "${name}" argument must be ${expected}. Received ${value === null ? "null" : typeof value}`);
  error.code = "ERR_INVALID_ARG_TYPE";
  return error;
}

function outOfRange(name, range, value) {
  const error = new RangeError(`The value of "${name}" is out of range. It must be ${range}. Received ${value}`);
  error.code = "ERR_OUT_OF_RANGE";
  return error;
}

class FastBuffer extends Uint8Array {}

function Buffer(arg, encodingOrOffset, length) {
  if (typeof arg === "number") {
    if (typeof encodingOrOffset === "string") throw invalidArg("string", "of type string", arg);
    return Buffer.alloc(arg);
  }
  return Buffer.from(arg, encodingOrOffset, length);
}
Buffer.prototype = FastBuffer.prototype;
Object.defineProperty(FastBuffer.prototype, "constructor", { value: Buffer, writable: true, configurable: true });
Object.setPrototypeOf(Buffer, Uint8Array);
Object.defineProperty(Buffer, Symbol.species, { get: () => FastBuffer, configurable: true });
Buffer.poolSize = 8192;

function wrap(view) {
  return new FastBuffer(view.buffer, view.byteOffset, view.byteLength);
}

function normalizeEncoding(value) {
  if (value === undefined || value === null || value === "") return "utf8";
  const lower = String(value).toLowerCase();
  if (!ENCODINGS.includes(lower)) {
    const error = new TypeError(`Unknown encoding: ${value}`);
    error.code = "ERR_UNKNOWN_ENCODING";
    throw error;
  }
  return lower;
}

function fromString(string, enc) {
  return wrap(encoding.encode(string, normalizeEncoding(enc)));
}

Buffer.from = function from(value, encodingOrOffset, length) {
  if (typeof value === "string") return fromString(value, encodingOrOffset);
  if (value instanceof ArrayBuffer || (typeof SharedArrayBuffer !== "undefined" && value instanceof SharedArrayBuffer)) {
    const offset = encodingOrOffset === undefined ? 0 : Number(encodingOrOffset);
    const size = length === undefined ? value.byteLength - offset : Number(length);
    if (offset > value.byteLength) throw outOfRange("offset", `<= ${value.byteLength}`, offset);
    return new FastBuffer(value, offset, size);
  }
  if (value instanceof Uint8Array) {
    const copy = new FastBuffer(value.length);
    copy.set(value);
    return copy;
  }
  if (value === null || value === undefined) throw invalidArg("first", "of type string or an instance of Buffer, ArrayBuffer, or Array", value);
  if (typeof value[Symbol.toPrimitive] === "function") {
    const primitive = value[Symbol.toPrimitive]("string");
    if (typeof primitive === "string") return fromString(primitive, encodingOrOffset);
  }
  const valueOf = value.valueOf && value.valueOf();
  if (valueOf !== undefined && valueOf !== value && (typeof valueOf === "string" || typeof valueOf === "object")) {
    return Buffer.from(valueOf, encodingOrOffset, length);
  }
  if (value.type === "Buffer" && Array.isArray(value.data)) return fromArrayLike(value.data);
  if (typeof value.length === "number" || Array.isArray(value)) return fromArrayLike(value);
  throw invalidArg("first", "of type string or an instance of Buffer, ArrayBuffer, or Array", value);
};

function fromArrayLike(list) {
  const buffer = new FastBuffer(list.length >>> 0);
  for (let i = 0; i < buffer.length; i++) buffer[i] = list[i] & 255;
  return buffer;
}

Buffer.of = (...items) => fromArrayLike(items);
Buffer.alloc = function alloc(size, fill, enc) {
  checkSize(size);
  const buffer = new FastBuffer(size);
  if (fill !== undefined && fill !== 0 && size > 0) buffer.fill(fill, enc);
  return buffer;
};
Buffer.allocUnsafe = function allocUnsafe(size) {
  checkSize(size);
  return new FastBuffer(size);
};
Buffer.allocUnsafeSlow = Buffer.allocUnsafe;
Buffer.isBuffer = (value) => value instanceof Buffer;
Buffer.isEncoding = (value) => typeof value === "string" && value !== "" && ENCODINGS.includes(value.toLowerCase());
Buffer.byteLength = function byteLength(value, enc) {
  if (typeof value !== "string") {
    if (ArrayBuffer.isView(value) || value instanceof ArrayBuffer) return value.byteLength;
    throw invalidArg("string", "of type string or an instance of Buffer or ArrayBuffer", value);
  }
  return encoding.encode(value, normalizeEncoding(enc)).length;
};
Buffer.compare = function compare(a, b) {
  if (!(a instanceof Uint8Array)) throw invalidArg("buf1", "an instance of Buffer or Uint8Array", a);
  if (!(b instanceof Uint8Array)) throw invalidArg("buf2", "an instance of Buffer or Uint8Array", b);
  const length = Math.min(a.length, b.length);
  for (let i = 0; i < length; i++) if (a[i] !== b[i]) return a[i] < b[i] ? -1 : 1;
  return a.length === b.length ? 0 : a.length < b.length ? -1 : 1;
};
Buffer.concat = function concat(list, totalLength) {
  if (!Array.isArray(list)) throw invalidArg("list", "an instance of Array", list);
  if (totalLength === undefined) totalLength = list.reduce((sum, item) => sum + item.length, 0);
  const result = Buffer.alloc(totalLength);
  let offset = 0;
  for (const item of list) {
    if (!(item instanceof Uint8Array)) throw invalidArg("list[i]", "an instance of Buffer or Uint8Array", item);
    const remaining = totalLength - offset;
    if (remaining <= 0) break;
    result.set(item.length > remaining ? item.subarray(0, remaining) : item, offset);
    offset += Math.min(item.length, remaining);
  }
  return result;
};
Buffer.copyBytesFrom = (view, offset = 0, length) => {
  const bytes = new Uint8Array(view.buffer, view.byteOffset, view.byteLength);
  const step = view.BYTES_PER_ELEMENT || 1;
  return Buffer.from(bytes.slice(offset * step, length === undefined ? undefined : (offset + length) * step));
};

function checkSize(size) {
  if (typeof size !== "number") throw invalidArg("size", "of type number", size);
  if (!(size >= 0 && size <= kMaxLength)) throw outOfRange("size", `>= 0 && <= ${kMaxLength}`, size);
}

const proto = Buffer.prototype;
proto.toString = function toString(enc, start, end) {
  if (arguments.length === 0) return encoding.decode(internal.bytesToLatin1(this), "utf8");
  start = start === undefined || start < 0 ? 0 : Math.min(start | 0, this.length);
  end = end === undefined || end > this.length ? this.length : end | 0;
  if (end <= start) return "";
  return encoding.decode(internal.bytesToLatin1(this.subarray(start, end)), normalizeEncoding(enc));
};
proto.toLocaleString = proto.toString;
proto.toJSON = function toJSON() {
  return { type: "Buffer", data: Array.from(this) };
};
proto.equals = function equals(other) {
  if (!(other instanceof Uint8Array)) throw invalidArg("otherBuffer", "an instance of Buffer or Uint8Array", other);
  return Buffer.compare(this, other) === 0;
};
proto.compare = function compare(target, targetStart = 0, targetEnd = target.length, sourceStart = 0, sourceEnd = this.length) {
  return Buffer.compare(this.subarray(sourceStart, sourceEnd), target.subarray(targetStart, targetEnd));
};
proto.slice = function slice(start, end) {
  return this.subarray(start, end);
};
proto.subarray = function subarray(start, end) {
  return wrap(Uint8Array.prototype.subarray.call(this, start, end));
};
proto.copy = function copy(target, targetStart = 0, sourceStart = 0, sourceEnd = this.length) {
  if (sourceEnd > this.length) sourceEnd = this.length;
  if (targetStart >= target.length || sourceStart >= sourceEnd) return 0;
  const chunk = this.subarray(sourceStart, Math.min(sourceEnd, sourceStart + target.length - targetStart));
  Uint8Array.prototype.set.call(target, chunk, targetStart);
  return chunk.length;
};
proto.write = function write(string, offset, length, enc) {
  if (typeof offset === "string") [enc, offset, length] = [offset, 0, undefined];
  else if (typeof length === "string") [enc, length] = [length, undefined];
  offset = offset === undefined ? 0 : offset >>> 0;
  const name = normalizeEncoding(enc);
  const bytes = encoding.encode(String(string), name);
  const room = Math.min(this.length - offset, length === undefined ? Infinity : length);
  let count = Math.min(bytes.length, room);
  // Do not split a UTF-8 sequence: back up to the start of the cut character.
  if ((name === "utf8" || name === "utf-8") && count < bytes.length) {
    while (count > 0 && (bytes[count] & 0xc0) === 0x80) count--;
  }
  this.set(bytes.subarray(0, count), offset);
  return count;
};
proto.fill = function fill(value, offset = 0, end = this.length, enc) {
  if (typeof offset === "string") [enc, offset, end] = [offset, 0, this.length];
  else if (typeof end === "string") [enc, end] = [end, this.length];
  if (typeof value === "number" || typeof value === "boolean") {
    Uint8Array.prototype.fill.call(this, Number(value) & 255, offset, end);
    return this;
  }
  const pattern = typeof value === "string" ? encoding.encode(value, normalizeEncoding(enc)) : value;
  if (pattern.length === 0) {
    if (typeof value === "string" && value.length > 0) throw Object.assign(new TypeError("The argument 'value' is invalid."), { code: "ERR_INVALID_ARG_VALUE" });
    Uint8Array.prototype.fill.call(this, 0, offset, end);
    return this;
  }
  for (let i = offset, j = 0; i < end; i++, j = (j + 1) % pattern.length) this[i] = pattern[j];
  return this;
};

function needle(value, enc) {
  if (typeof value === "number") return Uint8Array.of(value & 255);
  if (typeof value === "string") return encoding.encode(value, normalizeEncoding(enc));
  if (value instanceof Uint8Array) return value;
  throw invalidArg("value", "one of type number or string or an instance of Buffer or Uint8Array", value);
}

function search(buffer, value, byteOffset, enc, last) {
  if (typeof byteOffset === "string") [enc, byteOffset] = [byteOffset, undefined];
  const target = needle(value, enc);
  const length = buffer.length;
  let start = byteOffset === undefined ? (last ? length : 0) : Number(byteOffset) || 0;
  if (start < 0) start = Math.max(length + start, last ? -1 : 0);
  if (target.length === 0) return Math.min(start, length);
  const matches = (at) => {
    for (let j = 0; j < target.length; j++) if (buffer[at + j] !== target[j]) return false;
    return true;
  };
  if (last) {
    for (let i = Math.min(start, length - target.length); i >= 0; i--) if (matches(i)) return i;
  } else {
    for (let i = start; i <= length - target.length; i++) if (matches(i)) return i;
  }
  return -1;
}
proto.indexOf = function indexOf(value, byteOffset, enc) {
  return search(this, value, byteOffset, enc, false);
};
proto.lastIndexOf = function lastIndexOf(value, byteOffset, enc) {
  return search(this, value, byteOffset, enc, true);
};
proto.includes = function includes(value, byteOffset, enc) {
  return this.indexOf(value, byteOffset, enc) !== -1;
};

function swap(size) {
  return function swapBytes() {
    if (this.length % size !== 0) throw Object.assign(new RangeError(`Buffer size must be a multiple of ${size * 8}-bits`), { code: "ERR_INVALID_BUFFER_SIZE" });
    for (let i = 0; i < this.length; i += size) {
      for (let a = i, b = i + size - 1; a < b; a++, b--) [this[a], this[b]] = [this[b], this[a]];
    }
    return this;
  };
}
proto.swap16 = swap(2);
proto.swap32 = swap(4);
proto.swap64 = swap(8);

// Fixed-width numeric accessors through a DataView.
function view(buffer) {
  return new DataView(buffer.buffer, buffer.byteOffset, buffer.byteLength);
}
function checkOffset(buffer, offset, size) {
  if (offset === undefined) offset = 0;
  if (typeof offset !== "number") throw invalidArg("offset", "of type number", offset);
  if (!Number.isInteger(offset) || offset < 0 || offset + size > buffer.length) {
    throw outOfRange("offset", `>= 0 and <= ${buffer.length - size}`, offset);
  }
  return offset;
}
const ACCESSORS = [
  ["Int8", 1, "Int8"], ["UInt8", 1, "Uint8"], ["Int16", 2, "Int16"], ["UInt16", 2, "Uint16"],
  ["Int32", 4, "Int32"], ["UInt32", 4, "Uint32"], ["Float", 4, "Float32"], ["Double", 8, "Float64"],
  ["BigInt64", 8, "BigInt64"], ["BigUInt64", 8, "BigUint64"],
];
for (const [name, size, method] of ACCESSORS) {
  const variants = size === 1 ? [["", true]] : [["LE", true], ["BE", false]];
  for (const [suffix, little] of variants) {
    const read = function (offset) {
      offset = checkOffset(this, offset, size);
      return view(this)[`get${method}`](offset, little);
    };
    const write = function (value, offset) {
      offset = checkOffset(this, offset, size);
      view(this)[`set${method}`](offset, method.startsWith("Big") ? BigInt(value) : Number(value), little);
      return offset + size;
    };
    for (const alias of name.includes("UInt") ? [name, name.replace("UInt", "Uint")] : [name]) {
      proto[`read${alias}${suffix}`] = read;
      proto[`write${alias}${suffix}`] = write;
    }
  }
}
function variableRead(signed, little) {
  return function (offset, byteLength) {
    checkOffset(this, offset, byteLength);
    let value = 0;
    for (let i = 0; i < byteLength; i++) value += this[offset + (little ? i : byteLength - 1 - i)] * 2 ** (8 * i);
    if (signed && value >= 2 ** (8 * byteLength - 1)) value -= 2 ** (8 * byteLength);
    return value;
  };
}
function variableWrite(little) {
  return function (value, offset, byteLength) {
    checkOffset(this, offset, byteLength);
    let rest = value < 0 ? value + 2 ** (8 * byteLength) : value;
    for (let i = 0; i < byteLength; i++) {
      this[offset + (little ? i : byteLength - 1 - i)] = rest % 256;
      rest = Math.floor(rest / 256);
    }
    return offset + byteLength;
  };
}
for (const [suffix, little] of [["LE", true], ["BE", false]]) {
  proto[`readUInt${suffix}`] = variableRead(false, little);
  proto[`readUint${suffix}`] = proto[`readUInt${suffix}`];
  proto[`readInt${suffix}`] = variableRead(true, little);
  proto[`writeUInt${suffix}`] = variableWrite(little);
  proto[`writeUint${suffix}`] = proto[`writeUInt${suffix}`];
  proto[`writeInt${suffix}`] = variableWrite(little);
}

proto[customInspect] = function inspectBuffer() {
  const max = 50;
  const bytes = Array.from(this.subarray(0, max), (byte) => byte.toString(16).padStart(2, "0"));
  const more = this.length > max ? ` ... ${this.length - max} more byte${this.length - max > 1 ? "s" : ""}` : "";
  return `<${this.constructor.name} ${bytes.join(" ")}${more}>`;
};

class Blob {
  #bytes;
  #type;
  constructor(parts = [], options = {}) {
    const chunks = [];
    for (const part of parts) {
      if (part instanceof Blob) chunks.push(part.#bytes);
      else if (typeof part === "string") chunks.push(encoding.encode(part, "utf8"));
      else if (ArrayBuffer.isView(part)) chunks.push(new Uint8Array(part.buffer, part.byteOffset, part.byteLength));
      else if (part instanceof ArrayBuffer) chunks.push(new Uint8Array(part));
      else chunks.push(encoding.encode(String(part), "utf8"));
    }
    this.#bytes = new Uint8Array(Buffer.concat(chunks));
    this.#type = options.type ? String(options.type).toLowerCase() : "";
  }
  get size() {
    return this.#bytes.length;
  }
  get type() {
    return this.#type;
  }
  slice(start, end, type) {
    return new Blob([this.#bytes.slice(start, end)], { type });
  }
  async arrayBuffer() {
    return this.#bytes.slice().buffer;
  }
  async bytes() {
    return this.#bytes.slice();
  }
  async text() {
    return encoding.decode(internal.bytesToLatin1(this.#bytes), "utf8");
  }
  stream() {
    const bytes = this.#bytes.slice();
    return new ReadableStream({
      start(controller) {
        if (bytes.length) controller.enqueue(bytes);
        controller.close();
      },
    });
  }
  get [Symbol.toStringTag]() {
    return "Blob";
  }
}

class File extends Blob {
  #name;
  #lastModified;
  constructor(parts, name, options = {}) {
    super(parts, options);
    this.#name = String(name);
    this.#lastModified = options.lastModified === undefined ? Date.now() : Number(options.lastModified);
  }
  get name() {
    return this.#name;
  }
  get lastModified() {
    return this.#lastModified;
  }
  get [Symbol.toStringTag]() {
    return "File";
  }
}

function btoa(input) {
  input = String(input);
  for (let i = 0; i < input.length; i++) {
    if (input.charCodeAt(i) > 255) throw new (globalThis.DOMException || Error)("Invalid character", "InvalidCharacterError");
  }
  return encoding.decode(input, "base64");
}

function atob(input) {
  const text = String(input).replace(/[\t\n\f\r ]/g, "");
  if (text.length % 4 === 1 || /[^A-Za-z0-9+/=]/.test(text) || /=[^=]/.test(text)) {
    throw new (globalThis.DOMException || Error)("The string to be decoded is not correctly encoded.", "InvalidCharacterError");
  }
  return internal.bytesToLatin1(encoding.encode(text, "base64"));
}

const constants = { MAX_LENGTH: kMaxLength, MAX_STRING_LENGTH: 2 ** 29 - 24 };
module.exports = {
  Buffer,
  SlowBuffer: Buffer.allocUnsafeSlow,
  Blob,
  File,
  atob,
  btoa,
  constants,
  kMaxLength,
  kStringMaxLength: constants.MAX_STRING_LENGTH,
  INSPECT_MAX_BYTES: 50,
  isUtf8: (value) => encoding.isUtf8(internal.bytesToLatin1(value)),
  isAscii: (value) => Array.prototype.every.call(new Uint8Array(value.buffer || value, value.byteOffset, value.byteLength), (byte) => byte < 128),
  transcode,
  resolveObjectURL,
};

// ICU's converters as Node uses them: characters the target cannot hold become "?".
const TRANSCODE_NAMES = { utf8: "utf8", "utf-8": "utf8", ucs2: "utf16le", "ucs-2": "utf16le", utf16le: "utf16le", "utf-16le": "utf16le", latin1: "latin1", binary: "latin1", ascii: "ascii" };
function transcode(source, fromEncoding, toEncoding) {
  if (!(source instanceof Uint8Array)) {
    throw require("internal/errors").invalidArgType("source", "an instance of Buffer or Uint8Array", source);
  }
  if (source.length === 0) return Buffer.alloc(0);
  const from = TRANSCODE_NAMES[String(fromEncoding).toLowerCase()];
  const to = TRANSCODE_NAMES[String(toEncoding).toLowerCase()];
  if (!from || !to) {
    throw Object.assign(new Error("Unable to transcode Buffer [U_ILLEGAL_ARGUMENT_ERROR]"), { code: "U_ILLEGAL_ARGUMENT_ERROR", errno: 1 });
  }
  let text = Buffer.from(source.buffer, source.byteOffset, source.byteLength).toString(from === "ascii" ? "latin1" : from);
  if (from === "ascii") text = text.replace(/[\u0080-\u00ff]/g, "\ufffd");
  if (to === "ascii") text = text.replace(/[^\x00-\x7f]/gu, "?");
  else if (to === "latin1") text = text.replace(/[^\x00-\xff]/gu, "?");
  return Buffer.from(text, to);
}

// Blob URLs: blob:nodedata:<uuid>, registered by URL.createObjectURL.
const blobUrls = new Map();
internal.createObjectURL = function createObjectURL(blob) {
  if (!(blob instanceof Blob)) throw require("internal/errors").invalidArgType("obj", "an instance of Blob", blob);
  const id = require("crypto").randomUUID();
  blobUrls.set(id, blob);
  return `blob:nodedata:${id}`;
};
function blobId(target) {
  let parsed;
  try {
    parsed = new URL(`${target}`);
  } catch {
    return undefined;
  }
  if (parsed.protocol !== "blob:" || !parsed.pathname.startsWith("nodedata:")) return undefined;
  return parsed.pathname.slice("nodedata:".length);
}
internal.revokeObjectURL = function revokeObjectURL(target) {
  if (arguments.length === 0) throw require("internal/errors").missingArgs("url");
  const id = blobId(target);
  if (id !== undefined) blobUrls.delete(id);
};
function resolveObjectURL(target) {
  const id = blobId(target);
  return id === undefined ? undefined : blobUrls.get(id);
}
