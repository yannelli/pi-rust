// internal/streams_readable: Readable with Node's flowing, paused, and readable-event modes.
const { Buffer } = require("buffer");
const common = require("internal/streams_common");
const { Stream, errors, nop, destroy, errorOrDestroy, eos, getHighWaterMark } = common;
let StringDecoder;

function ReadableState(options = {}, isDuplex) {
  this.objectMode = !!(options.objectMode || (isDuplex && options.readableObjectMode));
  this.highWaterMark = getHighWaterMark(options, "readableHighWaterMark", isDuplex, this.objectMode);
  this.buffer = [];
  this.length = 0;
  this.pipes = [];
  this.flowing = null;
  this.ended = false;
  this.endEmitted = false;
  this.reading = false;
  this.constructed = true;
  this.sync = true;
  this.needReadable = false;
  this.emittedReadable = false;
  this.readableListening = false;
  this.resumeScheduled = false;
  this.errorEmitted = false;
  this.emitClose = options.emitClose !== false;
  this.autoDestroy = options.autoDestroy !== false;
  this.destroyed = false;
  this.errored = null;
  this.closed = false;
  this.closeEmitted = false;
  this.defaultEncoding = options.defaultEncoding || "utf8";
  this.readingMore = false;
  this.dataEmitted = false;
  this.decoder = null;
  this.encoding = null;
  this.readable = true;
  if (options.encoding) {
    StringDecoder ??= require("string_decoder").StringDecoder;
    this.decoder = new StringDecoder(options.encoding);
    this.encoding = options.encoding;
  }
}

function Readable(options) {
  if (!(this instanceof Readable)) return new Readable(options);
  const isDuplex = this instanceof require("internal/streams_writable").Duplex;
  this._readableState = new ReadableState(options, isDuplex);
  if (options) {
    if (typeof options.read === "function") this._read = options.read;
    if (typeof options.destroy === "function") this._destroy = options.destroy;
    if (options.signal) common.addAbortSignal(options.signal, this);
  }
  Stream.call(this, options);
  if (!isDuplex && options && typeof options.construct === "function") common.construct(this, options.construct);
}
Object.setPrototypeOf(Readable.prototype, Stream.prototype);
Object.setPrototypeOf(Readable, Stream);
Readable.ReadableState = ReadableState;


Readable.prototype.destroy = destroy;
Readable.prototype._undestroy = nop;
Readable.prototype._destroy = function _destroy(error, callback) {
  callback(error);
};
Readable.prototype[Symbol.asyncDispose] = function asyncDispose() {
  return new Promise((resolve, reject) => {
    eos(this.destroy(), (error) => {
      if (error && error.code !== "ERR_STREAM_PREMATURE_CLOSE") reject(error);
      else resolve();
    });
  });
};
Readable.prototype._read = function _read() {
  throw errors.notImplemented("_read()");
};

Readable.prototype.push = function push(chunk, encoding) {
  return addChunk(this, chunk, encoding, false);
};
Readable.prototype.unshift = function unshift(chunk, encoding) {
  return addChunk(this, chunk, encoding, true);
};

function addChunk(stream, chunk, encoding, toFront) {
  const state = stream._readableState;
  if (chunk === null) {
    state.reading = false;
    onEof(stream, state);
    return false;
  }
  if (!state.objectMode) {
    if (typeof chunk === "string") {
      encoding = encoding || state.defaultEncoding;
      if (state.encoding !== encoding) chunk = Buffer.from(chunk, encoding);
    } else if (ArrayBuffer.isView(chunk) && !Buffer.isBuffer(chunk)) {
      chunk = Buffer.from(chunk.buffer, chunk.byteOffset, chunk.byteLength);
    } else if (!Buffer.isBuffer(chunk)) {
      errorOrDestroy(stream, errors.invalidArg("chunk", "of type string or an instance of Buffer, TypedArray, or DataView", chunk));
      return false;
    }
  }
  if (state.ended && !toFront) {
    errorOrDestroy(stream, errors.pushAfterEof());
    return false;
  }
  if (state.destroyed || state.errored) return false;
  state.reading = false;
  if (state.decoder && typeof chunk !== "string") {
    chunk = state.decoder.write(chunk);
    if (!state.objectMode && chunk.length === 0) {
      maybeReadMore(stream, state);
      return state.length < state.highWaterMark;
    }
  }
  if (state.flowing && state.length === 0 && !state.sync && stream.listenerCount("data") > 0 && !toFront) {
    state.dataEmitted = true;
    stream.emit("data", chunk);
  } else {
    state.length += state.objectMode ? 1 : chunk.length;
    if (toFront) state.buffer.unshift(chunk);
    else state.buffer.push(chunk);
    if (state.needReadable) emitReadable(stream);
  }
  maybeReadMore(stream, state);
  return !state.ended && (state.length < state.highWaterMark || state.length === 0);
}

