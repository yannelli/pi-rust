// node:zlib: Transform streams, callback and sync helpers over the native codecs.
const nativeZlib = native.zlib;
const { Transform, finished } = require("stream");
const { Buffer } = require("buffer");
const { codeError, invalidArgType, validate } = require("internal/errors");
const { constants, codes } = require("internal/zlib_constants");

const kFlushFlag = Symbol("kFlushFlag");
const kMaxLength = 2 ** 32 - 1;
const ZLIB_FLUSH = [0, 5];
const BROTLI_FLUSH = [0, 3];
const ZSTD_FLUSH = [0, 2];

function rangeError(name, lower, upper, value) {
  const range = upper === Infinity ? `>= ${lower}` : `>= ${lower} and <= ${upper}`;
  return codeError(RangeError, "ERR_OUT_OF_RANGE", `The value of "${name}" is out of range. It must be ${range}. Received ${value}`);
}

function checkRange(value, name, lower, upper, fallback) {
  if (value === undefined) return fallback;
  if (typeof value !== "number") throw invalidArgType(name, "of type number", value);
  if (!Number.isFinite(value)) {
    if (Number.isNaN(value)) return fallback;
    throw codeError(RangeError, "ERR_OUT_OF_RANGE", `The value of "${name}" is out of range. It must be a finite number. Received ${value}`);
  }
  if (value < lower || value > upper) throw rangeError(name, lower, upper, value);
  return value;
}

function toBytes(value, name) {
  if (typeof value === "string") return Buffer.from(value);
  if (value instanceof Uint8Array) return value;
  if (ArrayBuffer.isView(value)) return new Uint8Array(value.buffer, value.byteOffset, value.byteLength);
  if (value instanceof ArrayBuffer) return new Uint8Array(value);
  throw invalidArgType(name, "of type string or an instance of Buffer, TypedArray, DataView, or ArrayBuffer", value);
}

function checkParams(params, valid, code, kind) {
  const pairs = [];
  if (params === undefined) return pairs;
  validate.object(params, "options.params");
  for (const key of Object.keys(params)) {
    const id = Number(key);
    if (!Number.isInteger(id) || !valid(id)) throw codeError(RangeError, code, `${key} is not a valid ${kind} parameter`);
    const value = params[key];
    if (typeof value !== "number" && typeof value !== "boolean") throw invalidArgType("options.params[key]", "of type number", value);
    pairs.push([id, Number(value)]);
  }
  return pairs;
}

// Shared state for every engine; `flushRange` bounds the flush options.
function ZlibBase(opts, mode, nativeOptions, flushRange, defaults) {
  opts = opts || {};
  if (opts !== null && typeof opts !== "object") throw invalidArgType("options", "of type object", opts);
  const [lo, hi] = flushRange;
  this._defaultFlushFlag = checkRange(opts.flush, "options.flush", lo, hi, defaults.flush);
  this._finishFlushFlag = checkRange(opts.finishFlush, "options.finishFlush", lo, hi, defaults.finish);
  this._defaultFullFlushFlag = defaults.full;
  this._chunkSize = checkRange(opts.chunkSize, "options.chunkSize", 64, Infinity, 16384);
  this._maxOutputLength = checkRange(opts.maxOutputLength, "options.maxOutputLength", 1, kMaxLength, kMaxLength);
  Transform.call(this, { ...opts, autoDestroy: true, readableHighWaterMark: opts.readableHighWaterMark || this._chunkSize });
  this._mode = mode;
  this._nativeOptions = nativeOptions;
  this._handle = nativeZlib.create(mode, nativeOptions);
  this._closed = false;
  this._info = Boolean(opts.info);
  this.bytesWritten = 0;
  this._level = nativeOptions.level;
  this._strategy = opts.strategy;
  this._rejectGarbageAfterEnd = opts.rejectGarbageAfterEnd === true;
}

