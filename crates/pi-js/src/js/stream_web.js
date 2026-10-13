// node:stream/web, also the source of the web stream globals.
const readable = require("internal/streams_web");
const writable = require("internal/streams_web_writable");

module.exports = {
  ReadableStream: readable.ReadableStream,
  ReadableStreamDefaultReader: readable.ReadableStreamDefaultReader,
  ReadableStreamDefaultController: readable.ReadableStreamDefaultController,
  ...writable,
  ReadableByteStreamController: class ReadableByteStreamController {
    constructor() {
      throw internal.unsupported("ReadableByteStreamController");
    }
  },
  ReadableStreamBYOBReader: class ReadableStreamBYOBReader {
    constructor() {
      throw internal.unsupported("ReadableStreamBYOBReader");
    }
  },
};
