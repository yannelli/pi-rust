// node:stream/web, also the source of the web stream globals.
const readable = require("internal/streams_web");
const writable = require("internal/streams_web_writable");
const bytes = require("internal/streams_web_bytes");

module.exports = {
  ReadableStream: readable.ReadableStream,
  ReadableStreamDefaultReader: readable.ReadableStreamDefaultReader,
  ReadableStreamDefaultController: readable.ReadableStreamDefaultController,
  ...writable,
  ReadableByteStreamController: bytes.ReadableByteStreamController,
  ReadableStreamBYOBReader: bytes.ReadableStreamBYOBReader,
  ReadableStreamBYOBRequest: bytes.ReadableStreamBYOBRequest,
};
