// The descriptor table and memory helpers for node:wasi, and the WASI preview1 errno values and
// rights as uvwasi defines them. System calls throw an errno number to fail.
const fs = require("fs");
const path = require("path");

// WASI errno names in value order (0 is success); fs errors map by their code.
const ERRNO_NAMES = ("SUCCESS 2BIG ACCES ADDRINUSE ADDRNOTAVAIL AFNOSUPPORT AGAIN ALREADY BADF BADMSG BUSY CANCELED " +
  "CHILD CONNABORTED CONNREFUSED CONNRESET DEADLK DESTADDRREQ DOM DQUOT EXIST FAULT FBIG HOSTUNREACH IDRM ILSEQ " +
  "INPROGRESS INTR INVAL IO ISCONN ISDIR LOOP MFILE MLINK MSGSIZE MULTIHOP NAMETOOLONG NETDOWN NETRESET NETUNREACH " +
  "NFILE NOBUFS NODEV NOENT NOEXEC NOLCK NOLINK NOMEM NOMSG NOPROTOOPT NOSPC NOSYS NOTCONN NOTDIR NOTEMPTY " +
  "NOTRECOVERABLE NOTSOCK NOTSUP NOTTY NXIO OVERFLOW OWNERDEAD PERM PIPE PROTO PROTONOSUPPORT PROTOTYPE RANGE ROFS " +
  "SPIPE SRCH STALE TIMEDOUT TXTBSY XDEV NOTCAPABLE").split(" ");
const E = Object.fromEntries(ERRNO_NAMES.map((name, index) => [name, index]));

const FILETYPE = { UNKNOWN: 0, CHARACTER_DEVICE: 2, DIRECTORY: 3, REGULAR_FILE: 4, SOCKET_STREAM: 6, SYMBOLIC_LINK: 7 };
const R = {
  FD_DATASYNC: 1n << 0n,
  FD_READ: 1n << 1n,
  FD_SEEK: 1n << 2n,
  FD_WRITE: 1n << 6n,
  FD_ALLOCATE: 1n << 8n,
  FD_READDIR: 1n << 14n,
  FD_FILESTAT_SET_SIZE: 1n << 22n,
};
const RIGHTS = {
  FILE: 0x8e001ffn,
  DIRECTORY: 0xfbffe98n,
  DIRECTORY_INHERITING: 0xfffffffn,
  TTY: 0x820004an,
  SOCKET: 0x1820004an,
};

function errnoOf(error) {
  if (typeof error === "number") return error;
  const code = typeof error?.code === "string" && error.code.startsWith("E") ? error.code.slice(1) : undefined;
  return code !== undefined && code in E ? E[code] : undefined;
}

function filetypeOf(stats) {
  if (stats.isFile()) return FILETYPE.REGULAR_FILE;
  if (stats.isDirectory()) return FILETYPE.DIRECTORY;
  if (stats.isSymbolicLink()) return FILETYPE.SYMBOLIC_LINK;
  if (stats.isCharacterDevice()) return FILETYPE.CHARACTER_DEVICE;
  if (stats.isBlockDevice()) return 1;
  if (stats.isSocket() || stats.isFIFO()) return FILETYPE.SOCKET_STREAM;
  return FILETYPE.UNKNOWN;
}

// Wasm memory access; every read goes through a fresh view since the buffer changes on growth.
class Memory {
  constructor() {
    this.memory = undefined;
  }
  get bytes() {
    return new Uint8Array(this.memory.buffer);
  }
  get view() {
    return new DataView(this.memory.buffer);
  }
  u32(at) {
    return this.view.getUint32(at >>> 0, true);
  }
  setU8(at, value) {
    this.view.setUint8(at >>> 0, value);
  }
  setU16(at, value) {
    this.view.setUint16(at >>> 0, value, true);
  }
  setU32(at, value) {
    this.view.setUint32(at >>> 0, value, true);
  }
  setU64(at, value) {
    this.view.setBigUint64(at >>> 0, BigInt.asUintN(64, BigInt(value)), true);
  }
  slice(at, length) {
    at >>>= 0;
    length >>>= 0;
    if (at + length > this.memory.buffer.byteLength) throw new RangeError("out of bounds");
    return Buffer.from(this.memory.buffer, at, length);
  }
  string(at, length) {
    return this.slice(at, length).toString("utf8");
  }
  // The buffers of an iovec array: `[pointer, length]` pairs.
  iovecs(at, count) {
    const list = [];
    for (let i = 0; i < count >>> 0; i++) list.push(this.slice(this.u32(at + 8 * i), this.u32(at + 8 * i + 4)));
    return list;
  }
}

