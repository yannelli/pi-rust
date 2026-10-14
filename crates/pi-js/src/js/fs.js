// node:fs: sync and callback APIs generated from internal/fs_ops, plus fd I/O, Dir, and streams.
const { Buffer } = require("buffer");
const utils = require("internal/fs_utils");
const { ops, runSync, runAsync } = require("internal/fs_ops");
const streams = require("internal/fs_streams");

const fs = {};

function checkCallback(callback) {
  if (typeof callback !== "function") throw utils.invalidArg("cb", "of type function", callback);
  return callback;
}

// Settle on a later tick so a throwing callback is an uncaught exception, as in Node.
function settle(promise, callback, toArgs = (value) => [null, value]) {
  promise.then(
    (value) => process.nextTick(() => callback(...toArgs(value))),
    (error) => process.nextTick(callback, error),
  );
}

for (const name of Object.keys(ops)) {
  if (name === "exists") continue;
  fs[`${name}Sync`] = function syncOperation(...args) {
    return runSync(ops[name](...args));
  };
  fs[name] = function callbackOperation(...args) {
    const callback = checkCallback(args.pop());
    settle(runAsync(ops[name](...args)), callback);
  };
}

// existsSync never throws in Node: invalid arguments and errors both mean false.
fs.existsSync = function existsSync(target) {
  try {
    return runSync(ops.exists(target));
  } catch {
    return false;
  }
};
fs.exists = function exists(target, callback) {
  checkCallback(callback);
  let promise;
  try {
    promise = runAsync(ops.exists(target));
  } catch {
    promise = Promise.resolve(false);
  }
  promise.then((result) => process.nextTick(callback, result), () => process.nextTick(callback, false));
};
function statSync(operation) {
  return function statOrUndefined(target, options) {
    try {
      return runSync(operation(target, options));
    } catch (error) {
      if (options && options.throwIfNoEntry === false && error.code === "ENOENT") return undefined;
      throw error;
    }
  };
}
fs.statSync = statSync(ops.stat);
fs.lstatSync = statSync(ops.lstat);
fs.realpathSync.native = fs.realpathSync;
fs.realpath.native = fs.realpath;

// The standard streams are not files here; their descriptors go to the runtime's output.
const STDIO = { 1: "stdout", 2: "stderr" };

function readArgs(buffer, offsetOrOptions, length, position) {
  let offset = offsetOrOptions;
  if (offsetOrOptions && typeof offsetOrOptions === "object") ({ offset, length, position } = offsetOrOptions);
  offset = offset === undefined ? 0 : offset;
  length = length === undefined ? buffer.byteLength - offset : length;
  if (offset < 0 || length < 0 || offset + length > buffer.byteLength) {
    throw Object.assign(new RangeError(`The value of "length" is out of range. It must be <= ${buffer.byteLength - offset}. Received ${length}`), { code: "ERR_OUT_OF_RANGE" });
  }
  const typedPosition = typeof position === "bigint" ? Number(position) : position;
  return { offset, length, position: typedPosition === undefined || typedPosition === null || typedPosition < 0 ? null : typedPosition };
}

function copyInto(buffer, offset, bytes) {
  new Uint8Array(buffer.buffer, buffer.byteOffset, buffer.byteLength).set(bytes, offset);
  return bytes.length;
}

fs.readSync = function readSync(fd, buffer, offsetOrOptions, length, position) {
  if (fd === 0) throw internal.unsupported("Reading standard input");
  const args = readArgs(buffer, offsetOrOptions, length, position);
  if (args.length === 0) return 0;
  return copyInto(buffer, args.offset, utils.sync({ op: "read", fd: utils.toFd(fd), length: args.length, position: args.position }));
};
fs.read = function read(fd, buffer, offsetOrOptions, length, position, callback) {
  if (typeof buffer === "function") [callback, buffer] = [buffer, Buffer.alloc(16384)];
  else if (buffer && !ArrayBuffer.isView(buffer) && typeof buffer === "object") {
    callback = offsetOrOptions;
    offsetOrOptions = buffer;
    buffer = buffer.buffer || Buffer.alloc(16384);
  } else if (typeof offsetOrOptions === "function") [callback, offsetOrOptions] = [offsetOrOptions, undefined];
  else if (typeof length === "function") [callback, length] = [length, undefined];
  else if (typeof position === "function") [callback, position] = [position, undefined];
  checkCallback(callback);
  const args = readArgs(buffer, offsetOrOptions, length, position);
  const promise = utils.async({ op: "read", fd: utils.toFd(fd), length: args.length, position: args.position });
  settle(promise, callback, (bytes) => [null, copyInto(buffer, args.offset, bytes), buffer]);
};

function writeArgs(data, offsetOrPosition, lengthOrEncoding, position) {
  if (typeof data === "string") {
    const encoding = typeof lengthOrEncoding === "string" ? lengthOrEncoding : "utf8";
    return { latin1: utils.dataToLatin1(data, encoding), position: offsetOrPosition };
  }
  if (!ArrayBuffer.isView(data)) throw utils.invalidArg("buffer", "of type string or an instance of Buffer, TypedArray, or DataView", data);
  let offset = offsetOrPosition;
  let length = lengthOrEncoding;
  if (offset && typeof offset === "object") ({ offset, length, position } = offset);
  offset = offset === undefined || offset === null ? 0 : offset;
  length = length === undefined || length === null ? data.byteLength - offset : length;
  const view = new Uint8Array(data.buffer, data.byteOffset + offset, length);
  return { latin1: internal.bytesToLatin1(view), position };
}

