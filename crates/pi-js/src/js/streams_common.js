// internal/streams_common: the legacy Stream base, errors, destroy, and end-of-stream helpers.
const EventEmitter = require("events");

function codedError(Base, code, message) {
  const error = new Base(message);
  error.code = code;
  return error;
}
const errors = {
  invalidArg: (name, expected, value) =>
    codedError(TypeError, "ERR_INVALID_ARG_TYPE", `The "${name}" argument must be ${expected}. Received ${value === null ? "null" : typeof value}`),
  nullValues: () => codedError(TypeError, "ERR_STREAM_NULL_VALUES", "May not write null values to stream"),
  pushAfterEof: () => codedError(Error, "ERR_STREAM_PUSH_AFTER_EOF", "stream.push() after EOF"),
  writeAfterEnd: () => codedError(Error, "ERR_STREAM_WRITE_AFTER_END", "write after end"),
  destroyed: (method) => codedError(Error, "ERR_STREAM_DESTROYED", `Cannot call ${method} after a stream was destroyed`),
  alreadyFinished: (method) => codedError(Error, "ERR_STREAM_ALREADY_FINISHED", `Cannot call ${method} after a stream was finished`),
  multipleCallback: () => codedError(Error, "ERR_MULTIPLE_CALLBACK", "Callback called multiple times"),
  prematureClose: () => codedError(Error, "ERR_STREAM_PREMATURE_CLOSE", "Premature close"),
  unknownEncoding: (name) => codedError(TypeError, "ERR_UNKNOWN_ENCODING", `Unknown encoding: ${name}`),
  missingArgs: (name) => codedError(TypeError, "ERR_MISSING_ARGS", `The "${name}" argument must be specified`),
  notImplemented: (method) => codedError(Error, "ERR_METHOD_NOT_IMPLEMENTED", `The ${method} method is not implemented`),
  abort: (cause) => Object.assign(new Error("The operation was aborted"), { name: "AbortError", code: "ABORT_ERR", cause }),
};

const nop = () => {};

function once(callback) {
  let called = false;
  return function onceWrapper(...args) {
    if (called) return;
    called = true;
    return Reflect.apply(callback, this, args);
  };
}

let defaultHighWaterMark = 64 * 1024;
let defaultObjectHighWaterMark = 16;
function getHighWaterMark(options, duplexKey, isDuplex, objectMode) {
  let value = options ? options.highWaterMark : undefined;
  if ((value === undefined || value === null) && isDuplex && options) value = options[duplexKey];
  if (value === undefined || value === null) return objectMode ? defaultObjectHighWaterMark : defaultHighWaterMark;
  if (!Number.isInteger(value) || value < 0) {
    throw codedError(TypeError, "ERR_INVALID_ARG_VALUE", `The property 'options.${duplexKey}' is invalid. Received ${value}`);
  }
  return value;
}
function setDefaultHighWaterMark(objectMode, value) {
  if (objectMode) defaultObjectHighWaterMark = value;
  else defaultHighWaterMark = value;
}
function getDefaultHighWaterMark(objectMode) {
  return objectMode ? defaultObjectHighWaterMark : defaultHighWaterMark;
}

function Stream(options) {
  EventEmitter.call(this, options);
}
Object.setPrototypeOf(Stream.prototype, EventEmitter.prototype);
Object.setPrototypeOf(Stream, EventEmitter);

// Legacy pipe for streams built directly on Stream (old-style npm packages).
Stream.prototype.pipe = function pipe(dest, options) {
  const source = this;
  function ondata(chunk) {
    if (dest.writable && dest.write(chunk) === false && source.pause) source.pause();
  }
  function ondrain() {
    if (source.readable && source.resume) source.resume();
  }
  source.on("data", ondata);
  dest.on("drain", ondrain);
  if (!dest._isStdio && (!options || options.end !== false)) {
    source.on("end", () => dest.end());
  }
  dest.emit("pipe", source);
  return dest;
};

function emitErrorNT(stream, error) {
  const writeState = stream._writableState;
  const readState = stream._readableState;
  if ((writeState && writeState.errorEmitted) || (readState && readState.errorEmitted)) return;
  if (writeState) writeState.errorEmitted = true;
  if (readState) readState.errorEmitted = true;
  stream.emit("error", error);
}

function emitCloseNT(stream) {
  const writeState = stream._writableState;
  const readState = stream._readableState;
  if (writeState) writeState.closeEmitted = true;
  if (readState) readState.closeEmitted = true;
  if ((writeState && writeState.emitClose) || (readState && readState.emitClose)) stream.emit("close");
}

function markErrored(stream, error) {
  for (const state of [stream._writableState, stream._readableState]) {
    if (state && !state.errored) state.errored = error;
  }
}

function destroy(error, callback) {
  const writeState = this._writableState;
  const readState = this._readableState;
  if ((writeState && writeState.destroyed) || (readState && readState.destroyed)) {
    if (typeof callback === "function") callback();
    return this;
  }
  if (error) markErrored(this, error);
  if (writeState) writeState.destroyed = true;
  if (readState) readState.destroyed = true;
  const done = once((failure) => {
    if (failure) markErrored(this, failure);
    if (writeState) writeState.closed = true;
    if (readState) readState.closed = true;
    if (typeof callback === "function") callback(failure);
    process.nextTick(() => {
      if (failure) emitErrorNT(this, failure);
      emitCloseNT(this);
    });
  });
  try {
    this._destroy(error || null, done);
  } catch (failure) {
    done(failure);
  }
  return this;
}

