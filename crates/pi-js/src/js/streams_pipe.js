// internal/streams_pipe: pipe(), async iteration, Readable.from, and pipeline().
const common = require("internal/streams_common");
const { errors, once, eos, isNodeStream } = common;

function prependErrorListener(emitter, listener) {
  if (typeof emitter.prependListener === "function") emitter.prependListener("error", listener);
  else emitter.on("error", listener);
}

function pipe(dest, options) {
  const source = this;
  const state = this._readableState;
  state.pipes.push(dest);
  const doEnd = (!options || options.end !== false) && !dest._isStdio;
  let drainListener = null;
  let cleanedUp = false;

  function unpipe() {
    source.unpipe(dest);
  }
  function onend() {
    dest.end();
  }
  const endListener = doEnd ? onend : unpipe;
  function ondata(chunk) {
    if (dest.write(chunk) === false) pause();
  }
  function pause() {
    if (cleanedUp) return;
    source.pause();
    if (!drainListener) {
      drainListener = () => {
        dest.removeListener("drain", drainListener);
        drainListener = null;
        source.resume();
      };
      dest.on("drain", drainListener);
    }
  }
  function onerror(error) {
    unpipe();
    dest.removeListener("error", onerror);
    if (dest.listenerCount("error") === 0) {
      const destState = dest._writableState || dest._readableState;
      if (destState && !destState.errorEmitted) common.errorOrDestroy(dest, error);
      else dest.emit("error", error);
    }
  }
  function onclose() {
    dest.removeListener("finish", onfinish);
    unpipe();
  }
  function onfinish() {
    dest.removeListener("close", onclose);
    unpipe();
  }
  function onunpipe(readable, info) {
    if (readable === source && info && !info.hasUnpiped) {
      info.hasUnpiped = true;
      cleanup();
    }
  }
  function cleanup() {
    cleanedUp = true;
    dest.removeListener("close", onclose);
    dest.removeListener("finish", onfinish);
    if (drainListener) dest.removeListener("drain", drainListener);
    dest.removeListener("error", onerror);
    dest.removeListener("unpipe", onunpipe);
    source.removeListener("end", onend);
    source.removeListener("end", unpipe);
    source.removeListener("data", ondata);
  }

  if (state.endEmitted) process.nextTick(endListener);
  else source.once("end", endListener);
  dest.on("unpipe", onunpipe);
  source.on("data", ondata);
  prependErrorListener(dest, onerror);
  dest.once("close", onclose);
  dest.once("finish", onfinish);
  dest.emit("pipe", source);
  if (dest.writableNeedDrain === true) pause();
  else if (!state.flowing) source.resume();
  return dest;
}

async function* iterate(stream, options = {}) {
  let wake = null;
  const notify = () => {
    if (wake) {
      const resolve = wake;
      wake = null;
      resolve();
    }
  };
  stream.on("readable", notify);
  // undefined while running, null after a clean end, or the error.
  let error;
  const cleanup = eos(stream, { writable: false }, (failure) => {
    error = failure || null;
    notify();
  });
  try {
    for (;;) {
      const chunk = stream.destroyed ? null : stream.read();
      if (chunk !== null) yield chunk;
      else if (error) throw error;
      else if (error === null) return;
      else await new Promise((resolve) => (wake = resolve));
    }
  } catch (failure) {
    error = failure;
    throw failure;
  } finally {
    const destroyOnReturn = options.destroyOnReturn !== false;
    if ((error || destroyOnReturn) && (error === undefined || stream._readableState.autoDestroy)) {
      stream.destroy(null);
    } else {
      stream.removeListener("readable", notify);
      cleanup();
    }
  }
}

function webIterator(stream) {
  const reader = stream.getReader();
  return {
    next: () => reader.read(),
    async return() {
      await reader.cancel();
      return { done: true, value: undefined };
    },
  };
}