// One open descriptor: a host fd for files and stdio, a host path for directories.
class Entry {
  constructor(fields) {
    this.hostFd = null;
    this.realPath = null;
    this.preopen = null;
    this.position = 0;
    this.flags = 0;
    Object.assign(this, fields);
  }
  get isFile() {
    return this.type === FILETYPE.REGULAR_FILE && this.hostFd !== null && !this.stdio;
  }
  stat(bigint = true) {
    if (this.hostFd !== null) return fs.fstatSync(this.hostFd, { bigint });
    return fs.statSync(this.realPath, { bigint });
  }
}

function stdioEntry(hostFd) {
  let type = FILETYPE.CHARACTER_DEVICE;
  if (!require("tty").isatty(hostFd)) {
    try {
      type = filetypeOf(fs.fstatSync(hostFd));
    } catch {}
  }
  const base = type === FILETYPE.REGULAR_FILE ? RIGHTS.FILE : type === FILETYPE.SOCKET_STREAM ? RIGHTS.SOCKET : RIGHTS.TTY;
  return new Entry({ type, hostFd, stdio: true, base, inheriting: type === FILETYPE.SOCKET_STREAM ? RIGHTS.SOCKET : 0n });
}

class Table {
  constructor(stdio, preopens) {
    this.entries = new Map();
    stdio.forEach((hostFd, fd) => this.entries.set(fd, stdioEntry(hostFd)));
    for (const [name, realPath] of preopens) {
      const entry = new Entry({ type: FILETYPE.DIRECTORY, realPath: path.resolve(realPath), preopen: name });
      Object.assign(entry, { base: RIGHTS.DIRECTORY, inheriting: RIGHTS.DIRECTORY_INHERITING });
      this.entries.set(this.entries.size, entry);
    }
  }
  get(fd, right = 0n) {
    const entry = this.entries.get(fd >>> 0);
    if (!entry) throw E.BADF;
    if ((entry.base & right) !== right) throw E.NOTCAPABLE;
    return entry;
  }
  insert(entry) {
    let fd = 0;
    while (this.entries.has(fd)) fd++;
    this.entries.set(fd, entry);
    return fd;
  }
  close(fd) {
    const entry = this.get(fd);
    this.entries.delete(fd >>> 0);
    if (entry.hostFd !== null && !entry.stdio) fs.closeSync(entry.hostFd);
  }
  // A path inside directory `fd`; leaving the directory fails with ENOTCAPABLE as in uvwasi.
  resolve(memory, fd, at, length, right = 0n) {
    const entry = this.get(fd, right);
    if (entry.type !== FILETYPE.DIRECTORY) throw E.NOTDIR;
    const text = memory.string(at, length);
    if (text.includes("\0")) throw E.INVAL;
    if (text.startsWith("/")) throw E.NOTCAPABLE;
    const parts = [];
    for (const part of text.split("/")) {
      if (part === "" || part === ".") continue;
      if (part !== "..") parts.push(part);
      else if (parts.pop() === undefined) throw E.NOTCAPABLE;
    }
    return { entry, real: path.join(entry.realPath, ...parts) };
  }
}

function writeFilestat(memory, at, stats) {
  const ns = (value) => (typeof value === "bigint" ? value : BigInt(Math.round(value * 1e6)));
  memory.setU64(at, stats.dev);
  memory.setU64(at + 8, stats.ino);
  memory.setU8(at + 16, filetypeOf(stats));
  memory.setU64(at + 24, stats.nlink);
  memory.setU64(at + 32, stats.size);
  memory.setU64(at + 40, stats.atimeNs ?? ns(stats.atimeMs));
  memory.setU64(at + 48, stats.mtimeNs ?? ns(stats.mtimeMs));
  memory.setU64(at + 56, stats.ctimeNs ?? ns(stats.ctimeMs));
}

// New access and modification times (in seconds) from WASI fstflags, as uvwasi computes them.
function newTimes(stats, atim, mtim, flags) {
  if ((flags & 1 && flags & 2) || (flags & 4 && flags & 8) || flags & ~15) throw E.INVAL;
  const now = Date.now() / 1000;
  const pick = (set, setNow, value, current) => (flags & setNow ? now : flags & set ? Number(BigInt(value)) / 1e9 : current);
  return [pick(1, 2, atim, Number(stats.atimeMs) / 1000), pick(4, 8, mtim, Number(stats.mtimeMs) / 1000)];
}

module.exports = { E, FILETYPE, R, RIGHTS, Memory, Entry, Table, errnoOf, filetypeOf, writeFilestat, newTimes };
