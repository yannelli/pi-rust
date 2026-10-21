// internal/fs_utils: argument handling, Stats, Dirent, constants, and the native call helpers.
const { Buffer } = require("buffer");
const path = require("path");
const nativeFs = native.fs;
const platform = internal.config.platform;

const S_IFMT = 0o170000;
const S_IFREG = 0o100000;
const S_IFDIR = 0o040000;
const S_IFCHR = 0o020000;
const S_IFBLK = 0o060000;
const S_IFIFO = 0o010000;
const S_IFLNK = 0o120000;
const S_IFSOCK = 0o140000;

const OPEN = platform === "darwin"
  ? { O_RDONLY: 0, O_WRONLY: 1, O_RDWR: 2, O_CREAT: 512, O_EXCL: 2048, O_TRUNC: 1024, O_APPEND: 8, O_SYNC: 128, O_NOFOLLOW: 256, O_DIRECTORY: 1048576, O_SYMLINK: 2097152 }
  : { O_RDONLY: 0, O_WRONLY: 1, O_RDWR: 2, O_CREAT: 64, O_EXCL: 128, O_TRUNC: 512, O_APPEND: 1024, O_SYNC: 1052672, O_NOFOLLOW: 131072, O_DIRECTORY: 65536 };

const constants = {
  F_OK: 0, R_OK: 4, W_OK: 2, X_OK: 1,
  COPYFILE_EXCL: 1, COPYFILE_FICLONE: 2, COPYFILE_FICLONE_FORCE: 4,
  UV_FS_COPYFILE_EXCL: 1, UV_FS_COPYFILE_FICLONE: 2, UV_FS_COPYFILE_FICLONE_FORCE: 4, UV_FS_SYMLINK_DIR: 1, UV_FS_SYMLINK_JUNCTION: 2,
  S_IFMT, S_IFREG, S_IFDIR, S_IFCHR, S_IFBLK, S_IFIFO, S_IFLNK, S_IFSOCK,
  S_IRWXU: 0o700, S_IRUSR: 0o400, S_IWUSR: 0o200, S_IXUSR: 0o100, S_IRWXG: 0o70, S_IRGRP: 0o40, S_IWGRP: 0o20, S_IXGRP: 0o10,
  S_IRWXO: 0o7, S_IROTH: 0o4, S_IWOTH: 0o2, S_IXOTH: 0o1,
  UV_DIRENT_UNKNOWN: 0, UV_DIRENT_FILE: 1, UV_DIRENT_DIR: 2, UV_DIRENT_LINK: 3, UV_DIRENT_FIFO: 4, UV_DIRENT_SOCKET: 5, UV_DIRENT_CHAR: 6, UV_DIRENT_BLOCK: 7,
  ...OPEN,
};

function invalidArg(name, expected, value) {
  return require("internal/errors").invalidArgType(name, expected, value);
}

// A path argument: string, Buffer, or file: URL.
function toPath(value, name = "path") {
  if (typeof value === "string") {
    if (value.includes("\u0000")) throw Object.assign(new TypeError(`The argument '${name}' must be a string without null bytes. Received ${JSON.stringify(value)}`), { code: "ERR_INVALID_ARG_VALUE" });
    return value;
  }
  if (value instanceof Uint8Array) return Buffer.from(value).toString();
  if (value && typeof value === "object" && typeof value.href === "string" && value.protocol !== undefined) {
    return require("url").fileURLToPath(value);
  }
  throw invalidArg(name, "of type string or an instance of Buffer or URL", value);
}

function toFd(value) {
  if (!Number.isInteger(value) || value < 0) throw invalidArg("fd", "of type number", value);
  return value;
}

// `options` may be an encoding string or an object; `defaults` fills missing keys.
function getOptions(options, defaults) {
  if (options === undefined || options === null || typeof options === "function") return { ...defaults };
  if (typeof options === "string") return { ...defaults, encoding: options };
  if (typeof options !== "object") throw invalidArg("options", "one of type string or object", options);
  return { ...defaults, ...options };
}

function flagString(flags, fallback) {
  if (flags === undefined || flags === null) return fallback;
  if (typeof flags === "string") return flags;
  if (typeof flags !== "number") throw invalidArg("flags", "of type string or number", flags);
  const access = flags & 3;
  const append = (flags & OPEN.O_APPEND) !== 0;
  const exclusive = (flags & OPEN.O_EXCL) !== 0 ? "x" : "";
  if (append) return `a${exclusive}${access === OPEN.O_RDWR ? "+" : ""}`;
  if ((flags & OPEN.O_TRUNC) !== 0 || (flags & OPEN.O_CREAT) !== 0) return `w${exclusive}${access === OPEN.O_RDWR ? "+" : ""}`;
  return access === OPEN.O_RDONLY ? "r" : "r+";
}