function onEof(stream, state) {
  if (state.ended) return;
  if (state.decoder) {
    const rest = state.decoder.end();
    if (rest && rest.length) {
      state.buffer.push(rest);
      state.length += state.objectMode ? 1 : rest.length;
    }
  }
  state.ended = true;
  if (state.sync) {
    emitReadable(stream);
  } else {
    state.needReadable = false;
    state.emittedReadable = true;
    emitReadableNow(stream);
  }
}

function emitReadable(stream) {
  const state = stream._readableState;
  state.needReadable = false;
  if (!state.emittedReadable) {
    state.emittedReadable = true;
    process.nextTick(emitReadableNow, stream);
  }
}

function emitReadableNow(stream) {
  const state = stream._readableState;
  if (!state.destroyed && !state.errored && (state.length || state.ended)) {
    stream.emit("readable");
    state.emittedReadable = false;
  }
  state.needReadable = !state.flowing && !state.ended && state.length <= state.highWaterMark;
  flow(stream);
}

function maybeReadMore(stream, state) {
  if (!state.readingMore && state.constructed) {
    state.readingMore = true;
    process.nextTick(() => {
      while (!state.reading && !state.ended && (state.length < state.highWaterMark || (state.flowing && state.length === 0))) {
        const before = state.length;
        stream.read(0);
        if (before === state.length) break;
      }
      state.readingMore = false;
    });
  }
}

function howMuchToRead(n, state) {
  if (n <= 0 || (state.length === 0 && state.ended)) return 0;
  if (state.objectMode) return 1;
  if (Number.isNaN(n)) return state.flowing && state.length ? state.buffer[0].length : state.length;
  if (n <= state.length) return n;
  return state.ended ? state.length : 0;
}

// Take `n` units from the front of the buffer.
function takeFromBuffer(n, state) {
  if (state.length === 0) return null;
  if (state.objectMode) return state.buffer.shift();
  if (!n || n >= state.length) {
    let all;
    if (state.decoder) all = state.buffer.join("");
    else if (state.buffer.length === 1) all = state.buffer[0];
    else all = Buffer.concat(state.buffer, state.length);
    state.buffer = [];
    return all;
  }
  const parts = [];
  let needed = n;
  while (needed > 0) {
    const head = state.buffer[0];
    if (head.length <= needed) {
      parts.push(state.buffer.shift());
      needed -= head.length;
    } else {
      parts.push(head.slice(0, needed));
      state.buffer[0] = head.slice(needed);
      needed = 0;
    }
  }
  return state.decoder ? parts.join("") : Buffer.concat(parts, n);
}

