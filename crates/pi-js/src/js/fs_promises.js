// node:fs/promises: promise APIs generated from internal/fs_ops, plus FileHandle and watch().
const { Buffer } = require("buffer");
const utils = require("internal/fs_utils");
const { ops, runAsync } = require("internal/fs_ops");
const streams = require("internal/fs_streams");

const promises = {};
for (const name of Object.keys(ops)) {
  if (name === "exists") continue;
  promises[name] = async function promiseOperation(...args) {
    return runAsync(ops[name](...args));
  };
}

class FileHandle {
  #fd;
  constructor(fd) {
    this.#fd = fd;
  }
  get fd() {
    return this.#fd;
  }
  #check() {
    if (this.#fd < 0) throw Object.assign(new Error("file closed"), { code: "EBADF", syscall: "read" });
    return this.#fd;
  }
  async close() {
    if (this.#fd < 0) return;
    const fd = this.#fd;
    this.#fd = -1;
    await runAsync(ops.close(fd));
  }
  async [Symbol.asyncDispose]() {
    await this.close();
  }
  async read(buffer, offset, length, position) {
    if (buffer && !ArrayBuffer.isView(buffer) && typeof buffer === "object") ({ buffer, offset, length, position } = buffer);
    buffer = buffer || Buffer.alloc(16384);
    if (offset && typeof offset === "object") ({ offset, length, position } = offset);
    offset = offset || 0;
    length = length === undefined ? buffer.byteLength - offset : length;
    const bytes = await utils.async({ op: "read", fd: this.#check(), length, position: typeof position === "number" && position >= 0 ? position : null });
    new Uint8Array(buffer.buffer, buffer.byteOffset, buffer.byteLength).set(bytes, offset);
    return { bytesRead: bytes.length, buffer };
  }
  async write(data, offsetOrOptions, lengthOrEncoding, position) {
    let latin1;
    if (typeof data === "string") {
      position = offsetOrOptions;
      latin1 = utils.dataToLatin1(data, typeof lengthOrEncoding === "string" ? lengthOrEncoding : "utf8");
    } else {
      let offset = offsetOrOptions;
      let length = lengthOrEncoding;
      if (offset && typeof offset === "object") ({ offset, length, position } = offset);
      offset = offset || 0;
      length = length === undefined || length === null ? data.byteLength - offset : length;
      latin1 = internal.bytesToLatin1(new Uint8Array(data.buffer, data.byteOffset + offset, length));
    }
    const target = typeof position === "number" && position >= 0 ? position : null;
    const bytesWritten = await utils.async({ op: "write", fd: this.#check(), position: target }, latin1);
    return { bytesWritten, buffer: data };
  }
  async readFile(options) {
    return runAsync(ops.readFile(this.#check(), options));
  }
  async writeFile(data, options) {
    return runAsync(ops.writeFile(this.#check(), data, options));
  }
  async appendFile(data, options) {
    return runAsync(ops.writeFile(this.#check(), data, options));
  }
  async stat(options) {
    return runAsync(ops.fstat(this.#check(), options));
  }
  async truncate(len = 0) {
    return runAsync(ops.ftruncate(this.#check(), len));
  }
  async sync() {
    return runAsync(ops.fsync(this.#check()));
  }
  async datasync() {
    return runAsync(ops.fsync(this.#check()));
  }
  async utimes(atime, mtime) {
    return runAsync(ops.futimes(this.#check(), atime, mtime));
  }
  async chmod() {
    throw internal.unsupported("FileHandle.chmod");
  }
  async chown() {
    throw internal.unsupported("FileHandle.chown");
  }
  readableWebStream() {
    throw internal.unsupported("FileHandle.readableWebStream");
  }
  createReadStream(options) {
    return new streams.ReadStream(null, { ...options, fd: this.#check(), autoClose: false });
  }
  createWriteStream(options) {
    return new streams.WriteStream(null, { ...options, fd: this.#check(), autoClose: false });
  }
  readLines(options) {
    return require("readline").createInterface({ input: this.createReadStream(options), crlfDelay: Infinity });
  }
}

promises.open = async function open(target, flags, mode) {
  return new FileHandle(await runAsync(ops.open(target, flags, mode)));
};
for (const name of ["readFile", "writeFile", "appendFile"]) {
  const generated = promises[name];
  promises[name] = async function handleOrPath(target, ...args) {
    if (target instanceof FileHandle) return target[name](...args);
    return generated(target, ...args);
  };
}
promises.opendir = async function opendir(target, options) {
  const directory = utils.toPath(target);
  const entries = await runAsync(ops.readdir(directory, { ...options, withFileTypes: true }));
  return new (require("fs").Dir)(directory, entries);
};

promises.watch = function watch(filename, options = {}) {
  const queue = [];
  let wake = null;
  let closed = false;
  const watcher = streams.watch(filename, options, (eventType, name) => {
    queue.push({ eventType, filename: name });
    if (wake) wake();
  });
  const close = () => {
    closed = true;
    watcher.close();
    if (wake) wake();
  };
  if (options.signal) {
    if (options.signal.aborted) close();
    else options.signal.addEventListener("abort", close, { once: true });
  }
  return {
    async next() {
      while (queue.length === 0 && !closed) await new Promise((resolve) => (wake = resolve));
      wake = null;
      if (queue.length) return { value: queue.shift(), done: false };
      if (options.signal && options.signal.aborted) throw Object.assign(new Error("The operation was aborted"), { name: "AbortError", code: "ABORT_ERR" });
      return { value: undefined, done: true };
    },
    async return() {
      close();
      return { value: undefined, done: true };
    },
    [Symbol.asyncIterator]() {
      return this;
    },
  };
};

promises.constants = utils.constants;
promises.FileHandle = FileHandle;
module.exports = promises;
