// internal/streams_web_bytes: readable byte streams (ReadableByteStreamController), BYOB requests,
// and ReadableStreamBYOBReader, following the WHATWG Streams spec.
const web = require("internal/streams_web");
const errors = require("internal/errors");
const { kState, deferred } = web;

const stateError = (message) => errors.codeError(TypeError, "ERR_INVALID_STATE", `Invalid state: ${message}`);

function enqueueChunk(stream, buffer, byteOffset, byteLength) {
  const state = stream[kState];
  state.queue.push({ chunk: new Uint8Array(buffer, byteOffset, byteLength), size: byteLength });
  state.queueSize += byteLength;
}

function invalidateRequest(controller) {
  const request = controller[kState].byobRequest;
  if (!request) return;
  request[kState].controller = null;
  request[kState].view = null;
  controller[kState].byobRequest = null;
}

function clearPullIntos(controller) {
  invalidateRequest(controller);
  controller[kState].pullIntos = [];
}

function readerOf(stream) {
  return stream[kState].reader;
}

// Copy queued bytes into a pull-into descriptor; true once it holds its minimum fill.
function fillFromQueue(stream, desc) {
  const state = stream[kState];
  const maxCopy = Math.min(state.queueSize, desc.byteLength - desc.bytesFilled);
  const maxFilled = desc.bytesFilled + maxCopy;
  const aligned = maxFilled - (maxFilled % desc.elementSize);
  let remaining = maxCopy;
  let ready = false;
  if (aligned >= desc.minimumFill) {
    remaining = aligned - desc.bytesFilled;
    ready = true;
  }
  while (remaining > 0) {
    const head = state.queue[0];
    const count = Math.min(remaining, head.size);
    new Uint8Array(desc.buffer, desc.byteOffset + desc.bytesFilled, count).set(head.chunk.subarray(0, count));
    if (head.size === count) state.queue.shift();
    else {
      head.chunk = head.chunk.subarray(count);
      head.size -= count;
    }
    state.queueSize -= count;
    desc.bytesFilled += count;
    remaining -= count;
  }
  return ready;
}

function convert(desc) {
  const length = desc.viewConstructor === DataView ? desc.bytesFilled : desc.bytesFilled / desc.elementSize;
  return new desc.viewConstructor(desc.buffer, desc.byteOffset, length);
}

function commit(stream, desc) {
  const done = stream[kState].state === "closed";
  const reader = readerOf(stream);
  const request = reader && reader[kState].requests.shift();
  if (!request) return;
  if (desc.readerType === "default") request.resolve(done ? { value: undefined, done } : { value: convert(desc), done });
  else request.resolve({ value: convert(desc), done });
}

function handleQueueDrain(stream) {
  const state = stream[kState];
  if (state.queueSize === 0 && state.closeRequested) web.finishClose(stream);
  else web.pullIfNeeded(stream);
}

function processPullIntos(controller) {
  const { stream, pullIntos } = controller[kState];
  while (pullIntos.length > 0 && stream[kState].queueSize > 0) {
    const first = pullIntos[0];
    if (!fillFromQueue(stream, first)) break;
    pullIntos.shift();
    commit(stream, first);
  }
  if (stream[kState].closeRequested && stream[kState].queueSize === 0) web.finishClose(stream);
}

function enqueueDetached(controller, desc) {
  if (desc.bytesFilled > 0) {
    const copy = desc.buffer.slice(desc.byteOffset, desc.byteOffset + desc.bytesFilled);
    enqueueChunk(controller[kState].stream, copy, 0, copy.byteLength);
  }
  controller[kState].pullIntos.shift();
}

function respondInternal(controller, bytesWritten) {
  const { stream, pullIntos } = controller[kState];
  const first = pullIntos[0];
  invalidateRequest(controller);
  if (stream[kState].state === "closed") {
    if (first.readerType === "none") pullIntos.shift();
    const reader = readerOf(stream);
    if (reader && reader[kState].byob) while (reader[kState].requests.length > 0 && pullIntos.length) commit(stream, pullIntos.shift());
  } else {
    first.bytesFilled += bytesWritten;
    if (first.readerType === "none") {
      enqueueDetached(controller, first);
      processPullIntos(controller);
    } else if (first.bytesFilled >= first.minimumFill) {
      pullIntos.shift();
      const remainder = first.bytesFilled % first.elementSize;
      if (remainder > 0) {
        const end = first.byteOffset + first.bytesFilled;
        enqueueChunk(stream, first.buffer.slice(end - remainder, end), 0, remainder);
      }
      first.bytesFilled -= remainder;
      commit(stream, first);
      processPullIntos(controller);
    }
  }
  web.pullIfNeeded(stream);
}