Readable.prototype.read = function read(n) {
  const state = this._readableState;
  n = n === undefined ? NaN : Number.parseInt(n, 10);
  const requested = n;
  if (n !== 0) state.emittedReadable = false;
  if (n === 0 && state.needReadable && ((state.highWaterMark !== 0 ? state.length >= state.highWaterMark : state.length > 0) || state.ended)) {
    if (state.length === 0 && state.ended) endReadable(this);
    else emitReadable(this);
    return null;
  }
  n = howMuchToRead(n, state);
  if (n === 0 && state.ended) {
    if (state.length === 0) endReadable(this);
    return null;
  }
  let doRead = state.needReadable || state.length === 0 || state.length - n < state.highWaterMark;
  if (state.ended || state.reading || state.destroyed || state.errored || !state.constructed) doRead = false;
  if (doRead) {
    state.reading = true;
    state.sync = true;
    if (state.length === 0) state.needReadable = true;
    try {
      this._read(state.highWaterMark);
    } catch (error) {
      errorOrDestroy(this, error);
    }
    state.sync = false;
    if (!state.reading) n = howMuchToRead(requested, state);
  }
  const result = n > 0 ? takeFromBuffer(n, state) : null;
  if (result === null) {
    state.needReadable = state.length <= state.highWaterMark;
    n = 0;
  } else {
    state.length -= state.objectMode ? 1 : result.length;
  }
  if (state.length === 0) {
    if (!state.ended) state.needReadable = true;
    if (requested !== n && state.ended) endReadable(this);
  }
  if (result !== null && !state.errorEmitted && !state.closeEmitted) {
    state.dataEmitted = true;
    this.emit("data", result);
  }
  return result;
};

function endReadable(stream) {
  const state = stream._readableState;
  if (state.endEmitted) return;
  state.ended = true;
  process.nextTick(() => {
    if (state.errored || state.closeEmitted || state.endEmitted || state.length !== 0) return;
    state.endEmitted = true;
    stream.emit("end");
    const writeState = stream._writableState;
    if (state.autoDestroy && (!writeState || (writeState.autoDestroy && (writeState.finished || writeState.writable === false)))) {
      stream.destroy();
    }
  });
}

function flow(stream) {
  const state = stream._readableState;
  while (state.flowing && stream.read() !== null);
}

Readable.prototype.on = function on(event, listener) {
  const result = Stream.prototype.on.call(this, event, listener);
  const state = this._readableState;
  if (event === "data") {
    state.readableListening = this.listenerCount("readable") > 0;
    if (state.flowing !== false) this.resume();
  } else if (event === "readable" && !state.endEmitted && !state.readableListening) {
    state.readableListening = state.needReadable = true;
    state.flowing = false;
    state.emittedReadable = false;
    if (state.length) emitReadable(this);
    else if (!state.reading) process.nextTick(() => this.read(0));
  }
  return result;
};
Readable.prototype.addListener = Readable.prototype.on;

function updateReadableListening(stream) {
  const state = stream._readableState;
  state.readableListening = stream.listenerCount("readable") > 0;
  if (state.resumeScheduled && state.paused === false) state.flowing = true;
  else if (stream.listenerCount("data") > 0) stream.resume();
  else if (!state.readableListening) state.flowing = null;
}
Readable.prototype.removeListener = function removeListener(event, listener) {
  const result = Stream.prototype.removeListener.call(this, event, listener);
  if (event === "readable") process.nextTick(updateReadableListening, this);
  return result;
};
Readable.prototype.off = Readable.prototype.removeListener;
Readable.prototype.removeAllListeners = function removeAllListeners(event) {
  const result = Stream.prototype.removeAllListeners.apply(this, arguments);
  if (event === "readable" || event === undefined) process.nextTick(updateReadableListening, this);
  return result;
};

Readable.prototype.resume = function resume() {
  const state = this._readableState;
  if (!state.flowing) {
    state.flowing = !state.readableListening;
    if (!state.resumeScheduled) {
      state.resumeScheduled = true;
      process.nextTick(() => {
        if (!state.reading) this.read(0);
        state.resumeScheduled = false;
        this.emit("resume");
        flow(this);
        if (state.flowing && !state.reading) this.read(0);
      });
    }
  }
  state.paused = false;
  return this;
};
Readable.prototype.pause = function pause() {
  const state = this._readableState;
  if (state.flowing !== false) {
    state.flowing = false;
    this.emit("pause");
  }
  state.paused = true;
  return this;
};
Readable.prototype.isPaused = function isPaused() {
  const state = this._readableState;
  return state.paused === true || state.flowing === false;
};
Readable.prototype.setEncoding = function setEncoding(encoding) {
  StringDecoder ??= require("string_decoder").StringDecoder;
  const state = this._readableState;
  const decoder = new StringDecoder(encoding);
  state.decoder = decoder;
  state.encoding = decoder.encoding;
  const content = state.buffer.map((chunk) => decoder.write(chunk)).join("");
  state.buffer = content ? [content] : [];
  state.length = content.length;
  return this;
};

