// internal/streams_web_writable: WritableStream, TransformStream, strategies, and text streams.
const { ReadableStream, kState, deferred, typeError } = require("internal/streams_web");

function callSink(sink, method, ...args) {
  if (typeof sink[method] === "function") return sink[method].call(sink, ...args);
  return undefined;
}

class WritableStreamDefaultController {
  #abort = new AbortController();
  constructor(stream) {
    this[kState] = stream;
  }
  get signal() {
    return this.#abort.signal;
  }
  error(reason) {
    errorWritable(this[kState], reason);
  }
  static abort(controller, reason) {
    controller.#abort.abort(reason);
  }
}

function desiredSize(state) {
  if (state.state === "errored") return null;
  if (state.state === "closed") return 0;
  return state.highWaterMark - state.queueSize;
}

// writer.ready is pending while the queue is at or above the high-water mark.
function updateReady(stream) {
  const state = stream[kState];
  if (!state.writer) return;
  const writer = state.writer[kState];
  const shouldWait = state.state === "writable" && desiredSize(state) <= 0;
  if (shouldWait && !writer.readyPending) {
    writer.ready = deferred();
    writer.readyPending = true;
  } else if (!shouldWait && writer.readyPending) {
    writer.ready.resolve();
    writer.readyPending = false;
  }
}

function errorWritable(stream, reason) {
  const state = stream[kState];
  if (state.state === "errored" || state.state === "closed") return;
  state.state = "errored";
  state.storedError = reason;
  for (const { done } of state.queue.splice(0)) done.reject(reason);
  if (state.closeRequest) state.closeRequest.reject(reason);
  const writer = state.writer;
  if (writer) {
    writer[kState].readyPending = false;
    // As in the spec, these rejections are marked handled; callers see them through write().
    for (const promise of [writer[kState].closed, writer[kState].ready]) {
      promise.reject(reason);
      promise.promise.catch(() => {});
    }
  }
}

// Run queued writes one at a time, then the close once the queue is empty.
function processQueue(stream) {
  const state = stream[kState];
  if (!state.started || state.writing || state.state === "errored" || state.state === "closed") return;
  if (state.queue.length) {
    const { chunk, size, done } = state.queue[0];
    state.writing = true;
    Promise.resolve()
      .then(() => callSink(state.sink, "write", chunk, state.controller))
      .then(
        () => {
          state.queue.shift();
          state.queueSize -= size;
          state.writing = false;
          done.resolve();
          updateReady(stream);
          processQueue(stream);
        },
        (error) => errorWritable(stream, error),
      );
    return;
  }
  if (state.state === "closing") {
    state.writing = true;
    Promise.resolve()
      .then(() => callSink(state.sink, "close"))
      .then(
        () => {
          state.state = "closed";
          state.closeRequest.resolve();
          if (state.writer) state.writer[kState].closed.resolve();
        },
        (error) => errorWritable(stream, error),
      );
  }
}

class WritableStream {
  constructor(sink = {}, strategy = {}) {
    this[kState] = {
      state: "writable",
      sink,
      queue: [],
      queueSize: 0,
      writing: false,
      started: false,
      writer: null,
      closeRequest: null,
      storedError: undefined,
      highWaterMark: strategy.highWaterMark === undefined ? 1 : strategy.highWaterMark,
      size: typeof strategy.size === "function" ? (chunk) => strategy.size(chunk) : () => 1,
    };
    this[kState].controller = new WritableStreamDefaultController(this);
    Promise.resolve()
      .then(() => callSink(sink, "start", this[kState].controller))
      .then(
        () => {
          this[kState].started = true;
          processQueue(this);
        },
        (error) => errorWritable(this, error),
      );
  }
  get locked() {
    return this[kState].writer !== null;
  }
  getWriter() {
    return new WritableStreamDefaultWriter(this);
  }
  abort(reason) {
    if (this.locked) return Promise.reject(typeError("WritableStream is locked"));
    return abortWritable(this, reason);
  }
  close() {
    if (this.locked) return Promise.reject(typeError("WritableStream is locked"));
    return closeWritable(this);
  }
  get [Symbol.toStringTag]() {
    return "WritableStream";
  }
}

function abortWritable(stream, reason) {
  const state = stream[kState];
  if (state.state === "closed" || state.state === "errored") return Promise.resolve();
  WritableStreamDefaultController.abort(state.controller, reason);
  errorWritable(stream, reason);
  return Promise.resolve(callSink(state.sink, "abort", reason)).then(() => undefined);
}

function closeWritable(stream) {
  const state = stream[kState];
  if (state.state !== "writable") return Promise.reject(typeError("WritableStream is closed or closing"));
  state.state = "closing";
  state.closeRequest = deferred();
  updateReady(stream);
  processQueue(stream);
  return state.closeRequest.promise;
}

