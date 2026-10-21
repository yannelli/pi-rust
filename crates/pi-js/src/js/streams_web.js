// internal/streams_web: WHATWG ReadableStream, WritableStream, TransformStream, and helpers.
// Byte stream controllers and BYOB readers are in internal/streams_web_bytes.
const kState = Symbol("kState");
const bytes = () => require("internal/streams_web_bytes");

function typeError(message, code = "ERR_INVALID_STATE") {
  return Object.assign(new TypeError(message), { code });
}

function deferred() {
  let resolve;
  let reject;
  const promise = new Promise((ok, fail) => {
    resolve = ok;
    reject = fail;
  });
  return { promise, resolve, reject };
}

function sizeOf(strategy) {
  return typeof strategy.size === "function" ? (chunk) => strategy.size(chunk) : () => 1;
}

class ReadableStreamDefaultController {
  constructor(stream) {
    this[kState] = stream;
  }
  get desiredSize() {
    const state = this[kState][kState];
    if (state.state === "errored") return null;
    if (state.state === "closed") return 0;
    return state.highWaterMark - state.queueSize;
  }
  enqueue(chunk) {
    const state = this[kState][kState];
    if (state.closeRequested || state.state !== "readable") throw typeError("Controller is already closed");
    const request = state.reader && state.reader[kState].requests.shift();
    if (request) request.resolve({ value: chunk, done: false });
    else {
      const size = state.size(chunk);
      state.queue.push({ chunk, size });
      state.queueSize += size;
    }
    pullIfNeeded(this[kState]);
  }
  close() {
    const state = this[kState][kState];
    if (state.closeRequested || state.state !== "readable") throw typeError("Controller is already closed");
    state.closeRequested = true;
    if (state.queue.length === 0) finishClose(this[kState]);
  }
  error(reason) {
    errorStream(this[kState], reason);
  }
}

// Pending BYOB reads stay pending on close (the source answers them with respond(0)), but a
// cancel ends them.
function finishClose(stream, cancelled = false) {
  const state = stream[kState];
  if (state.state !== "readable") return;
  state.state = "closed";
  const reader = state.reader;
  if (!reader) return;
  if (cancelled || !reader[kState].byob) {
    for (const request of reader[kState].requests.splice(0)) request.resolve({ value: undefined, done: true });
  }
  reader[kState].closed.resolve();
}

function errorStream(stream, reason) {
  const state = stream[kState];
  if (state.state !== "readable") return;
  state.state = "errored";
  state.storedError = reason;
  state.queue = [];
  state.queueSize = 0;
  if (state.bytes) bytes().hooks.clear(state.controller);
  const reader = state.reader;
  if (!reader) return;
  for (const request of reader[kState].requests.splice(0)) request.reject(reason);
  rejectClosed(reader, reason);
}

// The spec marks a reader's closed promise as handled, so its rejection is not reported.
function rejectClosed(reader, reason) {
  reader[kState].closed.reject(reason);
  reader[kState].closed.promise.catch(() => {});
}

function callSource(source, method, argument) {
  if (typeof source[method] === "function") return source[method].call(source, argument);
  return undefined;
}

function pullIfNeeded(stream) {
  const state = stream[kState];
  if (!state.started || state.closeRequested || state.state !== "readable") return;
  const waiting = state.reader && state.reader[kState].requests.length > 0;
  if (!waiting && state.highWaterMark - state.queueSize <= 0) return;
  if (state.pulling) {
    state.pullAgain = true;
    return;
  }
  state.pulling = true;
  let pulled;
  try {
    pulled = Promise.resolve(callSource(state.source, "pull", state.controller));
  } catch (error) {
    pulled = Promise.reject(error);
  }
  pulled
    .then(
      () => {
        state.pulling = false;
        if (state.pullAgain) {
          state.pullAgain = false;
          pullIfNeeded(stream);
        }
      },
      (error) => errorStream(stream, error),
    );
}

function defaultHighWaterMark(source, strategy) {
  if (strategy.highWaterMark !== undefined) return strategy.highWaterMark;
  return source.type === "bytes" ? 0 : 1;
}

