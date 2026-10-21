// internal/streams_duplexify: Duplex.from for every body Node accepts, plus Writable.fromWeb and
// Duplex.fromWeb, following lib/internal/streams/duplexify.js.
const { Readable } = require("internal/streams_readable");
const { Writable, Duplex } = require("internal/streams_writable");
const errors = require("internal/errors");

const isStream = (value) => value && typeof value === "object" && typeof value.on === "function" && (typeof value.pipe === "function" || typeof value.write === "function");
const isWebReadable = (value) => value instanceof require("internal/streams_web").ReadableStream;
const isWebWritable = (value) => value instanceof require("internal/streams_web_writable").WritableStream;
const isIterable = (value) => value && (typeof value[Symbol.asyncIterator] === "function" || typeof value[Symbol.iterator] === "function");

// One Duplex over two streams: writes go to `head`, reads come from `tail`.
function duplexify(head, tail) {
  let onFinal = null;
  let finished = false;
  const duplex = new Duplex({
    writableObjectMode: !!(head && head.writableObjectMode),
    readableObjectMode: !!(tail && tail.readableObjectMode),
    readable: !!tail,
    writable: !!head,
    write(chunk, encoding, callback) {
      if (head.write(chunk, encoding)) callback();
      else head.once("drain", () => callback());
    },
    final(callback) {
      head.end();
      if (finished) callback();
      else onFinal = callback;
    },
    read() {
      if (tail && tail.isPaused()) tail.resume();
    },
    destroy(error, callback) {
      for (const stream of [head, tail]) if (stream && !stream.destroyed) stream.destroy(error);
      callback(error);
    },
  });
  if (tail) {
    tail.on("data", (chunk) => {
      if (!duplex.push(chunk)) tail.pause();
    });
    tail.on("end", () => duplex.push(null));
  }
  const finish = (error) => {
    finished = true;
    if (error) duplex.destroy(error);
    else if (onFinal) onFinal();
    onFinal = null;
  };
  return { duplex, finish };
}

// The Duplex for a readable side, a writable side, or both; it finishes when the readable side
// ends, or when the writable side finishes if there is no readable side.
function pair(readable, writable) {
  const { duplex, finish } = duplexify(writable || null, readable || null);
  if (readable) readable.on("end", () => finish());
  else writable.on("finish", () => finish());
  for (const stream of [readable, writable]) if (stream) stream.on("error", (error) => duplex.destroy(error));
  return duplex;
}

// The source handed to a Duplex.from function: each written chunk, one at a time.
function fromAsyncGen(fn) {
  const deferred = () => {
    let resolve;
    const promise = new Promise((done) => (resolve = done));
    return { promise, resolve };
  };
  let next = deferred();
  const controller = new AbortController();
  const signal = controller.signal;
  const source = (async function* source() {
    for (;;) {
      const { chunk, done, callback } = await next.promise;
      process.nextTick(callback);
      if (done) return;
      if (signal.aborted) throw Object.assign(new Error("The operation was aborted", { cause: signal.reason }), { name: "AbortError", code: "ABORT_ERR" });
      next = deferred();
      yield chunk;
    }
  })();
  return {
    value: fn(source, { signal }),
    write(chunk, encoding, callback) {
      next.resolve({ chunk, done: false, callback });
    },
    final(callback) {
      next.resolve({ done: true, callback });
    },
    destroy(error, callback) {
      controller.abort();
      callback(error);
    },
  };
}

function fromFunction(body) {
  const { value, write, final, destroy } = fromAsyncGen(body);
  if (value instanceof Duplex) return value;
  if (isIterable(value)) {
    return require("internal/streams_pipe").from(Duplex, value, { objectMode: true, write, final, destroy });
  }
  if (value && typeof value.then === "function") {
    let duplex;
    const promise = value.then((result) => {
      if (result !== null && result !== undefined) {
        throw errors.codeError(TypeError, "ERR_INVALID_RETURN_VALUE", `Expected nully to be returned from the "body" function but got ${errors.describe(result)}.`);
      }
    });
    promise.catch((error) => duplex.destroy(error));
    duplex = new Duplex({
      objectMode: true,
      readable: false,
      write,
      final(callback) {
        final(() => promise.then(() => process.nextTick(callback, null), (error) => process.nextTick(callback, error)));
      },
      destroy,
    });
    return duplex;
  }
  throw errors.codeError(TypeError, "ERR_INVALID_RETURN_VALUE", `Expected Iterable, AsyncIterable or AsyncFunction to be returned from the "body" function but got ${errors.describe(value)}.`);
}

function fromPromise(body) {
  const duplex = new Duplex({ objectMode: true, writable: false, read() {} });
  body.then(
    (value) => {
      if (value !== null && value !== undefined) duplex.push(value);
      duplex.push(null);
    },
    (error) => duplex.destroy(error),
  );
  return duplex;
}

function from(body) {
  if (body instanceof Duplex) return body;
  if (isStream(body)) return typeof body.pipe === "function" ? pair(body) : pair(null, body);
  if (isWebReadable(body)) return pair(Readable.fromWeb(body));
  if (isWebWritable(body)) return pair(null, Writable.fromWeb(body));
  if (typeof body === "function") return fromFunction(body);
  if (body instanceof require("buffer").Blob) return fromPromise(body.arrayBuffer());
  if (isIterable(body)) {
    return require("internal/streams_pipe").from(Duplex, body, { objectMode: true, writable: false });
  }
  if (body && isWebReadable(body.readable) && isWebWritable(body.writable)) return Duplex.fromWeb(body);
  if (body && (typeof body.readable === "object" || typeof body.writable === "object")) {
    const readable = body.readable ? (isStream(body.readable) ? body.readable : from(body.readable)) : null;
    const writable = body.writable ? (isStream(body.writable) ? body.writable : from(body.writable)) : null;
    return pair(readable, writable);
  }
  if (body && typeof body.then === "function") return fromPromise(body);
  throw errors.invalidArgType(
    "body",
    "of type function or an instance of Blob, ReadableStream, WritableStream, Stream, Iterable, AsyncIterable, or Promise or { readable, writable } pair",
    body,
  );
}

// A Writable that forwards to a WHATWG WritableStream.
function writableFromWeb(stream, options = {}) {
  const writer = stream.getWriter();
  let closed = false;
  const writable = new Writable({
    ...options,
    write(chunk, encoding, callback) {
      writer.ready.then(() => writer.write(chunk)).then(() => callback(), callback);
    },
    writev(chunks, callback) {
      Promise.all(chunks.map(({ chunk }) => writer.write(chunk))).then(() => callback(), callback);
    },
    final(callback) {
      closed = true;
      writer.close().then(() => process.nextTick(callback), callback);
    },
    destroy(error, callback) {
      if (!closed) writer.abort(error).catch(() => {});
      callback(error);
    },
  });
  writer.closed.catch((error) => writable.destroy(error));
  return writable;
}

function duplexFromWeb(pairs, options = {}) {
  errors.validate.object(pairs, "pair");
  const { readableObjectMode = false, writableObjectMode = false, encoding, decodeStrings = true, signal } = options;
  const readable = Readable.fromWeb(pairs.readable, { objectMode: readableObjectMode, encoding, signal });
  const writable = writableFromWeb(pairs.writable, { objectMode: writableObjectMode, decodeStrings, signal });
  return pair(readable, writable);
}

Writable.fromWeb = writableFromWeb;
Duplex.fromWeb = duplexFromWeb;
Duplex.from = from;

module.exports = { duplexify, isStream };
