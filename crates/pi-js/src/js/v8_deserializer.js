// internal/v8_deserializer: reads V8's ValueSerializer wire format (versions up to 15).
const { Buffer } = require("buffer");
const { invalidArgType } = require("internal/errors");
const { VIEW_TAGS, ERROR_CTORS, REGEXP_FLAGS, VERSION, scratch } = require("internal/v8_serdes");

function invalid() {
  return new Error("Unable to deserialize cloned data.");
}

class Deserializer {
  constructor(buffer) {
    if (!ArrayBuffer.isView(buffer)) throw invalidArgType("buffer", "an instance of Buffer, TypedArray, or DataView", buffer);
    this.buffer = buffer;
    this._bytes = new Uint8Array(buffer.buffer, buffer.byteOffset, buffer.byteLength);
    this._offset = 0;
    this._version = 0;
    this._objects = [];
    this._transfers = new Map();
  }
  _byte() {
    if (this._offset >= this._bytes.length) throw invalid();
    return this._bytes[this._offset++];
  }
  _peek() {
    return this._bytes[this._offset];
  }
  _bigVarint() {
    let result = 0n;
    let shift = 0n;
    for (;;) {
      const byte = this._byte();
      result |= BigInt(byte & 0x7f) << shift;
      shift += 7n;
      if (byte < 0x80) return result;
    }
  }
  _varint() {
    return Number(this._bigVarint());
  }
  readHeader() {
    if (this._peek() === 0xff) {
      this._offset++;
      this._version = this._varint();
      if (this._version > VERSION) {
        throw new Error("Unable to deserialize cloned data due to invalid or unsupported version.");
      }
    }
    return true;
  }
  getWireFormatVersion() {
    return this._version;
  }
  readUint32() {
    return this._varint() >>> 0;
  }
  readUint64() {
    const value = this._bigVarint();
    return [Number((value >> 32n) & 0xffffffffn), Number(value & 0xffffffffn)];
  }
  readDouble() {
    const bytes = this._raw(8);
    for (let i = 0; i < 8; i++) scratch.setUint8(i, bytes[i]);
    return scratch.getFloat64(0, true);
  }
  _readRawBytes(length) {
    if (this._offset + length > this._bytes.length) throw invalid();
    const offset = this._offset;
    this._offset += length;
    return offset;
  }
  _raw(length) {
    const offset = this._readRawBytes(length);
    return this._bytes.subarray(offset, offset + length);
  }
  readRawBytes(length) {
    const offset = this._readRawBytes(length);
    return Buffer.from(this.buffer.buffer, this.buffer.byteOffset + offset, length);
  }
  transferArrayBuffer(id, arrayBuffer) {
    this._transfers.set(id, arrayBuffer);
  }
  _keep(value) {
    this._objects.push(value);
    return value;
  }
  _string(char) {
    const length = this._varint();
    const bytes = this._raw(length);
    if (char === "S") return new TextDecoder().decode(bytes);
    let text = "";
    if (char === "c") {
      for (let i = 0; i + 1 < bytes.length; i += 2) text += String.fromCharCode(bytes[i] | (bytes[i + 1] << 8));
    } else {
      for (let i = 0; i < bytes.length; i += 8192) text += String.fromCharCode.apply(null, bytes.subarray(i, i + 8192));
    }
    return text;
  }
  _bigint() {
    const bitfield = this._varint();
    const bytes = this._raw(bitfield >> 1);
    let value = 0n;
    for (let i = bytes.length - 1; i >= 0; i--) value = (value << 8n) | BigInt(bytes[i]);
    return bitfield & 1 ? -value : value;
  }
  _properties(target, end) {
    let count = 0;
    for (;;) {
      while (this._peek() === 0) this._offset++;
      if (this._peek() === end.charCodeAt(0)) {
        this._offset++;
        return count;
      }
      const key = this.readValue();
      target[key] = this.readValue();
      count++;
    }
  }
  readValue() {
    const char = String.fromCharCode(this._byte());
    switch (char) {
      case "\0": return this.readValue();
      case "?": return this._varint(), this.readValue();
      case "_": return undefined;
      case "0": return null;
      case "T": return true;
      case "F": return false;
      case "I": {
        const n = this._varint();
        return n % 2 ? -(n + 1) / 2 : n / 2;
      }
      case "U": return this._varint();
      case "N": return this.readDouble();
      case "Z": return this._bigint();
      case '"': case "S": case "c": return this._string(char);
      case "^": {
        const id = this._varint();
        if (id >= this._objects.length) throw invalid();
        return this._objects[id];
      }
      case "o": {
        const object = this._keep({});
        this._properties(object, "{");
        this._varint();
        return object;
      }
      case "A": {
        const array = this._keep(new Array(this._varint()));
        for (let i = 0; i < array.length; i++) {
          if (this._peek() === 0x2d) this._offset++;
          else array[i] = this.readValue();
        }
        this._properties(array, "$");
        this._varint();
        this._varint();
        return array;
      }
      case "a": {
        const array = this._keep(new Array(this._varint()));
        this._properties(array, "@");
        this._varint();
        this._varint();
        return array;
      }
      case "D": return this._keep(new Date(this.readDouble()));
      case "y": return this._keep(Object(true));
      case "x": return this._keep(Object(false));
      case "n": return this._keep(Object(this.readDouble()));
      case "z": return this._keep(Object(this._bigint()));
      case "s": {
        const index = this._objects.push(undefined) - 1;
        return (this._objects[index] = Object(this.readValue()));
      }
      case "R": {
        const index = this._objects.push(undefined) - 1;
        const source = this.readValue();
        const bits = this._varint();
        const flags = REGEXP_FLAGS.filter(([, bit]) => bits & bit).map(([flag]) => flag).join("");
        return (this._objects[index] = new RegExp(source, flags));
      }
      case ";": {
        const map = this._keep(new Map());
        while (this._peek() !== 0x3a) map.set(this.readValue(), this.readValue());
        this._offset++;
        this._varint();
        return map;
      }
      case "'": {
        const set = this._keep(new Set());
        while (this._peek() !== 0x2c) set.add(this.readValue());
        this._offset++;
        this._varint();
        return set;
      }
      case "B": {
        const length = this._varint();
        const buffer = this._keep(new Uint8Array(this._raw(length)).buffer);
        return this._maybeView(buffer);
      }
      case "~": {
        const length = this._varint();
        this._varint();
        return this._maybeView(this._keep(new Uint8Array(this._raw(length)).buffer));
      }
      case "t": {
        const buffer = this._transfers.get(this._varint());
        if (buffer === undefined) throw invalid();
        return this._maybeView(this._keep(buffer));
      }
      case "r": return this._error();
      case "\\": {
        const index = this._objects.push(undefined) - 1;
        if (typeof this._readHostObject !== "function") throw invalid();
        return (this._objects[index] = this._readHostObject());
      }
    }
    throw invalid();
  }
  _maybeView(buffer) {
    if (this._peek() !== 0x56) return buffer;
    this._offset++;
    const subtag = String.fromCharCode(this._byte());
    const offset = this._varint();
    const length = this._varint();
    if (this._version >= 14) this._varint();
    const entry = VIEW_TAGS.find(([tag]) => tag === subtag);
    if (!entry) throw invalid();
    const Ctor = entry[1];
    const size = Ctor.BYTES_PER_ELEMENT || 1;
    return this._keep(new Ctor(buffer, offset, Ctor === DataView ? length : length / size));
  }
  _error() {
    const index = this._objects.push(undefined) - 1;
    let Ctor = Error;
    const fields = {};
    for (;;) {
      const tag = String.fromCharCode(this._varint());
      if (tag === ".") break;
      if (ERROR_CTORS[tag]) Ctor = ERROR_CTORS[tag];
      else if (tag === "m") fields.message = this.readValue();
      else if (tag === "s") fields.stack = this.readValue();
      else if (tag === "c") fields.cause = this.readValue();
      else throw invalid();
    }
    const error = Object.create(Ctor.prototype);
    if ("message" in fields) Object.defineProperty(error, "message", { value: fields.message, writable: true, configurable: true });
    Object.defineProperty(error, "stack", { value: fields.stack, writable: true, configurable: true });
    if ("cause" in fields) Object.defineProperty(error, "cause", { value: fields.cause, writable: true, configurable: true });
    return (this._objects[index] = error);
  }
}

module.exports = { Deserializer };