function errorOrDestroy(stream, error, sync) {
  const writeState = stream._writableState;
  const readState = stream._readableState;
  if ((writeState && writeState.destroyed) || (readState && readState.destroyed)) return;
  if ((readState && readState.autoDestroy) || (writeState && writeState.autoDestroy)) {
    stream.destroy(error);
    return;
  }
  markErrored(stream, error);
  if (sync) process.nextTick(emitErrorNT, stream, error);
  else emitErrorNT(stream, error);
}

function isReadableStream(stream) {
  if (!stream || typeof stream.on !== "function") return false;
  const readState = stream._readableState;
  if (readState) return readState.readable !== false;
  return typeof stream.pipe === "function" && typeof stream.write !== "function";
}

function isWritableStream(stream) {
  if (!stream || typeof stream.on !== "function") return false;
  const writeState = stream._writableState;
  if (writeState) return writeState.writable !== false;
  return typeof stream.write === "function" && typeof stream.end === "function";
}

function isNodeStream(stream) {
  return isReadableStream(stream) || isWritableStream(stream) || !!(stream && (stream._readableState || stream._writableState));
}

// finished(): call back once the stream has ended, finished, errored, or closed early.
function eos(stream, options, callback) {
  if (typeof options === "function") {
    callback = options;
    options = {};
  }
  options = options || {};
  callback = once(callback);
  const readable = options.readable ?? isReadableStream(stream);
  const writable = options.writable ?? isWritableStream(stream);
  const readState = stream._readableState;
  const writeState = stream._writableState;
  let readableDone = !readable || !!(readState && readState.endEmitted);
  let writableDone = !writable || !!(writeState && writeState.finished);
  const onend = () => {
    readableDone = true;
    if (writableDone) callback.call(stream);
  };
  const onfinish = () => {
    writableDone = true;
    if (readableDone) callback.call(stream);
  };
  const onerror = (error) => callback.call(stream, error);
  const onclose = () => {
    const error = (readState && readState.errored) || (writeState && writeState.errored);
    if (error && options.error !== false) return callback.call(stream, error);
    if (!readableDone && !(readState && readState.endEmitted)) return callback.call(stream, errors.prematureClose());
    if (!writableDone && !(writeState && writeState.finished)) return callback.call(stream, errors.prematureClose());
    callback.call(stream);
  };
  stream.on("end", onend);
  stream.on("finish", onfinish);
  if (options.error !== false) stream.on("error", onerror);
  stream.on("close", onclose);
  const errored = (readState && readState.errorEmitted && readState.errored) || (writeState && writeState.errorEmitted && writeState.errored);
  if (errored) process.nextTick(onerror, errored);
  else if ((writeState && writeState.closeEmitted) || (readState && readState.closeEmitted)) process.nextTick(onclose);
  else if (readableDone && writableDone && (readState || writeState)) process.nextTick(() => callback.call(stream));
  const cleanup = () => {
    callback = nop;
    stream.removeListener("end", onend);
    stream.removeListener("finish", onfinish);
    stream.removeListener("error", onerror);
    stream.removeListener("close", onclose);
  };
  if (options.signal) {
    const onabort = () => {
      cleanup();
      callback.call(stream, errors.abort(options.signal.reason));
    };
    if (options.signal.aborted) process.nextTick(onabort);
    else options.signal.addEventListener("abort", onabort, { once: true });
  }
  return cleanup;
}

// Run `construct` before any _read, _write, or _final, as Node does.
function construct(stream, fn) {
  const states = [stream._readableState, stream._writableState].filter(Boolean);
  for (const state of states) state.constructed = false;
  process.nextTick(() => {
    let called = false;
    const done = (error) => {
      if (called) return errorOrDestroy(stream, errors.multipleCallback());
      called = true;
      for (const state of states) state.constructed = true;
      if (error) return errorOrDestroy(stream, error, true);
      if (stream._readableState && stream._readableState.needReadable) stream.read(0);
      if (stream._writableState) require("internal/streams_writable").afterConstruct(stream);
    };
    try {
      fn.call(stream, done);
    } catch (error) {
      done(error);
    }
  });
}

function addAbortSignal(signal, stream) {
  if (!signal || typeof signal.addEventListener !== "function") throw errors.invalidArg("signal", "an instance of AbortSignal", signal);
  const onabort = () => stream.destroy(errors.abort(signal.reason));
  if (signal.aborted) onabort();
  else {
    signal.addEventListener("abort", onabort, { once: true });
    eos(stream, () => signal.removeEventListener("abort", onabort));
  }
  return stream;
}

module.exports = {
  Stream,
  addAbortSignal,
  construct,
  errors,
  nop,
  once,
  destroy,
  errorOrDestroy,
  eos,
  isReadableStream,
  isWritableStream,
  isNodeStream,
  getHighWaterMark,
  getDefaultHighWaterMark,
  setDefaultHighWaterMark,
};