function from(Readable, iterable, options) {
  if (typeof iterable === "string" || iterable instanceof Uint8Array) {
    return new Readable({
      objectMode: true,
      ...options,
      read() {
        this.push(iterable);
        this.push(null);
      },
    });
  }
  let iterator;
  let isAsync = true;
  if (iterable && typeof iterable.getReader === "function") {
    iterator = webIterator(iterable);
    options = { objectMode: false, ...options };
  } else if (iterable && typeof iterable[Symbol.asyncIterator] === "function") {
    iterator = iterable[Symbol.asyncIterator]();
  } else if (iterable && typeof iterable[Symbol.iterator] === "function") {
    iterator = iterable[Symbol.iterator]();
    isAsync = false;
  } else {
    throw errors.invalidArg("iterable", "an Iterable", iterable);
  }
  const readable = new Readable({ objectMode: true, highWaterMark: 1, ...options });
  let reading = false;
  readable._read = function read() {
    if (!reading) {
      reading = true;
      pull();
    }
  };
  readable._destroy = function destroy(error, callback) {
    close(error).then(
      () => process.nextTick(callback, error),
      (failure) => process.nextTick(callback, failure || error),
    );
  };
  async function close(error) {
    if (error !== undefined && error !== null && typeof iterator.throw === "function") {
      const { done } = await iterator.throw(error);
      if (done) return;
    }
    if (typeof iterator.return === "function") await iterator.return();
  }
  async function pull() {
    for (;;) {
      try {
        const { value, done } = isAsync ? await iterator.next() : iterator.next();
        if (done) {
          readable.push(null);
        } else {
          const item = value && typeof value.then === "function" ? await value : value;
          if (item === null) {
            reading = false;
            throw errors.nullValues();
          }
          if (readable.push(item)) continue;
          reading = false;
        }
      } catch (error) {
        readable.destroy(error);
      }
      break;
    }
  }
  return readable;
}

function isIterable(value) {
  return !!value && (typeof value[Symbol.asyncIterator] === "function" || typeof value[Symbol.iterator] === "function");
}

function waitForDrain(writable) {
  return new Promise((resolve) => {
    const done = () => {
      writable.removeListener("drain", done);
      writable.removeListener("close", done);
      resolve();
    };
    writable.on("drain", done);
    writable.on("close", done);
  });
}

async function pump(iterable, writable, end) {
  for await (const chunk of iterable) {
    if (writable.destroyed) return;
    if (!writable.write(chunk)) await waitForDrain(writable);
  }
  if (end) writable.end();
}

async function drain(iterable) {
  let last;
  for await (const item of iterable) last = item;
  return last;
}

function pipelineImpl(stages, callback, options = {}) {
  if (stages.length === 1 && Array.isArray(stages[0])) stages = stages[0];
  if (stages.length < 2) throw errors.missingArgs("streams");
  callback = once(callback);
  const controller = new AbortController();
  const nodeStreams = [];
  let settled = false;
  function done(error, value) {
    if (settled) return;
    settled = true;
    if (error) {
      controller.abort(error);
      for (const stream of nodeStreams) if (!stream.destroyed) stream.destroy(error);
    }
    process.nextTick(callback, error || undefined, value);
  }
  if (options.signal) {
    const onabort = () => done(errors.abort(options.signal.reason));
    if (options.signal.aborted) process.nextTick(onabort);
    else options.signal.addEventListener("abort", onabort, { once: true });
  }
  let current;
  stages.forEach((stage, index) => {
    const isFirst = index === 0;
    const isLast = index === stages.length - 1;
    if (typeof stage === "function") {
      const result = isFirst ? stage({ signal: controller.signal }) : stage(current, { signal: controller.signal });
      if (isLast) {
        const settle = result && typeof result.then === "function" ? result : drain(result);
        settle.then((value) => done(null, value), done);
      }
      current = result;
      return;
    }
    if (isNodeStream(stage)) {
      nodeStreams.push(stage);
      eos(stage, { readable: !isLast, writable: !isFirst }, (error) => {
        if (error) done(error);
        else if (isLast) done(null);
      });
      if (!isFirst) {
        const end = !isLast || options.end !== false;
        if (isNodeStream(current) && typeof current.pipe === "function") current.pipe(stage, { end });
        else pump(current, stage, end).catch(done);
      }
      current = stage;
      return;
    }
    if (isFirst && isIterable(stage)) {
      current = stage;
      return;
    }
    throw errors.invalidArg(`streams[${index}]`, "a Stream, Iterable, AsyncIterable, or Function", stage);
  });
  return current;
}

function pipeline(...stages) {
  const callback = stages.pop();
  if (typeof callback !== "function") throw errors.invalidArg("callback", "of type function", callback);
  return pipelineImpl(stages, callback);
}

function pipelinePromise(...stages) {
  let options = {};
  const last = stages[stages.length - 1];
  if (last && typeof last === "object" && !isNodeStream(last) && !isIterable(last)) options = stages.pop();
  return new Promise((resolve, reject) => {
    pipelineImpl(stages, (error, value) => (error ? reject(error) : resolve(value)), options);
  });
}

function finishedPromise(stream, options) {
  return new Promise((resolve, reject) => {
    eos(stream, options, (error) => (error ? reject(error) : resolve()));
  });
}

module.exports = { pipe, iterate, from, pipeline, pipelinePromise, finishedPromise };
