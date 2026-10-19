// The WASI preview1 system calls for node:wasi, in JavaScript over the fs module. Paths stay
// inside their preopened directory, and the results follow uvwasi (the library Node uses).
const fs = require("fs");
const { E, FILETYPE, R, RIGHTS, Entry, Table, errnoOf, filetypeOf, writeFilestat, newTimes } = require("internal/wasi_fds");

const DIRENT_TYPES = [
  ["isFile", FILETYPE.REGULAR_FILE],
  ["isDirectory", FILETYPE.DIRECTORY],
  ["isSymbolicLink", FILETYPE.SYMBOLIC_LINK],
  ["isCharacterDevice", FILETYPE.CHARACTER_DEVICE],
  ["isBlockDevice", 1],
  ["isSocket", FILETYPE.SOCKET_STREAM],
];

function strings(memory, list, pointers, buffer) {
  let at = buffer >>> 0;
  list.forEach((value, index) => {
    const bytes = Buffer.from(value + "\0");
    memory.setU32(pointers + 4 * index, at);
    memory.bytes.set(bytes, at);
    at += bytes.length;
  });
}

function sizes(memory, list, countAt, sizeAt) {
  memory.setU32(countAt, list.length);
  memory.setU32(sizeAt, list.reduce((total, value) => total + Buffer.byteLength(value) + 1, 0));
}

function clockNow(id) {
  switch (id) {
    case 0:
      return BigInt(Date.now()) * 1000000n;
    case 1:
      return process.hrtime.bigint();
    case 2:
    case 3: {
      const usage = process.cpuUsage();
      return BigInt(usage.user + usage.system) * 1000n;
    }
    default:
      throw E.INVAL;
  }
}

// Read into or write from `buffers` at `position` (null for the descriptor's stream position).
function transfer(entry, buffers, position, write) {
  let total = 0;
  for (const buffer of buffers) {
    const at = position === null ? null : position + total;
    const done = write
      ? fs.writeSync(entry.hostFd, buffer, 0, buffer.length, at)
      : fs.readSync(entry.hostFd, buffer, 0, buffer.length, at);
    total += done;
    if (done < buffer.length) break;
  }
  return total;
}

function streamPosition(entry) {
  if (!entry.isFile) return null;
  if (entry.flags & 1) entry.position = Number(fs.fstatSync(entry.hostFd).size);
  return entry.position;
}

function openFlags(oflags, base, fdflags) {
  const c = fs.constants;
  const read = (base & (R.FD_READ | R.FD_READDIR)) !== 0n;
  const write = (base & (R.FD_DATASYNC | R.FD_WRITE | R.FD_ALLOCATE | R.FD_FILESTAT_SET_SIZE)) !== 0n;
  let flags = write ? (read ? c.O_RDWR : c.O_WRONLY) : c.O_RDONLY;
  if (oflags & 1) flags |= c.O_CREAT;
  if (oflags & 4) flags |= c.O_EXCL;
  if (oflags & 8) flags |= c.O_TRUNC;
  if (fdflags & 1) flags |= c.O_APPEND;
  return { flags, write };
}

