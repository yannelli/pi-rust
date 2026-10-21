// node:stream: the legacy Stream class carrying Readable, Writable, Duplex, Transform, and helpers.
const common = require("internal/streams_common");
const { Readable } = require("internal/streams_readable");
const { Writable, Duplex, Transform, PassThrough } = require("internal/streams_writable");
const pipes = require("internal/streams_pipe");
const { duplexify, isStream } = require("internal/streams_duplexify");

const Stream = common.Stream;
const promises = { pipeline: pipes.pipelinePromise, finished: pipes.finishedPromise };

function finished(stream, options, callback) {
  return common.eos(stream, options, callback);
}
finished[Symbol.for("nodejs.util.promisify.custom")] = pipes.finishedPromise;
const pipeline = pipes.pipeline;
pipeline[Symbol.for("nodejs.util.promisify.custom")] = pipes.pipelinePromise;

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
