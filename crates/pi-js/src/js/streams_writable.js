// internal/streams_writable: Writable, Duplex, Transform, and PassThrough.
const { Buffer } = require("buffer");
const common = require("internal/streams_common");
const { Stream, errors, nop, destroy, errorOrDestroy, getHighWaterMark } = common;
const { Readable } = require("internal/streams_readable");
const kOnFinished = Symbol("kOnFinished");

function WritableState(options = {}, stream, isDuplex) {
  this.objectMode = !!(options.objectMode || (isDuplex && options.writableObjectMode));
  this.highWaterMark = getHighWaterMark(options, "writableHighWaterMark", isDuplex, this.objectMode);
  this.finalCalled = false;
  this.needDrain = false;
  this.ending = false;
  this.ended = false;
  this.finished = false;
  this.destroyed = false;
  this.decodeStrings = options.decodeStrings !== false;
  this.defaultEncoding = options.defaultEncoding || "utf8";
  this.length = 0;
  this.writing = false;
  this.corked = 0;
  this.sync = true;
  this.bufferProcessing = false;
  this.onwrite = (error) => onwrite(stream, error);
  this.writecb = null;
  this.writelen = 0;
  this.buffered = [];
  this.pendingcb = 0;
  this.constructed = true;
  this.prefinished = false;
  this.errorEmitted = false;
  this.emitClose = options.emitClose !== false;
  this.autoDestroy = options.autoDestroy !== false;
  this.errored = null;
  this.closed = false;
  this.closeEmitted = false;
  this.writable = true;
  this[kOnFinished] = [];
}

function Writable(options) {
  const isDuplex = this instanceof Duplex;
  if (!isDuplex && !(this instanceof Writable)) return new Writable(options);
  this._writableState = new WritableState(options, this, isDuplex);
  if (options) {
    if (typeof options.write === "function") this._write = options.write;
    if (typeof options.writev === "function") this._writev = options.writev;
    if (typeof options.destroy === "function") this._destroy = options.destroy;
    if (typeof options.final === "function") this._final = options.final;
    if (options.signal) common.addAbortSignal(options.signal, this);
  }
  Stream.call(this, options);
  if (options && typeof options.construct === "function" && !isDuplex) common.construct(this, options.construct);
}
Object.setPrototypeOf(Writable.prototype, Stream.prototype);
Object.setPrototypeOf(Writable, Stream);
Writable.WritableState = WritableState;
Object.defineProperty(Writable, Symbol.hasInstance, {
  value(object) {
    if (Function.prototype[Symbol.hasInstance].call(this, object)) return true;
    return this === Writable && !!(object && object._writableState instanceof WritableState);
  },
});

function writeChunk(stream, chunk, encoding, callback) {
  const state = stream._writableState;
  if (typeof encoding === "function") {
    callback = encoding;
    encoding = state.defaultEncoding;
  } else {
    if (!encoding) encoding = state.defaultEncoding;
    else if (encoding !== "buffer" && !Buffer.isEncoding(encoding)) throw errors.unknownEncoding(encoding);
    if (typeof callback !== "function") callback = nop;
  }
  if (chunk === null) throw errors.nullValues();
  if (!state.objectMode) {
    if (typeof chunk === "string") {
      if (state.decodeStrings) {
        chunk = Buffer.from(chunk, encoding);
        encoding = "buffer";
      }
    } else if (chunk instanceof Uint8Array) {
      if (!Buffer.isBuffer(chunk)) chunk = Buffer.from(chunk.buffer, chunk.byteOffset, chunk.byteLength);
      encoding = "buffer";
    } else {
      throw errors.invalidArg("chunk", "of type string or an instance of Buffer or Uint8Array", chunk);
    }
  }
  let error;
  if (state.ending) error = errors.writeAfterEnd();
  else if (state.destroyed) error = errors.destroyed("write");
  if (error) {
    process.nextTick(callback, error);
    errorOrDestroy(stream, error, true);
    return error;
  }
  state.pendingcb++;
  const length = state.objectMode ? 1 : chunk.length;
  state.length += length;
  const belowMark = state.length < state.highWaterMark;
  if (!belowMark) state.needDrain = true;
  if (state.writing || state.corked || state.errored || !state.constructed) {
    state.buffered.push({ chunk, encoding, callback });
  } else {
    doWrite(stream, state, false, length, chunk, encoding, callback);
  }
  return belowMark && !state.errored && !state.destroyed;
}

