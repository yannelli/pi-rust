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

Duplex.from = function from(body) {
  if (body instanceof Duplex) return body;
  if (body && (typeof body[Symbol.asyncIterator] === "function" || typeof body[Symbol.iterator] === "function")) {
    return Readable.from(body);
  }
  throw internal.unsupported("Duplex.from with this argument");
};

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
  compose() {
    throw internal.unsupported("stream.compose");
  },
  _isUint8Array: (value) => value instanceof Uint8Array,
  _uint8ArrayToBuffer: (value) => require("buffer").Buffer.from(value.buffer, value.byteOffset, value.byteLength),
});
Object.defineProperty(Stream, "default", { value: Stream, enumerable: false });

module.exports = Stream;