function normalizePosition(position) {
  return typeof position === "number" && position >= 0 ? position : null;
}

fs.writeSync = function writeSync(fd, data, offsetOrPosition, lengthOrEncoding, position) {
  const args = writeArgs(data, offsetOrPosition, lengthOrEncoding, position);
  if (STDIO[fd]) {
    native.io.writeBytes(STDIO[fd], args.latin1);
    return args.latin1.length;
  }
  return utils.sync({ op: "write", fd: utils.toFd(fd), position: normalizePosition(args.position) }, args.latin1);
};
fs.write = function write(fd, data, ...rest) {
  const callback = checkCallback(rest.pop());
  const args = writeArgs(data, ...rest);
  if (STDIO[fd]) {
    native.io.writeBytes(STDIO[fd], args.latin1);
    process.nextTick(callback, null, args.latin1.length, data);
    return;
  }
  const promise = utils.async({ op: "write", fd: utils.toFd(fd), position: normalizePosition(args.position) }, args.latin1);
  settle(promise, callback, (written) => [null, written, data]);
};
fs.writevSync = function writevSync(fd, buffers, position) {
  return fs.writeSync(fd, Buffer.concat(buffers.map((item) => Buffer.from(item.buffer, item.byteOffset, item.byteLength))), 0, undefined, position);
};
fs.writev = function writev(fd, buffers, position, callback) {
  if (typeof position === "function") [callback, position] = [position, null];
  const joined = Buffer.concat(buffers.map((item) => Buffer.from(item.buffer, item.byteOffset, item.byteLength)));
  fs.write(fd, joined, 0, joined.length, position, (error, written) => callback(error, written, buffers));
};
fs.readvSync = function readvSync(fd, buffers, position) {
  let total = 0;
  for (const buffer of buffers) {
    const count = fs.readSync(fd, buffer, 0, buffer.byteLength, position === undefined || position === null ? null : position + total);
    total += count;
    if (count < buffer.byteLength) break;
  }
  return total;
};
fs.readv = function readv(fd, buffers, position, callback) {
  if (typeof position === "function") [callback, position] = [position, null];
  checkCallback(callback);
  let total;
  try {
    total = fs.readvSync(fd, buffers, position);
  } catch (error) {
    process.nextTick(callback, error);
    return;
  }
  process.nextTick(callback, null, total, buffers);
};

class Dir {
  #entries;
  #index = 0;
  #closed = false;
  constructor(directory, entries) {
    this.path = directory;
    this.#entries = entries;
  }
  readSync() {
    if (this.#closed) throw Object.assign(new Error("Directory handle was closed"), { code: "ERR_DIR_CLOSED" });
    return this.#index < this.#entries.length ? this.#entries[this.#index++] : null;
  }
  read(callback) {
    const promise = new Promise((resolve) => resolve(this.readSync()));
    if (typeof callback !== "function") return promise;
    settle(promise, callback);
    return undefined;
  }
  closeSync() {
    this.#closed = true;
  }
  close(callback) {
    this.#closed = true;
    if (typeof callback !== "function") return Promise.resolve();
    process.nextTick(callback, null);
    return undefined;
  }
  async *entries() {
    let entry;
    while ((entry = this.readSync()) !== null) yield entry;
    this.closeSync();
  }
  [Symbol.asyncIterator]() {
    return this.entries();
  }
}
fs.opendirSync = function opendirSync(directory, options) {
  const target = utils.toPath(directory);
  return new Dir(target, fs.readdirSync(target, { ...options, withFileTypes: true }));
};
fs.opendir = function opendir(directory, options, callback) {
  if (typeof options === "function") [callback, options] = [options, undefined];
  checkCallback(callback);
  const target = utils.toPath(directory);
  settle(runAsync(ops.readdir(target, { ...options, withFileTypes: true })), callback, (entries) => [null, new Dir(target, entries)]);
};

fs.createReadStream = (file, options) => new streams.ReadStream(file, options);
fs.createWriteStream = (file, options) => new streams.WriteStream(file, options);
Object.assign(fs, {
  ReadStream: streams.ReadStream,
  WriteStream: streams.WriteStream,
  FSWatcher: streams.FSWatcher,
  StatWatcher: streams.StatWatcher,
  watch: streams.watch,
  watchFile: streams.watchFile,
  unwatchFile: streams.unwatchFile,
  Stats: utils.Stats,
  Dirent: utils.Dirent,
  Dir,
  constants: utils.constants,
  F_OK: 0,
  R_OK: 4,
  W_OK: 2,
  X_OK: 1,
});
fs.globSync = (pattern, options) => require("internal/glob").globSync(pattern, options);
fs.glob = function glob(pattern, options, callback) {
  if (typeof options === "function") [callback, options] = [options, undefined];
  if (typeof callback !== "function") throw utils.invalidArg("callback", "of type function", callback);
  const matches = fs.globSync(pattern, options);
  process.nextTick(callback, null, matches);
};
Object.defineProperty(fs, "promises", { enumerable: true, configurable: true, get: () => require("fs/promises") });
fs._toUnixTimestamp = (time) => require("internal/fs_ops").toSeconds(time);

module.exports = fs;
