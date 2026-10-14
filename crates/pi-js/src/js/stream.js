// node:stream: the legacy Stream class carrying Readable, Writable, Duplex, Transform, and helpers.
const common = require("internal/streams_common");
const { Readable } = require("internal/streams_readable");
const { Writable, Duplex, Transform, PassThrough } = require("internal/streams_writable");
const pipes = require("internal/streams_pipe");

const Stream = common.Stream;
const promises = { pipeline: pipes.pipelinePromise, finished: pipes.finishedPromise };

function finished(stream, options, callback) {
  return common.eos(stream, options, callback);
}
finished[Symbol.for("nodejs.util.promisify.custom")] = pipes.finishedPromise;
const pipeline = pipes.pipeline;
pipeline[Symbol.for("nodejs.util.promisify.custom")] = pipes.pipelinePromise;

// One Duplex over two streams: writes go to `head`, reads come from `tail`.
function duplexify(head, tail) {
  let onFinal = null;
  let finished = false;
  const duplex = new Duplex({
    writableObjectMode: !!(head && head.writableObjectMode),
    readableObjectMode: !!(tail && tail.readableObjectMode),
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

// Duplex.from(async function* (source) {}): written chunks are the source; yields are read.
function fromFunction(fn) {
  const input = new PassThrough({ objectMode: true });
  const controller = new AbortController();
  const result = fn(input, { signal: controller.signal });
  if (result && (typeof result[Symbol.asyncIterator] === "function" || typeof result[Symbol.iterator] === "function")) {
    const output = Readable.from(result);
    const { duplex, finish } = duplexify(input, output);
    output.on("end", () => finish());
    output.on("error", finish);
    return duplex;
  }
  if (result && typeof result.then === "function") {
    const output = new Readable({ objectMode: true, read() {} });
    const { duplex, finish } = duplexify(input, output);
    result.then(() => {
      output.push(null);
      finish();
    }, finish);
    return duplex;
  }
  throw Object.assign(new TypeError(`The "body" argument must be of type AsyncIterable, Promise, or Function. Received ${typeof result}`), { code: "ERR_INVALID_RETURN_VALUE" });
}

const isStream = (value) => value && typeof value === "object" && typeof value.on === "function" && (typeof value.pipe === "function" || typeof value.write === "function");

Duplex.from = function from(body) {
  if (body instanceof Duplex) return body;
  if (typeof body === "function") return fromFunction(body);
  if (body && (typeof body[Symbol.asyncIterator] === "function" || typeof body[Symbol.iterator] === "function")) {
    return Readable.from(body);
  }
  if (body && typeof body === "object" && (isStream(body.readable) || isStream(body.writable))) {
    const { duplex, finish } = duplexify(body.writable, body.readable);
    if (body.readable) body.readable.on("end", () => finish());
    else body.writable.on("finish", () => finish());
    return duplex;
  }
  if (isStream(body)) return duplexify(body, body).duplex;
  throw Object.assign(new TypeError("The \"body\" argument must be of type Stream, Iterable, AsyncIterable, Function, or { readable, writable } pair."), { code: "ERR_INVALID_ARG_TYPE" });
};

function compose(...streams) {
  if (streams.length === 1 && Array.isArray(streams[0])) streams = streams[0];
  if (streams.length === 0) throw Object.assign(new TypeError('The "streams" argument must be specified'), { code: "ERR_MISSING_ARGS" });
  if (streams.length === 1) return Duplex.from(streams[0]);
  const stages = streams.map((stream, index) => {
    const converted = isStream(stream) ? stream : Duplex.from(stream);
    const invalid = (reason) => Object.assign(new TypeError(`The argument 'streams[${index}]' ${reason}. Received ${require("util").inspect(stream)}`), { code: "ERR_INVALID_ARG_VALUE" });
    if (index < streams.length - 1 && typeof converted.pipe !== "function") throw invalid("must be readable");
    if (index > 0 && typeof converted.write !== "function") throw invalid("must be writable");
    return converted;
  });
  const head = stages[0];
  const tail = stages[stages.length - 1];
  const { duplex, finish } = duplexify(typeof head.write === "function" ? head : null, typeof tail.pipe === "function" ? tail : null);
  pipes.pipeline(stages, (error) => finish(error));
  return duplex;
}

function duplexPair(options) {
  const left = new Duplex({ ...options, read() {} });
  const right = new Duplex({ ...options, read() {} });
  const link = (from, to) => {
    from._write = (chunk, encoding, callback) => {
      to.push(chunk);
      callback();
    };
    from._final = (callback) => {
      to.push(null);
      callback();
    };
  };
  link(left, right);
  link(right, left);
  return [left, right];
}

Object.assign(Stream, {
  Stream,
  Readable,
  Writable,
  Duplex,
  Transform,
  PassThrough,
  pipeline,
  finished,
  promises,
  addAbortSignal: common.addAbortSignal,
  duplexPair,
  getDefaultHighWaterMark: common.getDefaultHighWaterMark,
  setDefaultHighWaterMark: common.setDefaultHighWaterMark,
  isReadable: (stream) => !!(stream && stream.readable),
  isWritable: (stream) => !!(stream && stream.writable),
  isErrored: (stream) => !!(stream && stream.errored),
  isDisturbed: Readable.isDisturbed,
  compose,
  _isUint8Array: (value) => value instanceof Uint8Array,
  _uint8ArrayToBuffer: (value) => require("buffer").Buffer.from(value.buffer, value.byteOffset, value.byteLength),
});
Object.defineProperty(Stream, "default", { value: Stream, enumerable: false });

module.exports = Stream;