function doWrite(stream, state, writev, length, chunk, encoding, callback) {
  state.writelen = length;
  state.writecb = callback;
  state.writing = true;
  state.sync = true;
  if (state.destroyed) state.onwrite(errors.destroyed("write"));
  else if (writev) stream._writev(chunk, state.onwrite);
  else stream._write(chunk, encoding, state.onwrite);
  state.sync = false;
}

function onwrite(stream, error) {
  const state = stream._writableState;
  const sync = state.sync;
  const callback = state.writecb;
  if (typeof callback !== "function") {
    errorOrDestroy(stream, errors.multipleCallback());
    return;
  }
  state.writing = false;
  state.writecb = null;
  state.length -= state.writelen;
  state.writelen = 0;
  if (error) {
    if (!state.errored) state.errored = error;
    const fail = () => {
      state.pendingcb--;
      callback(error);
      failBuffered(state);
      errorOrDestroy(stream, error);
    };
    if (sync) process.nextTick(fail);
    else fail();
    return;
  }
  if (state.buffered.length) clearBuffer(stream, state);
  if (sync) process.nextTick(afterWrite, stream, state, callback);
  else afterWrite(stream, state, callback);
}

function afterWrite(stream, state, callback) {
  if (!state.ending && !stream.destroyed && state.length === 0 && state.needDrain) {
    state.needDrain = false;
    stream.emit("drain");
  }
  state.pendingcb--;
  callback();
  if (state.destroyed) failBuffered(state);
  finishMaybe(stream, state);
}

function failBuffered(state) {
  if (state.writing) return;
  for (const { chunk, callback } of state.buffered.splice(0)) {
    state.length -= state.objectMode ? 1 : chunk.length;
    state.pendingcb--;
    callback(state.errored || errors.destroyed("write"));
  }
  for (const callback of state[kOnFinished].splice(0)) callback(state.errored || errors.destroyed("end"));
}

function clearBuffer(stream, state) {
  if (state.corked || state.bufferProcessing || state.destroyed || !state.constructed) return;
  state.bufferProcessing = true;
  if (state.buffered.length > 1 && stream._writev) {
    const entries = state.buffered.splice(0);
    const length = state.objectMode ? entries.length : entries.reduce((sum, entry) => sum + entry.chunk.length, 0);
    state.pendingcb -= entries.length - 1;
    const callback = (error) => entries.forEach((entry) => entry.callback(error));
    doWrite(stream, state, true, length, entries.map(({ chunk, encoding }) => ({ chunk, encoding })), "", callback);
  } else {
    while (state.buffered.length && !state.writing) {
      const { chunk, encoding, callback } = state.buffered.shift();
      doWrite(stream, state, false, state.objectMode ? 1 : chunk.length, chunk, encoding, callback);
    }
  }
  state.bufferProcessing = false;
}

function needFinish(state) {
  return state.ending && !state.destroyed && state.constructed && state.length === 0 && !state.errored &&
    state.buffered.length === 0 && !state.finished && !state.writing && !state.errorEmitted && !state.closeEmitted;
}

function finishMaybe(stream, state, sync) {
  if (!needFinish(state)) return;
  prefinish(stream, state);
  if (state.pendingcb !== 0) return;
  state.pendingcb++;
  const complete = () => {
    if (needFinish(state)) finish(stream, state);
    else state.pendingcb--;
  };
  if (sync) process.nextTick(complete);
  else complete();
}

function prefinish(stream, state) {
  if (state.prefinished || state.finalCalled) return;
  if (typeof stream._final !== "function" || state.destroyed) {
    state.prefinished = true;
    stream.emit("prefinish");
    return;
  }
  state.finalCalled = true;
  state.sync = true;
  state.pendingcb++;
  let called = false;
  const onFinal = (error) => {
    if (called) return errorOrDestroy(stream, errors.multipleCallback());
    called = true;
    state.pendingcb--;
    if (error) {
      for (const callback of state[kOnFinished].splice(0)) callback(error);
      errorOrDestroy(stream, error, state.sync);
    } else if (needFinish(state)) {
      state.prefinished = true;
      stream.emit("prefinish");
      state.pendingcb++;
      process.nextTick(finish, stream, state);
    }
  };
  try {
    stream._final(onFinal);
  } catch (error) {
    onFinal(error);
  }
  state.sync = false;
}

