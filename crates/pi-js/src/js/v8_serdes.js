// internal/v8_serdes: V8's ValueSerializer wire format (version 15), in JavaScript.
const { Buffer } = require("buffer");
const { invalidArgType } = require("internal/errors");

const VERSION = 15;
const VIEW_TAGS = [
  ["b", Int8Array], ["B", Uint8Array], ["C", Uint8ClampedArray], ["w", Int16Array], ["W", Uint16Array], ["d", Int32Array],
  ["D", Uint32Array], ["f", Float32Array], ["F", Float64Array], ["q", BigInt64Array], ["Q", BigUint64Array], ["?", DataView],
];
const ERROR_TAGS = { EvalError: "E", RangeError: "R", ReferenceError: "F", SyntaxError: "S", TypeError: "T", URIError: "U" };
const ERROR_CTORS = { E: EvalError, R: RangeError, F: ReferenceError, S: SyntaxError, T: TypeError, U: URIError };
const REGEXP_FLAGS = [["g", 1], ["i", 2], ["m", 4], ["y", 8], ["u", 16], ["s", 32], ["d", 64], ["v", 256]];
const toTag = Object.prototype.toString;
const scratch = new DataView(new ArrayBuffer(8));
const isIndex = (key) => /^(0|[1-9]\d*)$/.test(key) && Number(key) < 4294967295;

function tagOf(value) {
  return toTag.call(value).slice(8, -1);
}

