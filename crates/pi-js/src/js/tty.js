// node:tty: descriptors 1 and 2 report the runtime's output; pi owns the real terminal.
const { Readable, Writable } = require("stream");
const STDIO = { 1: "stdout", 2: "stderr" };

function isatty(fd) {
  return STDIO[fd] !== undefined && native.io.isTTY(STDIO[fd]);
}

class ReadStream extends Readable {
  constructor() {
    super();
    throw internal.unsupported("tty.ReadStream");
  }
}

class WriteStream extends Writable {
  constructor(fd) {
    super();
    if (!STDIO[fd]) throw internal.unsupported(`tty.WriteStream for descriptor ${fd}`);
    return process[STDIO[fd]];
  }
}

module.exports = { isatty, ReadStream, WriteStream };