class ReadableStreamBYOBRequest {
  constructor(controller, view) {
    this[kState] = { controller, view };
  }
  get view() {
    return this[kState].view;
  }
  respond(bytesWritten) {
    const { controller } = this[kState];
    if (!controller) throw stateError("This BYOB request has been invalidated");
    errors.validate.integer(bytesWritten, "bytesWritten", 0);
    const first = controller[kState].pullIntos[0];
    const closed = controller[kState].stream[kState].state === "closed";
    if (closed && bytesWritten !== 0) throw errors.invalidArgValue("bytesWritten", bytesWritten, "must be 0 when the stream is closed");
    if (!closed && bytesWritten === 0) throw errors.invalidArgValue("bytesWritten", bytesWritten, "must be greater than 0");
    if (first.bytesFilled + bytesWritten > first.byteLength) {
      throw Object.assign(new RangeError(`The argument 'bytesWritten' is invalid. Received ${bytesWritten}`), { code: "ERR_INVALID_ARG_VALUE" });
    }
    first.buffer = first.buffer.transfer();
    respondInternal(controller, bytesWritten);
  }
  respondWithNewView(view) {
    const { controller } = this[kState];
    if (!controller) throw stateError("This BYOB request has been invalidated");
    if (!ArrayBuffer.isView(view)) throw errors.invalidArgType("view", "an instance of Buffer, TypedArray, or DataView", view);
    const first = controller[kState].pullIntos[0];
    const closed = controller[kState].stream[kState].state === "closed";
    const range = (reason) => Object.assign(new RangeError(`Invalid state: ${reason}`), { code: "ERR_INVALID_STATE" });
    if (closed !== (view.byteLength === 0)) throw stateError(closed ? "View is not zero-length" : "View is zero-length");
    if (first.byteOffset + first.bytesFilled !== view.byteOffset) throw range("The region specified by view does not match byobRequest");
    if (first.bufferByteLength !== view.buffer.byteLength) throw range("The buffer of view has different capacity than byobRequest");
    if (first.bytesFilled + view.byteLength > first.byteLength) throw range("The region specified by view is larger than byobRequest");
    const length = view.byteLength;
    first.buffer = view.buffer.transfer();
    respondInternal(controller, length);
  }
  get [Symbol.toStringTag]() {
    return "ReadableStreamBYOBRequest";
  }
}

class ReadableByteStreamController {
  constructor(token, stream, autoAllocateChunkSize) {
    if (token !== kState) throw errors.codeError(TypeError, "ERR_ILLEGAL_CONSTRUCTOR", "Illegal constructor");
    this[kState] = { stream, autoAllocateChunkSize, pullIntos: [], byobRequest: null };
  }
  get byobRequest() {
    const state = this[kState];
    if (state.byobRequest === null && state.pullIntos.length > 0) {
      const first = state.pullIntos[0];
      const view = new Uint8Array(first.buffer, first.byteOffset + first.bytesFilled, first.byteLength - first.bytesFilled);
      state.byobRequest = new ReadableStreamBYOBRequest(this, view);
    }
    return state.byobRequest;
  }
  get desiredSize() {
    const state = this[kState].stream[kState];
    if (state.state === "errored") return null;
    if (state.state === "closed") return 0;
    return state.highWaterMark - state.queueSize;
  }
  close() {
    const { stream, pullIntos } = this[kState];
    const state = stream[kState];
    if (state.closeRequested) throw stateError("Controller is already closed");
    if (state.state !== "readable") throw stateError("ReadableStream is already closed");
    if (state.queueSize > 0) {
      state.closeRequested = true;
      return;
    }
    const first = pullIntos[0];
    if (first && first.bytesFilled % first.elementSize !== 0) {
      const error = stateError("Partial read");
      web.errorStream(stream, error);
      throw error;
    }
    state.closeRequested = true;
    web.finishClose(stream);
  }
  enqueue(chunk) {
    if (!ArrayBuffer.isView(chunk)) throw errors.invalidArgType("buffer", "an instance of Buffer, TypedArray, or DataView", chunk);
    if (chunk.byteLength === 0 || chunk.buffer.byteLength === 0) throw stateError("chunk ArrayBuffer is zero-length or detached");
    const { stream, pullIntos } = this[kState];
    const state = stream[kState];
    if (state.closeRequested) throw stateError("Controller is already closed");
    if (state.state !== "readable") throw stateError("ReadableStream is already closed");
    const { byteOffset, byteLength } = chunk;
    const buffer = chunk.buffer.transfer();
    if (pullIntos.length > 0) {
      invalidateRequest(this);
      pullIntos[0].buffer = pullIntos[0].buffer.transfer();
      if (pullIntos[0].readerType === "none") enqueueDetached(this, pullIntos[0]);
    }
    const reader = readerOf(stream);
    if (reader && !reader[kState].byob && reader[kState].requests.length > 0) {
      if (pullIntos.length > 0) pullIntos.shift();
      reader[kState].requests.shift().resolve({ value: new Uint8Array(buffer, byteOffset, byteLength), done: false });
    } else {
      enqueueChunk(stream, buffer, byteOffset, byteLength);
      if (reader && reader[kState].byob) processPullIntos(this);
    }
    web.pullIfNeeded(stream);
  }
  error(reason) {
    web.errorStream(this[kState].stream, reason);
  }
  get [Symbol.toStringTag]() {
    return "ReadableByteStreamController";
  }
}