function finish(stream, state) {
  state.pendingcb--;
  state.finished = true;
  for (const callback of state[kOnFinished].splice(0)) callback();
  stream.emit("finish");
  if (state.autoDestroy) {
    const readState = stream._readableState;
    if (!readState || (readState.autoDestroy && (readState.endEmitted || readState.readable === false))) stream.destroy();
  }
}

Writable.prototype.write = function write(chunk, encoding, callback) {
  return writeChunk(this, chunk, encoding, callback) === true;
};
Writable.prototype.end = function end(chunk, encoding, callback) {
  const state = this._writableState;
  if (typeof chunk === "function") {
    callback = chunk;
    chunk = null;
    encoding = null;
  } else if (typeof encoding === "function") {
    callback = encoding;
    encoding = null;
  }
  let error;
  if (chunk !== null && chunk !== undefined) {
    const result = writeChunk(this, chunk, encoding);
    if (result instanceof Error) error = result;
  }
  if (state.corked) {
    state.corked = 1;
    this.uncork();
  }
  if (!error) {
    if (!state.errored && !state.ending) {
      state.ending = true;
      finishMaybe(this, state, true);
      state.ended = true;
    } else if (state.finished) error = errors.alreadyFinished("end");
    else if (state.destroyed) error = errors.destroyed("end");
  }
  if (typeof callback === "function") {
    if (error || state.finished) process.nextTick(callback, error);
    else state[kOnFinished].push(callback);
  }
  return this;
};
Writable.prototype.cork = function cork() {
  this._writableState.corked++;
};
Writable.prototype.uncork = function uncork() {
  const state = this._writableState;
  if (state.corked) {
    state.corked--;
    if (!state.writing) clearBuffer(this, state);
  }
};
Writable.prototype.setDefaultEncoding = function setDefaultEncoding(encoding) {
  if (typeof encoding === "string") encoding = encoding.toLowerCase();
  if (!Buffer.isEncoding(encoding)) throw errors.unknownEncoding(encoding);
  this._writableState.defaultEncoding = encoding;
  return this;
};
Writable.prototype._write = function _write(chunk, encoding, callback) {
  if (this._writev) this._writev([{ chunk, encoding }], callback);
  else throw errors.notImplemented("_write()");
};
Writable.prototype._writev = null;
Writable.prototype.destroy = function destroyWritable(error, callback) {
  const state = this._writableState;
  if (!state.destroyed && (state.buffered.length || state[kOnFinished].length)) process.nextTick(failBuffered, state);
  return destroy.call(this, error, callback);
};
Writable.prototype._undestroy = nop;
Writable.prototype._destroy = function _destroy(error, callback) {
  callback(error);
};

const WRITABLE_GETTERS = {
  writable: (state) => state.writable !== false && !state.destroyed && !state.errored && !state.ending && !state.ended,
  writableFinished: (state) => state.finished,
  writableObjectMode: (state) => state.objectMode,
  writableBuffer: (state) => state.buffered.map((entry) => entry.chunk),
  writableEnded: (state) => state.ending,
  writableNeedDrain: (state) => !state.destroyed && !state.ending && state.needDrain,
  writableHighWaterMark: (state) => state.highWaterMark,
  writableCorked: (state) => state.corked,
  writableLength: (state) => state.length,
  writableAborted: (state) => !!(state.destroyed || state.errored) && !state.finished,
  errored: (state) => state.errored,
  closed: (state) => state.closed,
};
for (const [name, read] of Object.entries(WRITABLE_GETTERS)) {
  Object.defineProperty(Writable.prototype, name, {
    configurable: true,
    get() {
      return this._writableState ? read(this._writableState) : undefined;
    },
  });
}
Object.defineProperty(Writable.prototype, "destroyed", {
  configurable: true,
  get() {
    return this._writableState ? this._writableState.destroyed : false;
  },
  set(value) {
    if (this._writableState) this._writableState.destroyed = value;
  },
});

