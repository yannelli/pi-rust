// node:wasi cases; wasi.txt is this script's stdout under Node 22. The Wasm module writes with
// fd_write and then calls `env.run`, which drives the other system calls through wasiImport.
const { WASI } = require("wasi");
const fs = require("fs");
const path = require("path");
const { ModuleBuilder, op } = require("./builder");

const root = path.join(__dirname, "sandbox");
fs.mkdirSync(path.join(root, "sub"), { recursive: true });
fs.writeFileSync(path.join(root, "in.txt"), "file contents\n");
const RIGHTS = 0xfffffffn;

function program(body) {
  const m = new ModuleBuilder();
  const fdWrite = m.importFunc("wasi_snapshot_preview1", "fd_write", ["i32", "i32", "i32", "i32"], ["i32"]);
  const exit = m.importFunc("wasi_snapshot_preview1", "proc_exit", ["i32"], []);
  const run = m.importFunc("env", "run", [], []);
  m.memory(1);
  m.export("memory", "memory", 0);
  m.data(0, [16, 0, 0, 0, 6, 0, 0, 0]);
  m.data(16, "hello\n");
  m.func([], [], body({ fdWrite, exit, run }), { export: "_start" });
  return new WebAssembly.Module(m.build());
}

const hello = program(({ fdWrite, run }) => [op.i32_const(1), op.i32_const(0), op.i32_const(1), op.i32_const(8), op.call(fdWrite), op.drop, op.call(run)]);
const wasi = new WASI({
  version: "preview1",
  args: ["prog", "a b", "ü"],
  env: { HOME: "/home/x", EMPTY: "", SKIPPED: undefined },
  preopens: { "/sandbox": root, rel: path.join(root, "sub") },
});

function run() {
  const w = wasi.wasiImport;
  const memory = instance.exports.memory;
  const view = () => new DataView(memory.buffer);
  const u32 = (at) => view().getUint32(at, true);
  const u64 = (at) => view().getBigUint64(at, true);
  const text = (at, length) => JSON.stringify(Buffer.from(memory.buffer, at, length).toString());
  const put = (at, value) => {
    const bytes = Buffer.from(value);
    new Uint8Array(memory.buffer).set(bytes, at);
    return bytes.length;
  };
  const iovec = (at, pointer, length) => {
    view().setUint32(at, pointer, true);
    view().setUint32(at + 4, length, true);
  };
  const log = (...args) => console.log(...args);
  log("args_sizes_get", w.args_sizes_get(100, 104), u32(100), u32(104));
  log("args_get", w.args_get(200, 300), [0, 1, 2].map((i) => u32(200 + 4 * i) - 300), text(300, 12));
  log("environ_sizes_get", w.environ_sizes_get(100, 104), u32(100), u32(104));
  log("environ_get", w.environ_get(200, 300), text(300, 20));
  log("clock_res_get", w.clock_res_get(0, 100), u64(100) > 0n, w.clock_res_get(7, 100));
  log("clock_time_get", w.clock_time_get(0, 1000n, 100), u64(100) > 1600000000000000000n, w.clock_time_get(9, 1000n, 100));
  log("random_get", w.random_get(400, 16), w.random_get(65530, 16));
  log("fd_prestat_get", w.fd_prestat_get(3, 100), view().getUint8(100), u32(104), w.fd_prestat_get(5, 100), w.fd_prestat_get(1, 100));
  log("fd_prestat_dir_name", w.fd_prestat_dir_name(3, 120, 8), text(120, 8), w.fd_prestat_dir_name(4, 120, 3), text(120, 3));
  log("fd_prestat_dir_name short", w.fd_prestat_dir_name(3, 120, 2));
  log("fd_fdstat_get dir", w.fd_fdstat_get(3, 100), view().getUint8(100), u64(108), u64(116));
  log("path_open rights", w.path_open(3, 1, 500, put(500, "in.txt"), 0, 0x1fffffffn, RIGHTS, 0, 600));
  log("path_open", w.path_open(3, 1, 500, put(500, "in.txt"), 0, RIGHTS, RIGHTS, 0, 600), u32(600));
  const fd = u32(600);
  log("fd_fdstat_get file", w.fd_fdstat_get(fd, 100), view().getUint8(100));
  iovec(700, 800, 5);
  iovec(708, 900, 100);
  log("fd_read", w.fd_read(fd, 700, 2, 720), u32(720), text(800, 5), text(900, 9));
  log("fd_read eof", w.fd_read(fd, 700, 1, 720), u32(720));
  log("fd_seek", w.fd_seek(fd, 5n, 0, 730), u64(730), w.fd_tell(fd, 730), u64(730));
  log("fd_seek end", w.fd_seek(fd, -3n, 2, 730), u64(730), w.fd_seek(fd, -30n, 1, 730));
  log("fd_pread", w.fd_pread(fd, 700, 1, 0n, 720), u32(720), text(800, 5));
  log("fd_filestat_get", w.fd_filestat_get(fd, 1000), view().getUint8(1016), u64(1032));
  log("fd_close", w.fd_close(fd), w.fd_close(fd));
  const out = put(500, "out.txt");
  log("path_open create", w.path_open(3, 0, 500, out, 1 | 8, RIGHTS, RIGHTS, 0, 600));
  const outFd = u32(600);
  iovec(700, 800, put(800, "written!\n"));
  log("fd_write", w.fd_write(outFd, 700, 1, 720), u32(720));
  log("fd_pwrite", w.fd_pwrite(outFd, 700, 1, 2n, 720), u32(720));
  log("fd_filestat_set_size", w.fd_filestat_set_size(outFd, 4n));
  log("fd_sync", w.fd_sync(outFd), w.fd_datasync(outFd));
  log("fd_close", w.fd_close(outFd), JSON.stringify(fs.readFileSync(path.join(root, "out.txt"), "utf8")));
  log("path_open exclusive", w.path_open(3, 0, 500, out, 1 | 4, RIGHTS, RIGHTS, 0, 600));
  log("path_open missing", w.path_open(3, 0, 500, put(500, "nope.txt"), 0, RIGHTS, RIGHTS, 0, 600));
  log("path_open escape", w.path_open(3, 0, 500, put(500, "../x"), 0, RIGHTS, RIGHTS, 0, 600));
  log("path_open absolute", w.path_open(3, 0, 500, put(500, "/etc/passwd"), 0, RIGHTS, RIGHTS, 0, 600));
  log("path_open dir for write", w.path_open(3, 0, 500, put(500, "sub"), 2, RIGHTS, RIGHTS, 0, 600));
  log("path_open dir", w.path_open(3, 0, 500, put(500, "sub/."), 2, 1n << 14n, 0n, 0, 600), u32(600));
  log("fd_close dir", w.fd_close(u32(600)));
  log("path_open not dir", w.path_open(3, 0, 500, put(500, "in.txt"), 2, RIGHTS, RIGHTS, 0, 600));
  log("path_create_directory", w.path_create_directory(3, 500, put(500, "made")), fs.statSync(path.join(root, "made")).isDirectory());
  log("path_create_directory again", w.path_create_directory(3, 500, put(500, "made")));
  log("path_filestat_get", w.path_filestat_get(3, 1, 500, put(500, "in.txt"), 1000), view().getUint8(1016), u64(1032));
  log("path_filestat_get dir", w.path_filestat_get(3, 1, 500, put(500, "sub"), 1000), view().getUint8(1016));
  log("path_rename", w.path_rename(3, 500, put(500, "out.txt"), 3, 550, put(550, "moved.txt")), fs.existsSync(path.join(root, "moved.txt")));
  log("path_unlink_file", w.path_unlink_file(3, 550, put(550, "moved.txt")), w.path_unlink_file(3, 550, put(550, "moved.txt")));
  log("path_remove_directory", w.path_remove_directory(3, 500, put(500, "made")));
  log("path_link", w.path_link(3, 0, 500, put(500, "in.txt"), 3, 550, put(550, "hard.txt")), fs.readFileSync(path.join(root, "hard.txt"), "utf8").length);
  log("path_unlink_file link", w.path_unlink_file(3, 550, put(550, "hard.txt")));
  log("fd_readdir", w.fd_readdir(3, 2000, 512, 0n, 720));
  const names = [];
  for (let at = 2000; at + 24 <= 2000 + u32(720); at += 24 + u32(at + 16)) {
    names.push(`${Buffer.from(memory.buffer, at + 24, u32(at + 16))}:${view().getUint8(at + 20)}`);
  }
  log("readdir names", names.sort().join(","));
  log("fd_readdir small", w.fd_readdir(3, 2000, 10, 0n, 720), u32(720));
  log("fd_renumber", w.fd_renumber(3, 4), w.fd_prestat_get(4, 100), w.fd_prestat_get(3, 100));
  log("sched_yield", w.sched_yield(), w.poll_oneoff(0, 0, 0, 720));
  log("bad fd", w.fd_write(99, 0, 0, 720), w.fd_close(99), w.fd_read(99, 0, 0, 720));
  log("out of bounds", w.args_sizes_get(65535, 0));
  log("keys", Object.keys(w).length);
}