function parseMode(mode, fallback) {
  if (mode === undefined || mode === null) return fallback;
  if (typeof mode === "string") {
    if (!/^[0-7]+$/.test(mode)) throw require("internal/errors").invalidArgValue("mode", mode, "must be a 32-bit unsigned integer or an octal string");
    return Number.parseInt(mode, 8);
  }
  return mode;
}

function decode(bytes, encoding) {
  const buffer = Buffer.from(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  return encoding && encoding !== "buffer" ? buffer.toString(encoding) : buffer;
}

// Data for a write as a latin1 string the native side turns back into bytes.
function dataToLatin1(data, encoding) {
  if (typeof data === "string") return internal.bytesToLatin1(Buffer.from(data, encoding || "utf8"));
  const bytes = internal.bytesToLatin1(data);
  if (bytes === undefined) {
    throw invalidArg("data", "of type string or an instance of Buffer, TypedArray, or DataView", data);
  }
  return bytes;
}

function dateFromMs(ms) {
  return new Date(Math.round(ms));
}

class Stats {
  constructor(raw) {
    Object.assign(this, raw);
    delete this.exact;
  }
}

// Node's BigIntStats: integer fields as bigints, plus nanosecond times.
const EXACT = ["dev", "ino", "rdev", "size", "blocks", "atimeNs", "mtimeNs", "ctimeNs", "birthtimeNs"];
class BigIntStats {
  constructor(raw) {
    for (const key of ["dev", "mode", "nlink", "uid", "gid", "rdev", "blksize", "ino", "size", "blocks"]) this[key] = BigInt(raw[key]);
    raw.exact.forEach((value, index) => (this[EXACT[index]] = BigInt(value)));
    for (const name of ["atime", "mtime", "ctime", "birthtime"]) {
      const ns = this[`${name}Ns`];
      this[`${name}Ms`] = ns / 1000000n - (ns < 0n && ns % 1000000n !== 0n ? 1n : 0n);
      this[name] = new Date(Number(this[`${name}Ms`]));
    }
  }
}
for (const name of ["atime", "mtime", "ctime", "birthtime"]) {
  Object.defineProperty(Stats.prototype, name, {
    configurable: true,
    enumerable: true,
    get() {
      return dateFromMs(this[`${name}Ms`]);
    },
    set(value) {
      Object.defineProperty(this, name, { value, writable: true, configurable: true, enumerable: true });
    },
  });
}

const kType = Symbol("type");
const DIRENT_TYPES = [0, S_IFREG, S_IFDIR, S_IFLNK, S_IFIFO, S_IFSOCK, S_IFCHR, S_IFBLK];
class Dirent {
  constructor(name, type, parentPath) {
    this.name = name;
    this.parentPath = parentPath;
    this.path = parentPath;
    Object.defineProperty(this, kType, { value: DIRENT_TYPES[type] || 0 });
  }
}

const TYPE_CHECKS = {
  isFile: S_IFREG,
  isDirectory: S_IFDIR,
  isSymbolicLink: S_IFLNK,
  isFIFO: S_IFIFO,
  isSocket: S_IFSOCK,
  isCharacterDevice: S_IFCHR,
  isBlockDevice: S_IFBLK,
};
for (const [name, bits] of Object.entries(TYPE_CHECKS)) {
  Stats.prototype[name] = function () {
    return (this.mode & S_IFMT) === bits;
  };
  BigIntStats.prototype[name] = function () {
    return (this.mode & BigInt(S_IFMT)) === BigInt(bits);
  };
  Dirent.prototype[name] = function () {
    return this[kType] === bits;
  };
}

function makeEntries(entries, directory, options) {
  if (options.withFileTypes) {
    return entries.map(([name, type]) => {
      const parent = path.join(directory, path.dirname(name));
      return new Dirent(path.basename(name), type, parent === "." ? directory : parent);
    });
  }
  if (options.encoding === "buffer") return entries.map(([name]) => Buffer.from(name));
  return entries.map(([name]) => name);
}

// The stats mapper for an options object: plain Stats or BigIntStats.
function statsMapper(options) {
  return options && options.bigint ? (raw) => new BigIntStats(raw) : (raw) => new Stats(raw);
}

module.exports = {
  constants,
  Stats,
  Dirent,
  invalidArg,
  toPath,
  toFd,
  getOptions,
  flagString,
  parseMode,
  decode,
  dataToLatin1,
  makeEntries,
  statsMapper,
  BigIntStats,
  sync: nativeFs.sync,
  async: nativeFs.async,
};