// Node fails only streams and the callback helpers on it, after pushing the output; sync calls ignore it.
ZlibBase.prototype._junkError = function junkError() {
  if (!this._rejectGarbageAfterEnd || this._handle === null || !nativeZlib.trailingJunk(this._handle)) return null;
  return codeError(TypeError, "ERR_TRAILING_JUNK_AFTER_STREAM_END", "Trailing junk found after the end of the compressed stream");
};
Object.setPrototypeOf(ZlibBase.prototype, Transform.prototype);
Object.setPrototypeOf(ZlibBase, Transform);

Object.defineProperty(ZlibBase.prototype, "bytesRead", {
  configurable: true,
  get() { return this.bytesWritten; },
  set(value) { this.bytesWritten = value; },
});

ZlibBase.prototype._run = function run(chunk, flushFlag) {
  if (this._handle === null) throw codeError(Error, "ERR_ZLIB_BINDING_CLOSED", "zlib binding closed");
  const out = nativeZlib.process(this._handle, chunk, flushFlag);
  this.bytesWritten += chunk.byteLength;
  return out;
};

ZlibBase.prototype._pushOut = function pushOut(out) {
  for (let offset = 0; offset < out.length; offset += this._chunkSize) {
    const end = Math.min(out.length, offset + this._chunkSize);
    this.push(Buffer.from(out.buffer, out.byteOffset + offset, end - offset));
  }
};

ZlibBase.prototype._transform = function transform(chunk, encoding, callback) {
  const flag = chunk[kFlushFlag] !== undefined ? chunk[kFlushFlag] : this._defaultFlushFlag;
  let out;
  try {
    out = this._run(chunk, flag);
  } catch (error) {
    this._closeHandle();
    return callback(error);
  }
  this._pushOut(out);
  const junk = this._junkError();
  if (junk) {
    this.destroy(junk);
    return callback(junk);
  }
  callback();
};

ZlibBase.prototype._flush = function flush(callback) {
  const marker = Buffer.alloc(0);
  marker[kFlushFlag] = this._finishFlushFlag;
  this._transform(marker, "", callback);
};

ZlibBase.prototype._processChunk = function processChunk(chunk, flushFlag, callback) {
  if (typeof callback === "function") {
    const marked = Buffer.from(chunk);
    marked[kFlushFlag] = flushFlag;
    return this.write(marked, callback);
  }
  const out = this._run(toBytes(chunk, "chunk"), flushFlag);
  return Buffer.from(out.buffer, out.byteOffset, out.byteLength);
};

ZlibBase.prototype.flush = function flush(kind, callback) {
  if (typeof kind === "function" || (kind === undefined && !callback)) {
    callback = kind;
    kind = this._defaultFullFlushFlag;
  }
  if (this.writableFinished) {
    if (callback) process.nextTick(callback);
  } else if (this.writableEnded) {
    if (callback) this.once("end", callback);
  } else {
    const marker = Buffer.alloc(0);
    marker[kFlushFlag] = kind;
    this.write(marker, "", callback);
  }
};

ZlibBase.prototype.params = function params(level, strategy, callback) {
  checkRange(level, "level", -1, 9, -1);
  checkRange(strategy, "strategy", 0, 4, 0);
  const apply = () => {
    if (this._handle !== null) nativeZlib.params(this._handle, level);
    this._level = level;
    this._strategy = strategy;
    if (callback) callback();
  };
  if (this._level !== level || this._strategy !== strategy) this.flush(constants.Z_SYNC_FLUSH, apply);
  else process.nextTick(apply);
};

ZlibBase.prototype.reset = function reset() {
  if (this._handle === null) throw codeError(Error, "ERR_ZLIB_BINDING_CLOSED", "zlib binding closed");
  nativeZlib.close(this._handle);
  this._handle = nativeZlib.create(this._mode, this._nativeOptions);
};

ZlibBase.prototype._closeHandle = function closeHandle() {
  if (this._handle !== null) nativeZlib.close(this._handle);
  this._handle = null;
  this._closed = true;
};