// Hooks streams_web calls for byte streams.
const hooks = {
  create(stream, source) {
    const size = source.autoAllocateChunkSize;
    if (size !== undefined && (!Number.isInteger(size) || size <= 0)) {
      throw errors.codeError(TypeError, "ERR_INVALID_ARG_VALUE", "The property 'source.autoAllocateChunkSize' must be a positive integer");
    }
    return new ReadableByteStreamController(kState, stream, size);
  },
  // A default reader read with an empty queue: allocate a buffer for the source to fill.
  pullSteps(controller) {
    const state = controller[kState];
    if (state.autoAllocateChunkSize === undefined) return;
    const buffer = new ArrayBuffer(state.autoAllocateChunkSize);
    state.pullIntos.push({ buffer, bufferByteLength: buffer.byteLength, byteOffset: 0, byteLength: buffer.byteLength, bytesFilled: 0, minimumFill: 1, elementSize: 1, viewConstructor: Uint8Array, readerType: "default" });
  },
  release(controller) {
    const state = controller[kState];
    if (state.pullIntos.length === 0) return;
    state.pullIntos[0].readerType = "none";
    state.pullIntos = [state.pullIntos[0]];
  },
  clear: clearPullIntos,
};

class ReadableStreamBYOBReader {
  constructor(stream) {
    if (!(stream instanceof web.ReadableStream)) throw errors.invalidArgType("stream", "an instance of ReadableStream", stream);
    if (!(stream[kState].controller instanceof ReadableByteStreamController)) {
      const shown = `ReadableStream { locked: ${stream.locked}, state: '${stream[kState].state}', supportsBYOB: false }`;
      throw errors.codeError(TypeError, "ERR_INVALID_ARG_VALUE", `The argument 'stream' must be a byte stream. Received ${shown}`);
    }
    if (stream.locked) throw stateError("ReadableStream is locked");
    this[kState] = { stream, requests: [], closed: deferred(), byob: true };
    stream[kState].reader = this;
    const state = stream[kState].state;
    if (state === "closed") this[kState].closed.resolve();
    if (state === "errored") web.rejectClosed(this, stream[kState].storedError);
  }
  get closed() {
    return this[kState].closed.promise;
  }
  read(view, options = {}) {
    if (!ArrayBuffer.isView(view)) return Promise.reject(errors.invalidArgType("view", "an instance of Buffer, TypedArray, or DataView", view));
    if (view.byteLength === 0 || view.buffer.byteLength === 0) return Promise.reject(stateError("View or Viewed ArrayBuffer is zero-length or detached"));
    const min = options.min === undefined ? 1 : options.min;
    if (min === 0) return Promise.reject(errors.invalidArgValue("options.min", min, "must be greater than 0"));
    const isData = view instanceof DataView;
    const limit = isData ? view.byteLength : view.length;
    if (min > limit) return Promise.reject(errors.outOfRange("options.min", `<= ${isData ? "view.byteLength" : "view.length"}`, min));
    const stream = this[kState].stream;
    if (!stream) return Promise.reject(stateError("The reader is not attached to a stream"));
    const state = stream[kState];
    state.disturbed = true;
    if (state.state === "errored") return Promise.reject(state.storedError);
    const elementSize = view.BYTES_PER_ELEMENT || 1;
    const { byteOffset, byteLength } = view;
    const buffer = view.buffer.transfer();
    const desc = {
      buffer,
      bufferByteLength: buffer.byteLength,
      byteOffset,
      byteLength,
      bytesFilled: 0,
      minimumFill: min * elementSize,
      elementSize,
      viewConstructor: view.constructor,
      readerType: "byob",
    };
    const request = deferred();
    const controller = state.controller;
    const pullIntos = controller[kState].pullIntos;
    if (pullIntos.length > 0) {
      pullIntos.push(desc);
      this[kState].requests.push(request);
      return request.promise;
    }
    if (state.state === "closed") return Promise.resolve({ value: new desc.viewConstructor(desc.buffer, desc.byteOffset, 0), done: true });
    if (state.queueSize > 0) {
      if (fillFromQueue(stream, desc)) {
        const value = convert(desc);
        handleQueueDrain(stream);
        return Promise.resolve({ value, done: false });
      }
      if (state.closeRequested) {
        const error = stateError("Partial read");
        web.errorStream(stream, error);
        return Promise.reject(error);
      }
    }
    pullIntos.push(desc);
    this[kState].requests.push(request);
    web.pullIfNeeded(stream);
    return request.promise;
  }
  cancel(reason) {
    const stream = this[kState].stream;
    if (!stream) return Promise.reject(stateError("The reader is not attached to a stream"));
    return web.cancelStream(stream, reason);
  }
  releaseLock() {
    const stream = this[kState].stream;
    if (!stream) return;
    const error = stateError("Releasing reader");
    for (const request of this[kState].requests.splice(0)) request.reject(error);
    if (stream[kState].state === "readable") web.rejectClosed(this, stateError("Reader released"));
    hooks.release(stream[kState].controller);
    stream[kState].reader = null;
    this[kState].stream = null;
  }
  get [Symbol.toStringTag]() {
    return "ReadableStreamBYOBReader";
  }
}

module.exports = { ReadableByteStreamController, ReadableStreamBYOBRequest, ReadableStreamBYOBReader, hooks };