class WritableStreamDefaultWriter {
  constructor(stream) {
    if (stream.locked) throw typeError("WritableStream is locked");
    this[kState] = { stream, closed: deferred(), ready: deferred(), readyPending: true };
    stream[kState].writer = this;
    if (stream[kState].state === "closed") this[kState].closed.resolve();
    updateReady(stream);
  }
  get closed() {
    return this[kState].closed.promise;
  }
  get ready() {
    return this[kState].ready.promise;
  }
  get desiredSize() {
    return this[kState].stream ? desiredSize(this[kState].stream[kState]) : null;
  }
  write(chunk) {
    const stream = this[kState].stream;
    if (!stream) return Promise.reject(typeError("The writer is not attached to a stream"));
    const state = stream[kState];
    if (state.state === "errored") return Promise.reject(state.storedError);
    if (state.state !== "writable") return Promise.reject(typeError("WritableStream is closed or closing"));
    const size = state.size(chunk);
    const done = deferred();
    state.queue.push({ chunk, size, done });
    state.queueSize += size;
    updateReady(stream);
    processQueue(stream);
    return done.promise;
  }
  close() {
    const stream = this[kState].stream;
    return stream ? closeWritable(stream) : Promise.reject(typeError("The writer is not attached to a stream"));
  }
  abort(reason) {
    const stream = this[kState].stream;
    return stream ? abortWritable(stream, reason) : Promise.reject(typeError("The writer is not attached to a stream"));
  }
  releaseLock() {
    const stream = this[kState].stream;
    if (!stream) return;
    stream[kState].writer = null;
    this[kState].stream = null;
  }
}

class TransformStreamDefaultController {
  #readable;
  #onError;
  constructor(readableController, onError) {
    this.#readable = readableController;
    this.#onError = onError;
  }
  get desiredSize() {
    return this.#readable().desiredSize;
  }
  enqueue(chunk) {
    this.#readable().enqueue(chunk);
  }
  error(reason) {
    this.#onError(reason);
  }
  terminate() {
    this.#readable().close();
  }
}

class TransformStream {
  constructor(transformer = {}, writableStrategy = {}, readableStrategy = { highWaterMark: 0 }) {
    const self = this;
    let readableController;
    let pulled = deferred();
    let pullRequested = false;
    const fail = (reason) => {
      readableController.error(reason);
      errorWritable(this.writable, reason);
    };
    const controller = new TransformStreamDefaultController(() => readableController, fail);
    this.readable = new ReadableStream({
      start(c) {
        readableController = c;
      },
      pull() {
        pullRequested = true;
        pulled.resolve();
      },
      cancel(reason) {
        errorWritable(self.writable, reason);
        return callSink(transformer, "cancel", reason);
      },
    }, readableStrategy);
    this.writable = new WritableStream({
      start: () => callSink(transformer, "start", controller),
      write: async (chunk) => {
        if (readableController.desiredSize <= 0 && !pullRequested) await pulled.promise;
        pullRequested = false;
        pulled = deferred();
        if (typeof transformer.transform === "function") await transformer.transform(chunk, controller);
        else controller.enqueue(chunk);
      },
      close: async () => {
        await callSink(transformer, "flush", controller);
        if (readableController.desiredSize !== null) readableController.close();
      },
      abort: (reason) => readableController.error(reason),
    }, writableStrategy);
  }
  get [Symbol.toStringTag]() {
    return "TransformStream";
  }
}

class ByteLengthQueuingStrategy {
  constructor({ highWaterMark }) {
    this.highWaterMark = highWaterMark;
  }
  size(chunk) {
    return chunk.byteLength;
  }
}

class CountQueuingStrategy {
  constructor({ highWaterMark }) {
    this.highWaterMark = highWaterMark;
  }
  size() {
    return 1;
  }
}

class TextEncoderStream extends TransformStream {
  constructor() {
    const encoder = new TextEncoder();
    super({
      transform(chunk, controller) {
        const bytes = encoder.encode(String(chunk));
        if (bytes.length) controller.enqueue(bytes);
      },
    });
  }
  get encoding() {
    return "utf-8";
  }
}

class TextDecoderStream extends TransformStream {
  #decoder;
  constructor(label = "utf-8", options = {}) {
    const decoder = new TextDecoder(label, options);
    super({
      transform(chunk, controller) {
        const text = decoder.decode(chunk, { stream: true });
        if (text) controller.enqueue(text);
      },
      flush(controller) {
        const text = decoder.decode();
        if (text) controller.enqueue(text);
      },
    });
    this.#decoder = decoder;
  }
  get encoding() {
    return this.#decoder.encoding;
  }
}

module.exports = {
  WritableStream,
  WritableStreamDefaultWriter,
  WritableStreamDefaultController,
  TransformStream,
  TransformStreamDefaultController,
  ByteLengthQueuingStrategy,
  CountQueuingStrategy,
  TextEncoderStream,
  TextDecoderStream,
};