ZlibBase.prototype._destroy = function destroy(error, callback) {
  this._closeHandle();
  callback(error);
};

ZlibBase.prototype.close = function close(callback) {
  if (callback) finished(this, callback);
  this.destroy();
};

function zlibOptions(opts, mode) {
  opts = opts || {};
  const windowBits = opts.windowBits === 0 && (mode === constants.INFLATE || mode === constants.GUNZIP || mode === constants.UNZIP)
    ? 0
    : checkRange(opts.windowBits, "options.windowBits", 8, 15, 15);
  const level = checkRange(opts.level, "options.level", -1, 9, -1);
  checkRange(opts.memLevel, "options.memLevel", 1, 9, 8);
  checkRange(opts.strategy, "options.strategy", 0, 4, 0);
  let dictionary;
  if (opts.dictionary !== undefined) {
    if (!ArrayBuffer.isView(opts.dictionary) && !(opts.dictionary instanceof ArrayBuffer)) {
      throw invalidArgType("options.dictionary", "an instance of Buffer, TypedArray, DataView, or ArrayBuffer", opts.dictionary);
    }
    dictionary = toBytes(opts.dictionary, "options.dictionary");
  }
  return { level, windowBits, dictionary };
}

function Zlib(opts, mode) {
  const defaults = { flush: constants.Z_NO_FLUSH, finish: constants.Z_FINISH, full: constants.Z_FULL_FLUSH };
  ZlibBase.call(this, opts, mode, zlibOptions(opts, mode), ZLIB_FLUSH, defaults);
}
Object.setPrototypeOf(Zlib.prototype, ZlibBase.prototype);
Object.setPrototypeOf(Zlib, ZlibBase);

function engine(name, mode, Base) {
  const ctor = {
    [name]: function (opts) {
      if (!new.target) return new ctor(opts);
      Base.call(this, opts, mode);
    },
  }[name];
  Object.setPrototypeOf(ctor.prototype, Base.prototype);
  Object.setPrototypeOf(ctor, Base);
  return ctor;
}

const brotliParams = new Set([0, 1, 2, 3, 4, 5, 6, 7, 8]);
function Brotli(opts, mode) {
  opts = opts || {};
  const valid = mode === constants.BROTLI_ENCODE ? (id) => brotliParams.has(id) : (id) => id === 0 || id === 1;
  const params = checkParams(opts.params, valid, "ERR_BROTLI_INVALID_PARAM", "Brotli");
  const defaults = { flush: 0, finish: 2, full: 1 };
  ZlibBase.call(this, opts, mode, { params }, BROTLI_FLUSH, defaults);
}
Object.setPrototypeOf(Brotli.prototype, ZlibBase.prototype);
Object.setPrototypeOf(Brotli, ZlibBase);

const zstdCParams = new Set([100, 101, 102, 103, 104, 105, 106, 107, 160, 161, 162, 163, 164, 200, 201, 202, 400, 401, 402]);
function Zstd(opts, mode) {
  opts = opts || {};
  const valid = mode === constants.ZSTD_COMPRESS ? (id) => zstdCParams.has(id) : (id) => id === 100;
  const params = checkParams(opts.params, valid, "ERR_ZSTD_INVALID_PARAM", "zstd");
  const defaults = { flush: 0, finish: 2, full: 1 };
  const pledgedSrcSize = opts.pledgedSrcSize === undefined ? undefined : checkRange(opts.pledgedSrcSize, "options.pledgedSrcSize", 0, Number.MAX_SAFE_INTEGER, 0);
  ZlibBase.call(this, opts, mode, { params, pledgedSrcSize }, ZSTD_FLUSH, defaults);
}
Object.setPrototypeOf(Zstd.prototype, ZlibBase.prototype);
Object.setPrototypeOf(Zstd, ZlibBase);