Readable.prototype.pipe = require("internal/streams_pipe").pipe;
Readable.prototype.unpipe = function unpipe(dest) {
  const state = this._readableState;
  if (state.pipes.length === 0) return this;
  const targets = dest ? [dest] : state.pipes.slice();
  for (const target of targets) {
    const index = state.pipes.indexOf(target);
    if (index === -1) continue;
    state.pipes.splice(index, 1);
    target.emit("unpipe", this, { hasUnpiped: false });
  }
  if (state.pipes.length === 0) this.pause();
  return this;
};

Readable.prototype.wrap = function wrap(stream) {
  let paused = false;
  stream.on("data", (chunk) => {
    if (!this.push(chunk) && stream.pause) {
      paused = true;
      stream.pause();
    }
  });
  stream.on("end", () => this.push(null));
  stream.on("error", (error) => errorOrDestroy(this, error));
  stream.on("close", () => this.destroy());
  this._read = () => {
    if (paused && stream.resume) {
      paused = false;
      stream.resume();
    }
  };
  return this;
};

Readable.prototype[Symbol.asyncIterator] = function asyncIterator() {
  return require("internal/streams_pipe").iterate(this);
};
Readable.prototype.iterator = function iterator(options) {
  return require("internal/streams_pipe").iterate(this, options);
};
Readable.prototype.toArray = async function toArray() {
  const items = [];
  for await (const item of this) items.push(item);
  return items;
};
Readable.prototype.forEach = async function forEach(fn) {
  for await (const item of this) await fn(item);
};

Object.defineProperty(Readable.prototype, "readable", {
  configurable: true,
  get() {
    const state = this._readableState;
    return !!state && state.readable !== false && !state.destroyed && !state.errorEmitted && !state.endEmitted;
  },
  set(value) {
    if (this._readableState) this._readableState.readable = !!value;
  },
});
Object.defineProperty(Readable.prototype, "destroyed", {
  configurable: true,
  get() {
    return this._readableState ? this._readableState.destroyed : false;
  },
  set(value) {
    if (this._readableState) this._readableState.destroyed = value;
  },
});
const STATE_GETTERS = {
  readableDidRead: (state) => state.dataEmitted,
  readableAborted: (state) => !!(state.destroyed || state.errored) && !state.endEmitted,
  readableHighWaterMark: (state) => state.highWaterMark,
  readableBuffer: (state) => state.buffer,
  readableLength: (state) => state.length,
  readableObjectMode: (state) => state.objectMode,
  readableEncoding: (state) => state.encoding,
  readableEnded: (state) => state.endEmitted,
  errored: (state) => state.errored,
  closed: (state) => state.closed,
};
for (const [name, read] of Object.entries(STATE_GETTERS)) {
  Object.defineProperty(Readable.prototype, name, {
    configurable: true,
    get() {
      return this._readableState ? read(this._readableState) : undefined;
    },
  });
}
Object.defineProperty(Readable.prototype, "readableFlowing", {
  get() {
    return this._readableState.flowing;
  },
  set(value) {
    this._readableState.flowing = value;
  },
});

Readable.from = (iterable, options) => require("internal/streams_pipe").from(Readable, iterable, options);
Readable.isDisturbed = (stream) => !!(stream._readableState && (stream._readableState.dataEmitted || stream._readableState.destroyed));
Readable.fromWeb = (webStream, options) => require("internal/streams_pipe").from(Readable, webStream, options);
Readable.toWeb = (stream) => require("internal/streams_web").ReadableStream.from(stream);

module.exports = { Readable, ReadableState };
