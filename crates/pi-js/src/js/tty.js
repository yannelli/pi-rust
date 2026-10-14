// node:tty: descriptor 0 is the process's stdin; 1 and 2 report the runtime's output.
const { Readable, Writable } = require("stream");
const STDIO = { 1: "stdout", 2: "stderr" };

function isatty(fd) {
  if (fd === 0) return native.pipe.stdinIsTTY();
  return STDIO[fd] !== undefined && native.io.isTTY(STDIO[fd]);
}

// Only descriptor 0 can be a terminal input here; it is process.stdin.
class ReadStream extends Readable {
  constructor(fd) {
    super();
    if (fd !== 0 || !native.pipe.stdinIsTTY()) {
      const error = new Error("TTY initialization failed: uv_tty_init returned EINVAL (invalid argument)");
      throw Object.assign(error, { name: "SystemError", code: "ERR_TTY_INIT_FAILED", errno: require("internal/errors").errnoOf("EINVAL"), syscall: "uv_tty_init" });
    }
    return process.stdin;
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