function afterConstruct(stream) {
  const state = stream._writableState;
  clearBuffer(stream, state);
  finishMaybe(stream, state);
}

function Duplex(options) {
  if (!(this instanceof Duplex)) return new Duplex(options);
  Readable.call(this, options);
  Writable.call(this, options);
  this.allowHalfOpen = !options || options.allowHalfOpen !== false;
  if (options && options.readable === false) {
    this._readableState.readable = false;
    this._readableState.ended = true;
    this._readableState.endEmitted = true;
  }
  if (options && options.writable === false) {
    this._writableState.writable = false;
    this._writableState.ending = this._writableState.ended = this._writableState.finished = true;
  }
  if (options && typeof options.construct === "function") common.construct(this, options.construct);
  if (!this.allowHalfOpen) {
    this.once("end", () => {
      if (!this._writableState.ended) process.nextTick(() => this.end());
    });
  }
}
Object.setPrototypeOf(Duplex.prototype, Readable.prototype);
Object.setPrototypeOf(Duplex, Readable);
for (const name of Object.getOwnPropertyNames(Writable.prototype)) {
  if (name === "constructor" || Object.prototype.hasOwnProperty.call(Duplex.prototype, name)) continue;
  if (name === "destroyed" || name === "errored" || name === "closed") continue;
  Object.defineProperty(Duplex.prototype, name, Object.getOwnPropertyDescriptor(Writable.prototype, name));
}
Duplex.prototype.destroy = Writable.prototype.destroy;
Object.defineProperty(Duplex.prototype, "destroyed", {
  configurable: true,
  get() {
    return !!(this._readableState && this._writableState && this._readableState.destroyed && this._writableState.destroyed);
  },
  set(value) {
    this._readableState.destroyed = value;
    this._writableState.destroyed = value;
  },
});

const kCallback = Symbol("kCallback");
function Transform(options) {
  if (!(this instanceof Transform)) return new Transform(options);
  Duplex.call(this, options);
  this._readableState.sync = false;
  this[kCallback] = null;
  if (options) {
    if (typeof options.transform === "function") this._transform = options.transform;
    if (typeof options.flush === "function") this._flush = options.flush;
  }
  this.on("prefinish", transformPrefinish);
}
Object.setPrototypeOf(Transform.prototype, Duplex.prototype);
Object.setPrototypeOf(Transform, Duplex);

function transformFinal(callback) {
  if (typeof this._flush === "function" && !this.destroyed) {
    this._flush((error, data) => {
      if (error) {
        if (callback) callback(error);
        else this.destroy(error);
        return;
      }
      if (data !== null && data !== undefined) this.push(data);
      this.push(null);
      if (callback) callback();
    });
  } else {
    this.push(null);
    if (callback) callback();
  }
}
function transformPrefinish() {
  if (this._final !== transformFinal) transformFinal.call(this);
}
Transform.prototype._final = transformFinal;
Transform.prototype._transform = function _transform() {
  throw errors.notImplemented("_transform()");
};
Transform.prototype._write = function _write(chunk, encoding, callback) {
  const readState = this._readableState;
  const writeState = this._writableState;
  const before = readState.length;
  this._transform(chunk, encoding, (error, value) => {
    if (error) return callback(error);
    if (value !== null && value !== undefined) this.push(value);
    if (writeState.ended || before === readState.length || readState.length < readState.highWaterMark) callback();
    else this[kCallback] = callback;
  });
};
Transform.prototype._read = function _read() {
  const callback = this[kCallback];
  if (callback) {
    this[kCallback] = null;
    callback();
  }
};

function PassThrough(options) {
  if (!(this instanceof PassThrough)) return new PassThrough(options);
  Transform.call(this, options);
}
Object.setPrototypeOf(PassThrough.prototype, Transform.prototype);
Object.setPrototypeOf(PassThrough, Transform);
PassThrough.prototype._transform = function _transform(chunk, encoding, callback) {
  callback(null, chunk);
};

module.exports = { Writable, WritableState, Duplex, Transform, PassThrough, afterConstruct };
