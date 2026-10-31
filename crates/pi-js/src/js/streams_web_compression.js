// internal/streams_web_compression: CompressionStream and DecompressionStream over the zlib
// streams, as lib/internal/webstreams/compression.js and the duplex adapter of adapters.js.
const common = require("internal/streams_common");
const { ReadableStream } = require("internal/streams_web");
const { WritableStream, ByteLengthQueuingStrategy, CountQueuingStrategy } = require("internal/streams_web_writable");
const { constants } = require("internal/zlib_constants");
const { codeError } = require("internal/errors");
const { Buffer } = require("buffer");

const FORMATS = ["deflate", "deflate-raw", "gzip", "brotli"];
const ZLIB_FAILURES = new Set([...Object.keys(constants).filter((name) => constants[name] < 0), "Z_NEED_DICT"]);

let zlib;
const lazyZlib = () => (zlib ??= require("zlib"));

const withResolvers = () => {
  let resolve;
  let reject;
  const promise = new Promise((res, rej) => {
    resolve = res;
    reject = rej;
  });
  return { promise, resolve, reject };
};

/** WebIDL `CompressionFormat`: the string value of `format`, one of the four names. */
function compressionFormat(format, constructor) {
  const value = `${format}`;
  if (!FORMATS.includes(value)) {
    const message = `Failed to construct '${constructor}': 1st argument value '${value}' is not a valid enum value of type CompressionFormat.`;
    throw codeError(TypeError, "ERR_INVALID_ARG_VALUE", message);
  }
  return value;
}

function handleKnownInternalErrors(cause) {
  if (cause?.code === "ERR_STREAM_PREMATURE_CLOSE") return common.errors.abort(cause);
  if (ZLIB_FAILURES.has(cause?.code)) {
    const error = new TypeError(undefined, { cause });
    error.code = cause.code;
    return error;
  }
  return cause;
}

function writableFromStream(stream) {
  const highWaterMark = stream.writableHighWaterMark;
  const strategy = stream.writableObjectMode ? new CountQueuingStrategy({ highWaterMark }) : { highWaterMark };
  let controller;
  let backpressure;
  let closed;
  const cleanup = common.eos(stream, (error) => {
    error = handleKnownInternalErrors(error);
    cleanup();
    stream.on("error", () => {});
    if (error != null) {
      backpressure?.reject(error);
      closed?.reject(error);
      closed = undefined;
      controller?.error(error);
      controller = undefined;
      return;
    }
    if (closed !== undefined) {
      closed.resolve();
      closed = undefined;
      return;
    }
    controller?.error(common.errors.abort());
    controller = undefined;
  });
  stream.on("drain", () => backpressure?.resolve());
  return new WritableStream({
    start(c) {
      controller = c;
    },
    write(chunk) {
      // As Node 24: an ArrayBuffer is a BufferSource; Node 22 leaves its write pending.
      if (chunk instanceof ArrayBuffer) chunk = new Uint8Array(chunk);
      let accepted = false;
      if (!stream.writableNeedDrain) {
        try {
          accepted = stream.write(chunk);
        } catch (error) {
          // As Node 24: a bad chunk fails both sides instead of leaving the write pending.
          stream.destroy(error);
          throw error;
        }
      }
      if (!accepted) {
        backpressure = withResolvers();
        return backpressure.promise.finally(() => {
          backpressure = undefined;
        });
      }
    },
    abort(reason) {
      stream.destroy(reason);
    },
    close() {
      if (closed === undefined && !stream.writableEnded) {
        closed = withResolvers();
        stream.end();
        return closed.promise;
      }
      controller = undefined;
      return Promise.resolve();
    },
  }, strategy);
}

function readableFromStream(stream) {
  const objectMode = stream.readableObjectMode;
  const highWaterMark = stream.readableHighWaterMark;
  const strategy = objectMode ? new CountQueuingStrategy({ highWaterMark }) : new ByteLengthQueuingStrategy({ highWaterMark });
  let controller;
  let canceled = false;
  stream.pause();
  const cleanup = common.eos(stream, (error) => {
    error = handleKnownInternalErrors(error);
    cleanup();
    stream.on("error", () => {});
    if (error) return controller.error(error);
    if (!canceled) controller.close();
  });
  stream.on("data", (chunk) => {
    if (!objectMode && Buffer.isBuffer(chunk)) chunk = new Uint8Array(chunk);
    controller.enqueue(chunk);
    if (controller.desiredSize <= 0) stream.pause();
  });
  return new ReadableStream({
    start(c) {
      controller = c;
    },
    pull() {
      stream.resume();
    },
    cancel(reason) {
      canceled = true;
      stream.destroy(reason);
    },
  }, strategy);
}

const pairOf = (stream) => ({ readable: readableFromStream(stream), writable: writableFromStream(stream) });

class CompressionStream {
  #transform;

  constructor(format) {
    format = compressionFormat(format, "CompressionStream");
    const z = lazyZlib();
    const create = { deflate: z.createDeflate, "deflate-raw": z.createDeflateRaw, gzip: z.createGzip, brotli: z.createBrotliCompress };
    this.#transform = pairOf(create[format]());
  }

  get readable() {
    return this.#transform.readable;
  }

  get writable() {
    return this.#transform.writable;
  }
}

class DecompressionStream {
  #transform;

  constructor(format) {
    format = compressionFormat(format, "DecompressionStream");
    const z = lazyZlib();
    const handle = {
      deflate: () => z.createInflate({ rejectGarbageAfterEnd: true }),
      "deflate-raw": () => z.createInflateRaw(),
      gzip: () => z.createGunzip({ rejectGarbageAfterEnd: true }),
      brotli: () => z.createBrotliDecompress(),
    }[format]();
    this.#transform = pairOf(handle);
    handle.on("error", (error) => {
      const writable = this.#transform?.writable;
      if (writable && !writable.locked && typeof writable.abort === "function") writable.abort(error);
    });
  }

  get readable() {
    return this.#transform.readable;
  }

  get writable() {
    return this.#transform.writable;
  }
}

for (const [Class, name] of [[CompressionStream, "CompressionStream"], [DecompressionStream, "DecompressionStream"]]) {
  for (const key of ["readable", "writable"]) Object.defineProperty(Class.prototype, key, { enumerable: true });
  Object.defineProperty(Class.prototype, Symbol.toStringTag, { configurable: true, value: name });
}

module.exports = { CompressionStream, DecompressionStream };