function createCalls(config, memory) {
  const table = new Table(config.stdio, config.preopens);
  const stat = (real, follow) => (follow ? fs.statSync(real, { bigint: true }) : fs.lstatSync(real, { bigint: true }));

  const calls = {
    args_get: (argv, buffer) => strings(memory, config.args, argv, buffer),
    args_sizes_get: (countAt, sizeAt) => sizes(memory, config.args, countAt, sizeAt),
    environ_get: (environ, buffer) => strings(memory, config.env, environ, buffer),
    environ_sizes_get: (countAt, sizeAt) => sizes(memory, config.env, countAt, sizeAt),
    clock_res_get(id, at) {
      if (id >>> 0 > 3) throw E.INVAL;
      memory.setU64(at, 1000n);
    },
    clock_time_get: (id, precision, at) => memory.setU64(at, clockNow(id >>> 0)),
    fd_advise(fd, offset, length, advice) {
      table.get(fd);
      if (advice >>> 0 > 5) throw E.INVAL;
    },
    fd_allocate(fd, offset, length) {
      const entry = table.get(fd, R.FD_ALLOCATE);
      const end = Number(BigInt(offset) + BigInt(length));
      if (entry.isFile && fs.fstatSync(entry.hostFd).size < end) fs.ftruncateSync(entry.hostFd, end);
    },
    fd_close: (fd) => table.close(fd),
    fd_datasync(fd) {
      const entry = table.get(fd, R.FD_DATASYNC);
      if (entry.isFile) fs.fdatasyncSync(entry.hostFd);
    },
    fd_fdstat_get(fd, at) {
      const entry = table.get(fd);
      memory.setU8(at, entry.type);
      memory.setU16(at + 2, entry.flags);
      memory.setU64(at + 8, entry.base);
      memory.setU64(at + 16, entry.inheriting);
    },
    fd_fdstat_set_flags(fd, flags) {
      table.get(fd, 1n << 3n).flags = flags & 0x1f;
    },
    fd_fdstat_set_rights(fd, base, inheriting) {
      const entry = table.get(fd);
      base = BigInt(base);
      inheriting = BigInt(inheriting);
      if ((entry.base | base) !== entry.base || (entry.inheriting | inheriting) !== entry.inheriting) throw E.NOTCAPABLE;
      Object.assign(entry, { base, inheriting });
    },
    fd_filestat_get(fd, at) {
      const entry = table.get(fd, 1n << 21n);
      try {
        writeFilestat(memory, at, entry.stat());
      } catch (error) {
        if (!entry.stdio) throw error;
        memory.bytes.fill(0, at >>> 0, (at >>> 0) + 64);
        memory.setU8(at + 16, entry.type);
      }
    },
    fd_filestat_set_size(fd, size) {
      const entry = table.get(fd, R.FD_FILESTAT_SET_SIZE);
      if (entry.hostFd === null) throw E.ISDIR;
      fs.ftruncateSync(entry.hostFd, Number(size));
    },
    fd_filestat_set_times(fd, atim, mtim, flags) {
      const entry = table.get(fd, 1n << 23n);
      const times = newTimes(entry.stat(false), atim, mtim, flags);
      if (entry.hostFd !== null) fs.futimesSync(entry.hostFd, ...times);
      else fs.utimesSync(entry.realPath, ...times);
    },
    fd_pread(fd, iovs, count, offset, nreadAt) {
      const entry = table.get(fd, R.FD_READ | R.FD_SEEK);
      if (!entry.isFile) throw entry.hostFd === null ? E.ISDIR : E.SPIPE;
      memory.setU32(nreadAt, transfer(entry, memory.iovecs(iovs, count), Number(offset), false));
    },
    fd_prestat_get(fd, at) {
      const entry = table.get(fd);
      if (entry.preopen === null) throw E.INVAL;
      memory.setU8(at, 0);
      memory.setU32(at + 4, Buffer.byteLength(entry.preopen));
    },
    fd_prestat_dir_name(fd, at, length) {
      const entry = table.get(fd);
      if (entry.preopen === null) throw E.INVAL;
      const name = Buffer.from(entry.preopen);
      if (name.length > length >>> 0) throw E.NOBUFS;
      memory.bytes.set(name, at >>> 0);
    },
    fd_pwrite(fd, iovs, count, offset, nwrittenAt) {
      const entry = table.get(fd, R.FD_WRITE | R.FD_SEEK);
      if (!entry.isFile) throw entry.hostFd === null ? E.ISDIR : E.SPIPE;
      memory.setU32(nwrittenAt, transfer(entry, memory.iovecs(iovs, count), Number(offset), true));
    },
    fd_read(fd, iovs, count, nreadAt) {
      const entry = table.get(fd, R.FD_READ);
      if (entry.hostFd === null) throw E.ISDIR;
      const position = streamPosition(entry);
      const total = transfer(entry, memory.iovecs(iovs, count), position, false);
      if (position !== null) entry.position += total;
      memory.setU32(nreadAt, total);
    },
    fd_readdir(fd, buffer, length, cookie, usedAt) {
      const entry = table.get(fd, R.FD_READDIR);
      if (entry.type !== FILETYPE.DIRECTORY) throw E.NOTDIR;
      const out = [];
      const names = fs.readdirSync(entry.realPath, { withFileTypes: true });
      for (let index = Number(cookie); index < names.length; index++) {
        const name = Buffer.from(names[index].name);
        const header = Buffer.alloc(24);
        header.writeBigUInt64LE(BigInt(index + 1), 0);
        header.writeUInt32LE(name.length, 16);
        header[20] = (DIRENT_TYPES.find(([method]) => names[index][method]()) || [, FILETYPE.UNKNOWN])[1];
        out.push(header, name);
      }
      const bytes = Buffer.concat(out).subarray(0, length >>> 0);
      memory.bytes.set(bytes, buffer >>> 0);
      memory.setU32(usedAt, bytes.length);
    },
    fd_renumber(from, to) {
      const entry = table.get(from);
      table.get(to);
      table.close(to);
      table.entries.delete(from >>> 0);
      table.entries.set(to >>> 0, entry);
    },
    fd_seek(fd, offset, whence, at) {
      const entry = table.get(fd, R.FD_SEEK);
      if (!entry.isFile) throw entry.hostFd === null ? E.ISDIR : E.SPIPE;
      const base = [0, entry.position, Number(fs.fstatSync(entry.hostFd).size)][whence];
      if (base === undefined) throw E.INVAL;
      const position = base + Number(offset);
      if (position < 0) throw E.INVAL;
      entry.position = position;
      memory.setU64(at, position);
    },
    fd_sync(fd) {
      const entry = table.get(fd, 1n << 4n);
      if (entry.isFile) fs.fsyncSync(entry.hostFd);
    },
    fd_tell(fd, at) {
      const entry = table.get(fd, 1n << 5n);
      if (!entry.isFile) throw entry.hostFd === null ? E.ISDIR : E.SPIPE;
      memory.setU64(at, entry.position);
    },
    fd_write(fd, iovs, count, nwrittenAt) {
      const entry = table.get(fd, R.FD_WRITE);
      if (entry.hostFd === null) throw E.ISDIR;
      const position = streamPosition(entry);
      const buffers = memory.iovecs(iovs, count);
      const total = entry.stdio ? fs.writeSync(entry.hostFd, Buffer.concat(buffers)) : transfer(entry, buffers, position, true);
      if (position !== null) entry.position += total;
      memory.setU32(nwrittenAt, total);
    },
    path_create_directory(fd, at, length) {
      fs.mkdirSync(table.resolve(memory, fd, at, length, 1n << 9n).real);
    },
    path_filestat_get(fd, flags, at, length, out) {
      writeFilestat(memory, out, stat(table.resolve(memory, fd, at, length, 1n << 18n).real, flags & 1));
    },
    path_filestat_set_times(fd, flags, at, length, atim, mtim, fstflags) {
      const { real } = table.resolve(memory, fd, at, length, 1n << 20n);
      const stats = stat(real, flags & 1);
      if (stats.isSymbolicLink()) throw E.NOSYS;
      fs.utimesSync(real, ...newTimes(stats, atim, mtim, fstflags));
    },
    path_link(oldFd, oldFlags, oldAt, oldLength, newFd, newAt, newLength) {
      const from = table.resolve(memory, oldFd, oldAt, oldLength, 1n << 11n).real;
      fs.linkSync(from, table.resolve(memory, newFd, newAt, newLength, 1n << 12n).real);
    },
    path_open(fd, dirflags, at, length, oflags, base, inheriting, fdflags, fdAt) {
      const { entry: dir, real } = table.resolve(memory, fd, at, length, 1n << 13n);
      base = BigInt(base);
      inheriting = BigInt(inheriting);
      if ((dir.inheriting | base) !== dir.inheriting || (dir.inheriting | inheriting) !== dir.inheriting) throw E.NOTCAPABLE;
      const { flags, write } = openFlags(oflags, base, fdflags);
      let existing = null;
      try {
        existing = stat(real, dirflags & 1);
      } catch (error) {
        if (error.code !== "ENOENT" || !(oflags & 1)) throw error;
      }
      if (existing && oflags & 1 && oflags & 4) throw E.EXIST;
      if (oflags & 2 && existing && !existing.isDirectory()) throw E.NOTDIR;
      if (existing?.isDirectory()) {
        if (write) throw E.ISDIR;
        const opened = new Entry({ type: FILETYPE.DIRECTORY, realPath: real, flags: fdflags & 0x1f });
        Object.assign(opened, { base: base & RIGHTS.DIRECTORY, inheriting: inheriting & RIGHTS.DIRECTORY_INHERITING });
        return memory.setU32(fdAt, table.insert(opened));
      }
      if (oflags & 2) throw E.NOENT;
      const hostFd = fs.openSync(real, flags);
      const type = existing ? filetypeOf(existing) : FILETYPE.REGULAR_FILE;
      const opened = new Entry({ type, hostFd, realPath: real, flags: fdflags & 0x1f, base: base & RIGHTS.FILE, inheriting: 0n });
      memory.setU32(fdAt, table.insert(opened));
    },
    path_readlink(fd, at, length, buffer, bufferLength, usedAt) {
      const target = Buffer.from(fs.readlinkSync(table.resolve(memory, fd, at, length, 1n << 15n).real));
      const copied = target.subarray(0, bufferLength >>> 0);
      memory.bytes.set(copied, buffer >>> 0);
      memory.setU32(usedAt, copied.length);
    },
    path_remove_directory(fd, at, length) {
      fs.rmdirSync(table.resolve(memory, fd, at, length, 1n << 25n).real);
    },
    path_rename(fd, oldAt, oldLength, newFd, newAt, newLength) {
      const from = table.resolve(memory, fd, oldAt, oldLength, 1n << 16n).real;
      fs.renameSync(from, table.resolve(memory, newFd, newAt, newLength, 1n << 17n).real);
    },
    path_symlink(oldAt, oldLength, fd, newAt, newLength) {
      fs.symlinkSync(memory.string(oldAt, oldLength), table.resolve(memory, fd, newAt, newLength, 1n << 24n).real);
    },
    path_unlink_file(fd, at, length) {
      const { real } = table.resolve(memory, fd, at, length, 1n << 26n);
      if (fs.lstatSync(real).isDirectory()) throw E.ISDIR;
      fs.unlinkSync(real);
    },
    poll_oneoff: (input, output, count, eventsAt) => pollOneoff(memory, table, input, output, count >>> 0, eventsAt),
    proc_raise: () => E.NOSYS,
    random_get(at, length) {
      require("crypto").randomFillSync(memory.slice(at, length));
    },
    sched_yield() {},
    sock_accept(fd) {
      table.get(fd);
      throw E.NOTSUP;
    },
    sock_recv(fd) {
      table.get(fd);
      throw E.NOTSOCK;
    },
    sock_send(fd) {
      table.get(fd);
      throw E.NOTSOCK;
    },
    sock_shutdown(fd) {
      table.get(fd);
      throw E.NOTSOCK;
    },
  };
  for (const [name, fn] of Object.entries(calls)) calls[name] = syscall(memory, fn);
  return calls;
}