class Serializer {
  constructor() {
    this._bytes = new Uint8Array(64);
    this._length = 0;
    this._ids = new Map();
    this._transfers = new Map();
    this._hostViews = false;
  }
  _reserve(count) {
    if (this._length + count <= this._bytes.length) return;
    const next = new Uint8Array(Math.max(this._bytes.length * 2, this._length + count));
    next.set(this._bytes.subarray(0, this._length));
    this._bytes = next;
  }
  _byte(byte) {
    this._reserve(1);
    this._bytes[this._length++] = byte;
  }
  _tag(char) {
    this._byte(char.charCodeAt(0));
  }
  _varint(value) {
    let n = typeof value === "bigint" ? value : BigInt(Math.floor(value));
    do {
      let byte = Number(n & 0x7fn);
      n >>= 7n;
      if (n > 0n) byte |= 0x80;
      this._byte(byte);
    } while (n > 0n);
  }
  writeHeader() {
    this._byte(0xff);
    this._varint(VERSION);
  }
  writeUint32(value) {
    this._varint(value >>> 0);
  }
  writeUint64(hi, lo) {
    this._varint((BigInt(hi >>> 0) << 32n) | BigInt(lo >>> 0));
  }
  writeDouble(value) {
    scratch.setFloat64(0, Number(value), true);
    this.writeRawBytes(new Uint8Array(scratch.buffer));
  }
  writeRawBytes(source) {
    if (!ArrayBuffer.isView(source)) throw invalidArgType("source", "an instance of Buffer, TypedArray, or DataView", source);
    const bytes = new Uint8Array(source.buffer, source.byteOffset, source.byteLength);
    this._reserve(bytes.length);
    this._bytes.set(bytes, this._length);
    this._length += bytes.length;
  }
  releaseBuffer() {
    const out = Buffer.from(this._bytes.subarray(0, this._length));
    this._length = 0;
    this._ids = new Map();
    return out;
  }
  transferArrayBuffer(id, arrayBuffer) {
    this._transfers.set(arrayBuffer, id);
  }
  _setTreatArrayBufferViewsAsHostObjects(flag) {
    this._hostViews = Boolean(flag);
  }
  _cloneError(text) {
    const Ctor = this._getDataCloneError || Error;
    return new Ctor(`${text} could not be cloned.`);
  }
  _writeString(text) {
    let oneByte = true;
    for (let i = 0; i < text.length && oneByte; i++) oneByte = text.charCodeAt(i) < 256;
    if (oneByte) {
      this._tag('"');
      this._varint(text.length);
      this._reserve(text.length);
      for (let i = 0; i < text.length; i++) this._bytes[this._length++] = text.charCodeAt(i);
      return;
    }
    const byteLength = text.length * 2;
    let varintSize = 1;
    for (let n = byteLength >> 7; n > 0; n >>= 7) varintSize++;
    if ((this._length + 1 + varintSize) & 1) this._byte(0);
    this._tag("c");
    this._varint(byteLength);
    for (let i = 0; i < text.length; i++) {
      const code = text.charCodeAt(i);
      this._byte(code & 0xff);
      this._byte(code >> 8);
    }
  }
  _writeNumber(value) {
    if (Number.isInteger(value) && value >= -2147483648 && value <= 2147483647 && !Object.is(value, -0)) {
      this._tag("I");
      this._varint(value < 0 ? -2 * value - 1 : 2 * value);
    } else {
      this._tag("N");
      this.writeDouble(value);
    }
  }
  _writeBigInt(value) {
    const negative = value < 0n;
    let magnitude = negative ? -value : value;
    const words = [];
    while (magnitude > 0n) {
      words.push(magnitude & 0xffffffffffffffffn);
      magnitude >>= 64n;
    }
    this._varint(BigInt(words.length * 8 * 2 + (negative ? 1 : 0)));
    for (const word of words) for (let i = 0n; i < 64n; i += 8n) this._byte(Number((word >> i) & 0xffn));
  }
  writeValue(value) {
    switch (typeof value) {
      case "undefined": return this._tag("_"), true;
      case "boolean": return this._tag(value ? "T" : "F"), true;
      case "number": return this._writeNumber(value), true;
      case "bigint": return this._tag("Z"), this._writeBigInt(value), true;
      case "string": return this._writeString(value), true;
      case "symbol": throw this._cloneError(String(value));
      case "function": throw this._cloneError(Function.prototype.toString.call(value));
    }
    if (value === null) return this._tag("0"), true;
    const id = this._ids.get(value);
    if (id !== undefined) {
      this._tag("^");
      this._varint(id);
      return true;
    }
    this._writeObject(value, tagOf(value));
    return true;
  }
  _assign(value) {
    this._ids.set(value, this._ids.size);
  }
  _writeObject(value, kind) {
    if (ArrayBuffer.isView(value)) {
      if (this._hostViews) {
        this._assign(value);
        this._tag("\\");
        return this._writeHostObject(value);
      }
      if (!this._ids.has(value.buffer)) this._writeObject(value.buffer, "ArrayBuffer");
      this._assign(value);
      const entry = VIEW_TAGS.find(([, Ctor]) => value instanceof Ctor);
      if (!entry) throw this._cloneError(`#<${kind}>`);
      this._tag("V");
      this._tag(entry[0]);
      this._varint(value.byteOffset);
      this._varint(value.byteLength);
      this._varint(0);
      return;
    }
    if (kind === "Function" || kind === "AsyncFunction" || kind === "GeneratorFunction") throw this._cloneError(String(value));
    this._assign(value);
    switch (kind) {
      case "Array": return this._writeArray(value);
      case "Date": return this._tag("D"), this.writeDouble(value.getTime());
      case "Boolean": return this._tag(value.valueOf() ? "y" : "x");
      case "Number": return this._tag("n"), this.writeDouble(value.valueOf());
      case "BigInt": return this._tag("z"), this._writeBigInt(value.valueOf());
      case "String": return this._tag("s"), this._writeString(value.valueOf());
      case "RegExp": {
        this._tag("R");
        this._writeString(value.source);
        return this._varint(REGEXP_FLAGS.reduce((bits, [flag, bit]) => (value.flags.includes(flag) ? bits | bit : bits), 0));
      }
      case "Map": {
        this._tag(";");
        for (const [key, entry] of value) this.writeValue(key), this.writeValue(entry);
        return this._tag(":"), this._varint(value.size * 2);
      }
      case "Set": {
        this._tag("'");
        for (const entry of value) this.writeValue(entry);
        return this._tag(","), this._varint(value.size);
      }
      case "ArrayBuffer": {
        const transfer = this._transfers.get(value);
        if (transfer !== undefined) return this._tag("t"), this._varint(transfer);
        this._tag("B");
        this._varint(value.byteLength);
        return this.writeRawBytes(new Uint8Array(value));
      }
      case "SharedArrayBuffer": {
        if (typeof this._getSharedArrayBufferId !== "function") throw this._cloneError(`#<${kind}>`);
        return this._tag("u"), this._varint(this._getSharedArrayBufferId(value));
      }
      case "Error": return this._writeError(value);
      case "Object": return this._writeProperties(value, Object.keys(value), "o", "{");
    }
    throw this._cloneError(`#<${kind}>`);
  }
  _writeProperties(value, keys, open, close, extra) {
    if (open) this._tag(open);
    for (const key of keys) {
      if (isIndex(key)) this._writeNumber(Number(key));
      else this._writeString(key);
      this.writeValue(value[key]);
    }
    this._tag(close);
    this._varint(keys.length);
    if (extra !== undefined) this._varint(extra);
  }
  _writeArray(value) {
    const keys = Object.keys(value);
    const indices = keys.filter(isIndex);
    if (indices.length === value.length) {
      this._tag("A");
      this._varint(value.length);
      for (let i = 0; i < value.length; i++) this.writeValue(value[i]);
      return this._writeProperties(value, keys.filter((key) => !isIndex(key)), "", "$", value.length);
    }
    this._tag("a");
    this._varint(value.length);
    this._writeProperties(value, keys, "", "@", value.length);
  }
  _writeError(value) {
    this._tag("r");
    const tag = ERROR_TAGS[value.name];
    if (tag) this._tag(tag);
    const message = Object.getOwnPropertyDescriptor(value, "message");
    if (message && "value" in message) this._tag("m"), this._writeString(String(message.value));
    if (typeof value.stack === "string") this._tag("s"), this._writeString(value.stack);
    if (Object.prototype.hasOwnProperty.call(value, "cause")) this._tag("c"), this.writeValue(value.cause);
    this._tag(".");
  }
}

module.exports = { Serializer, VIEW_TAGS, ERROR_CTORS, REGEXP_FLAGS, VERSION, scratch };