const instance = new WebAssembly.Instance(hello, { ...wasi.getImportObject(), env: { run } });
console.log("start", wasi.start(instance));
try {
  wasi.start(instance);
} catch (error) {
  console.log(error.code, error.message);
}

const exiting = program(({ exit }) => [op.i32_const(3), op.call(exit), op.unreachable]);
const second = new WASI({ version: "unstable" });
console.log("import object", Object.keys(second.getImportObject()));
const third = new WASI({ version: "preview1" });
console.log("proc_exit", third.start(new WebAssembly.Instance(exiting, { ...third.getImportObject(), env: { run() {} } })));

function check(label, fn) {
  try {
    console.log(label, fn());
  } catch (error) {
    console.log(label, error.name, error.code, error.message);
  }
}
check("no version", () => new WASI());
check("bad version", () => new WASI({ version: "x" }));
check("args", () => new WASI({ version: "preview1", args: "x" }));
check("stdin", () => new WASI({ version: "preview1", stdin: -1 }));
check("returnOnExit", () => new WASI({ version: "preview1", returnOnExit: 1 }));
check("not started", () => new WASI({ version: "preview1" }).wasiImport.args_sizes_get(0, 4));
check("start number", () => new WASI({ version: "preview1" }).start(5));
check("no memory", () => new WASI({ version: "preview1" }).start({ exports: {} }));
const memory = new WebAssembly.Memory({ initial: 1 });
check("no _start", () => new WASI({ version: "preview1" }).start({ exports: { memory } }));
check("with _initialize", () => new WASI({ version: "preview1" }).start({ exports: { memory, _start() {}, _initialize() {} } }));
check("initialize with _start", () => new WASI({ version: "preview1" }).initialize({ exports: { memory, _start() {} } }));
check("initialize", () => new WASI({ version: "preview1" }).initialize({ exports: { memory, _initialize: () => console.log("init ran") } }));