class ReadableStream {
  constructor(source = {}, strategy = {}) {
    if (source === null) throw new TypeError("The underlying source must be an object");
    this[kState] = {
      state: "readable",
      queue: [],
      queueSize: 0,
      source,
      reader: null,
      started: false,
      pulling: false,
      pullAgain: false,
      closeRequested: false,
      disturbed: false,
      storedError: undefined,
      highWaterMark: defaultHighWaterMark(source, strategy),
      size: sizeOf(strategy),
      bytes: source.type === "bytes",
    };
    const controller = this[kState].bytes ? bytes().hooks.create(this, source) : new ReadableStreamDefaultController(this);
    this[kState].controller = controller;
    Promise.resolve(callSource(source, "start", controller))
      .then(
        () => {
          this[kState].started = true;
          pullIfNeeded(this);
        },
        (error) => errorStream(this, error),
      );
  }
  static from(iterable) {
    const iterator = iterable[Symbol.asyncIterator] ? iterable[Symbol.asyncIterator]() : iterable[Symbol.iterator]();
    return new ReadableStream({
      async pull(controller) {
        const { value, done } = await iterator.next();
        if (done) controller.close();
        else controller.enqueue(value);
      },
      async cancel(reason) {
        if (typeof iterator.return === "function") await iterator.return(reason);
      },
    }, { highWaterMark: 0 });
  }
  get locked() {
    return this[kState].reader !== null;
  }
  getReader(options = {}) {
    if (options.mode === undefined) return new ReadableStreamDefaultReader(this);
    if (`${options.mode}` === "byob") return new (bytes().ReadableStreamBYOBReader)(this);
    throw require("internal/errors").invalidArgValue("options.mode", options.mode);
  }
  cancel(reason) {
    if (this.locked) return Promise.reject(typeError("ReadableStream is locked"));
    return cancelStream(this, reason);
  }
  tee() {
    const reader = this.getReader();
    const branches = [];
    let reading = false;
    const pull = async () => {
      if (reading) return;
      reading = true;
      let result;
      try {
        result = await reader.read();
      } catch (error) {
        for (const branch of branches) branch.error(error);
        return;
      } finally {
        reading = false;
      }
      const { value, done } = result;
      for (const branch of branches) {
        if (done) {
          if (branch.desiredSize !== null) branch.close();
        } else if (branch.desiredSize !== null) branch.enqueue(value);
      }
    };
    const make = () => new ReadableStream({
      start(controller) {
        branches.push(controller);
      },
      pull,
      cancel() {},
    });
    return [make(), make()];
  }
  async pipeTo(destination, options = {}) {
    const reader = this.getReader();
    const writer = destination.getWriter();
    const { signal, preventClose, preventAbort, preventCancel } = options;
    let aborted = null;
    const onabort = () => {
      aborted = signal.reason;
      reader.cancel(aborted).catch(() => {});
    };
    if (signal) {
      if (signal.aborted) onabort();
      else signal.addEventListener("abort", onabort, { once: true });
    }
    try {
      for (;;) {
        if (aborted !== null) throw aborted;
        const { value, done } = await reader.read();
        if (done) break;
        await writer.ready;
        await writer.write(value);
      }
      if (aborted !== null) throw aborted;
      if (!preventClose) await writer.close();
    } catch (error) {
      if (!preventAbort) await writer.abort(error).catch(() => {});
      if (!preventCancel) await reader.cancel(error).catch(() => {});
      throw error;
    } finally {
      if (signal) signal.removeEventListener("abort", onabort);
      reader.releaseLock();
      writer.releaseLock();
    }
  }
  pipeThrough(transform, options) {
    // Failures reach the caller through transform.readable, so this promise is marked handled.
    this.pipeTo(transform.writable, options).catch(() => {});
    return transform.readable;
  }
  values(options = {}) {
    const reader = this.getReader();
    // The spec's "is finished" flag: QuickJS calls return() after next() rejects in for await.
    let finished = false;
    return {
      async next() {
        if (finished) return { value: undefined, done: true };
        try {
          const result = await reader.read();
          if (result.done) {
            finished = true;
            reader.releaseLock();
          }
          return result;
        } catch (error) {
          finished = true;
          reader.releaseLock();
          throw error;
        }
      },
      async return(value) {
        if (finished) return { value, done: true };
        finished = true;
        if (!options.preventCancel) await reader.cancel(value);
        reader.releaseLock();
        return { value, done: true };
      },
      [Symbol.asyncIterator]() {
        return this;
      },
    };
  }
  [Symbol.asyncIterator](options) {
    return this.values(options);
  }
  get [Symbol.toStringTag]() {
    return "ReadableStream";
  }
}

function cancelStream(stream, reason) {
  const state = stream[kState];
  state.disturbed = true;
  if (state.state === "closed") return Promise.resolve();
  if (state.state === "errored") return Promise.reject(state.storedError);
  state.queue = [];
  state.queueSize = 0;
  if (state.bytes) bytes().hooks.clear(state.controller);
  finishClose(stream, true);
  return Promise.resolve(callSource(state.source, "cancel", reason)).then(() => undefined);
}

class ReadableStreamDefaultReader {
  constructor(stream) {
    if (stream.locked) throw typeError("ReadableStream is locked");
    this[kState] = { stream, requests: [], closed: deferred() };
    stream[kState].reader = this;
    const state = stream[kState].state;
    if (state === "closed") this[kState].closed.resolve();
    if (state === "errored") rejectClosed(this, stream[kState].storedError);
  }
  get closed() {
    return this[kState].closed.promise;
  }
  read() {
    const stream = this[kState].stream;
    if (!stream) return Promise.reject(typeError("The reader is not attached to a stream"));
    const state = stream[kState];
    state.disturbed = true;
    if (state.queue.length) {
      const { chunk, size } = state.queue.shift();
      state.queueSize -= size;
      if (state.closeRequested && state.queue.length === 0) finishClose(stream);
      else pullIfNeeded(stream);
      return Promise.resolve({ value: chunk, done: false });
    }
    if (state.state === "closed") return Promise.resolve({ value: undefined, done: true });
    if (state.state === "errored") return Promise.reject(state.storedError);
    if (state.bytes) bytes().hooks.pullSteps(state.controller);
    const request = deferred();
    this[kState].requests.push(request);
    pullIfNeeded(stream);
    return request.promise;
  }
  cancel(reason) {
    const stream = this[kState].stream;
    if (!stream) return Promise.reject(typeError("The reader is not attached to a stream"));
    return cancelStream(stream, reason);
  }
  releaseLock() {
    const stream = this[kState].stream;
    if (!stream) return;
    const error = typeError("Reader was released");
    for (const request of this[kState].requests.splice(0)) request.reject(error);
    if (stream[kState].bytes) bytes().hooks.release(stream[kState].controller);
    stream[kState].reader = null;
    this[kState].stream = null;
  }
}

module.exports = {
  ReadableStream,
  ReadableStreamDefaultReader,
  ReadableStreamDefaultController,
  kState,
  deferred,
  typeError,
  finishClose,
  errorStream,
  pullIfNeeded,
  cancelStream,
  rejectClosed,
};