// Clock subscriptions sleep until the earliest deadline; descriptors are always ready.
function pollOneoff(memory, table, input, output, count, eventsAt) {
  if (count === 0) throw E.INVAL;
  const events = [];
  let wait = Infinity;
  for (let i = 0; i < count; i++) {
    const at = input + 48 * i;
    const view = memory.view;
    const userdata = view.getBigUint64(at, true);
    const tag = view.getUint8(at + 8);
    if (tag === 0) {
      const timeout = view.getBigUint64(at + 24, true);
      const absolute = view.getUint16(at + 40, true) & 1;
      const delay = absolute ? Number(timeout - clockNow(view.getUint32(at + 16, true))) : Number(timeout);
      wait = Math.min(wait, Math.max(0, delay / 1e6));
      events.push({ userdata, tag, error: 0, at: delay });
    } else {
      let error = 0;
      try {
        table.get(view.getUint32(at + 16, true));
      } catch (errno) {
        error = errno;
      }
      events.push({ userdata, tag, error, at: -1 });
    }
  }
  const ready = events.filter((event) => event.tag !== 0);
  if (ready.length === 0 && wait > 0) sleep(wait);
  const fired = ready.length ? ready : events.filter((event) => event.at / 1e6 <= wait);
  fired.forEach((event, index) => {
    const at = output + 32 * index;
    memory.bytes.fill(0, at, at + 32);
    memory.setU64(at, event.userdata);
    memory.setU16(at + 8, event.error);
    memory.setU8(at + 10, event.tag);
  });
  memory.setU32(eventsAt, fired.length);
}

function sleep(milliseconds) {
  try {
    Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, milliseconds);
  } catch {
    const end = Date.now() + milliseconds;
    while (Date.now() < end);
  }
}

// Run a system call: an errno it throws (or an fs error) becomes its result.
function syscall(memory, fn) {
  return function (...args) {
    if (memory.memory === undefined) {
      throw Object.assign(new Error("wasi.start() has not been called"), { code: "ERR_WASI_NOT_STARTED" });
    }
    try {
      return fn(...args) ?? E.SUCCESS;
    } catch (error) {
      if (error instanceof RangeError) return E.OVERFLOW;
      const errno = errnoOf(error);
      if (errno === undefined) throw error;
      return errno;
    }
  };
}

module.exports = { createCalls };