const Deflate = engine("Deflate", constants.DEFLATE, Zlib);
const Inflate = engine("Inflate", constants.INFLATE, Zlib);
const Gzip = engine("Gzip", constants.GZIP, Zlib);
const Gunzip = engine("Gunzip", constants.GUNZIP, Zlib);
const DeflateRaw = engine("DeflateRaw", constants.DEFLATERAW, Zlib);
const InflateRaw = engine("InflateRaw", constants.INFLATERAW, Zlib);
const Unzip = engine("Unzip", constants.UNZIP, Zlib);
const BrotliCompress = engine("BrotliCompress", constants.BROTLI_ENCODE, Brotli);
const BrotliDecompress = engine("BrotliDecompress", constants.BROTLI_DECODE, Brotli);
const ZstdCompress = engine("ZstdCompress", constants.ZSTD_COMPRESS, Zstd);
const ZstdDecompress = engine("ZstdDecompress", constants.ZSTD_DECOMPRESS, Zstd);

function bufferSync(stream, buffer, failOnJunk = false) {
  const bytes = toBytes(buffer, "buffer");
  if (stream._mode === constants.ZSTD_COMPRESS && stream._nativeOptions.pledgedSrcSize === undefined) {
    nativeZlib.close(stream._handle);
    stream._handle = nativeZlib.create(stream._mode, { ...stream._nativeOptions, pledgedSrcSize: bytes.byteLength });
  }
  let out;
  try {
    out = stream._run(bytes, stream._finishFlushFlag);
    const junk = failOnJunk && stream._junkError();
    if (junk) throw junk;
  } finally {
    stream._closeHandle();
  }
  if (out.length > stream._maxOutputLength) {
    throw codeError(RangeError, "ERR_BUFFER_TOO_LARGE", `Cannot create a Buffer larger than ${stream._maxOutputLength} bytes`);
  }
  const result = Buffer.from(out.buffer, out.byteOffset, out.byteLength);
  return stream._info ? { buffer: result, engine: stream } : result;
}

const api = {
  constants,
  codes,
  Deflate, Inflate, Gzip, Gunzip, DeflateRaw, InflateRaw, Unzip, BrotliCompress, BrotliDecompress, ZstdCompress, ZstdDecompress,
  crc32(data, value = 0) {
    if (typeof data !== "string" && !ArrayBuffer.isView(data)) {
      throw invalidArgType("data", "of type string or an instance of Buffer, TypedArray, or DataView", data);
    }
    validate.integer(value, "value", 0, 2 ** 32 - 1);
    return nativeZlib.crc32(toBytes(data, "data"), value);
  },
};

const engines = {
  deflate: Deflate, inflate: Inflate, gzip: Gzip, gunzip: Gunzip, deflateRaw: DeflateRaw, inflateRaw: InflateRaw,
  unzip: Unzip, brotliCompress: BrotliCompress, brotliDecompress: BrotliDecompress, zstdCompress: ZstdCompress,
  zstdDecompress: ZstdDecompress,
};
for (const [name, Engine] of Object.entries(engines)) {
  const Name = name[0].toUpperCase() + name.slice(1);
  api["create" + Name] = (options) => new Engine(options);
  api[name + "Sync"] = { [name + "Sync"]: (buffer, options) => bufferSync(new Engine(options), buffer) }[name + "Sync"];
  api[name] = {
    [name](buffer, options, callback) {
      if (typeof options === "function") {
        callback = options;
        options = {};
      }
      validate.function(callback, "callback");
      const bytes = toBytes(buffer, "buffer");
      const stream = new Engine(options);
      setImmediate(() => {
        let result;
        try {
          result = bufferSync(stream, bytes, true);
        } catch (error) {
          return callback(error);
        }
        callback(null, result);
      });
    },
  }[name];
}

for (const [key, value] of Object.entries(constants)) {
  if (key.startsWith("Z_")) Object.defineProperty(api, key, { value, enumerable: false });
}

module.exports = api;
