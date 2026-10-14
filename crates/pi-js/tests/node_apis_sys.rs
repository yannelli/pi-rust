//! Node APIs backed by the operating system: fs.glob, fs.cp filters, bigint Stats, fs.watch,
//! os priorities and interfaces, and process resource usage.

mod common;

use common::*;
use serde_json::json;

#[tokio::test]
async fn fs_glob_cp_filter_and_bigint_stats() {
    let (_dir, root) = tempdir();
    let (runtime, _capture) = runtime(&root);
    for file in [
        "src/a.js",
        "src/b.ts",
        "src/lib/c.js",
        "src/.hidden/d.js",
        "docs/readme.md",
        "tree/keep.txt",
        "tree/skip.log",
        "tree/sub/deep.txt",
    ] {
        write(&root, file, "x");
    }
    let actual = run_cjs(
        &runtime,
        &root,
        r#"
const fs = require("fs");
const path = require("path");
process.chdir(__dirname);
const sorted = (list) => list.map((p) => p.split(path.sep).join("/")).sort();
const sync = sorted(fs.globSync("src/**/*.js"));
const braces = sorted(fs.globSync(["src/*.{js,ts}", "docs/*"]));
const excluded = sorted(fs.globSync("src/**/*.js", { exclude: (p) => p.endsWith("c.js") }));
const cwd = sorted(fs.globSync("*.js", { cwd: path.join(__dirname, "src") }));
const dirents = fs.globSync("docs/*.md", { withFileTypes: true }).map((d) => [d.name, d.isFile(), d.parentPath === path.join(__dirname, "docs")]);
const viaCallback = await new Promise((resolve, reject) => fs.glob("src/lib/*", (err, matches) => (err ? reject(err) : resolve(sorted(matches)))));
const viaPromise = [];
for await (const entry of fs.promises.glob("**/*.md")) viaPromise.push(entry);
const absolute = fs.globSync(path.join(__dirname, "docs", "*.md")).map((p) => p === path.join(__dirname, "docs", "readme.md"));
const seen = [];
fs.cpSync("tree", "copy", { recursive: true, filter: (src, dest) => { seen.push(path.relative(__dirname, src)); return !src.endsWith(".log"); } });
await fs.promises.cp("tree", "copy2", { recursive: true, filter: async (src) => !src.endsWith("sub") });
let syncPromiseError;
try { fs.cpSync("tree", "copy3", { recursive: true, filter: async () => true }); } catch (e) { syncPromiseError = e.code; }
const big = fs.statSync("docs/readme.md", { bigint: true });
const plain = fs.statSync("docs/readme.md");
const lbig = await fs.promises.lstat("docs", { bigint: true });
return {
  sync, braces, excluded, cwd, dirents, viaCallback, viaPromise: sorted(viaPromise), absolute,
  copied: sorted(fs.readdirSync("copy", { recursive: true })), copied2: sorted(fs.readdirSync("copy2", { recursive: true })),
  seen: sorted(seen), syncPromiseError,
  big: [typeof big.size, big.size === 1n, typeof big.mtimeNs, big.mtimeMs === BigInt(Math.floor(plain.mtimeMs)), big.mtime instanceof Date, big.isFile(), lbig.isDirectory(), big instanceof fs.Stats, "mtimeNs" in plain],
  globError: (() => { try { fs.globSync(1); } catch (e) { return e.code; } })(),
};
"#,
    )
    .await;
    assert_eq!(actual["sync"], json!(["src/a.js", "src/lib/c.js"]));
    assert_eq!(actual["braces"], json!(["docs/readme.md", "src/a.js", "src/b.ts"]));
    assert_eq!(actual["excluded"], json!(["src/a.js"]));
    assert_eq!(actual["cwd"], json!(["a.js"]));
    assert_eq!(actual["dirents"], json!([["readme.md", true, true]]));
    assert_eq!(actual["viaCallback"], json!(["src/lib/c.js"]));
    assert_eq!(actual["viaPromise"], json!(["docs/readme.md"]));
    assert_eq!(actual["absolute"], json!([true]));
    assert_eq!(actual["copied"], json!(["keep.txt", "sub", "sub/deep.txt"]));
    assert_eq!(actual["copied2"], json!(["keep.txt", "skip.log"]));
    assert_eq!(actual["seen"], json!(["tree", "tree/keep.txt", "tree/skip.log", "tree/sub", "tree/sub/deep.txt"]));
    assert_eq!(actual["syncPromiseError"], "ERR_INVALID_RETURN_VALUE");
    assert_eq!(actual["big"], json!(["bigint", true, "bigint", true, true, true, true, false, false]));
    assert_eq!(actual["globError"], "ERR_INVALID_ARG_TYPE");
}

#[tokio::test]
async fn os_priority_interfaces_and_process_usage() {
    let (_dir, root) = tempdir();
    let (runtime, _capture) = runtime(&root);
    let actual = run_cjs(
        &runtime,
        &root,
        r#"
const os = require("os");
const tty = require("tty");
const { spawn } = require("child_process");
const err = (fn) => { try { fn(); return "no error"; } catch (e) { return [e.code, e.name === "SystemError" || e.info !== undefined]; } };
const interfaces = os.networkInterfaces();
const all = Object.values(interfaces).flat();
const loopback = all.find((entry) => entry.internal && entry.family === "IPv4");
let child;
let childPriority = null;
if (process.platform !== "win32") {
  child = spawn("sleep", ["5"]);
  await new Promise((resolve) => child.on("spawn", resolve));
  os.setPriority(child.pid, 12);
  childPriority = os.getPriority(child.pid);
  child.kill();
}
const usage = process.cpuUsage();
let spin = 0;
for (let i = 0; i < 2e5; i++) spin += i;
const delta = process.cpuUsage(usage);
const resources = process.resourceUsage();
return {
  shape: all.every((entry) => ["address", "netmask", "family", "mac", "internal", "cidr"].every((key) => key in entry) && (entry.family === "IPv4" || typeof entry.scopeid === "number")),
  loopback: loopback && [loopback.address, loopback.netmask, loopback.cidr, loopback.mac],
  priority: typeof os.getPriority(), childPriority,
  priorityErrors: [err(() => os.getPriority(2 ** 31 - 2)), err(() => os.setPriority(0, 20)), err(() => os.getPriority("1"))],
  usage: [usage.user >= 0, usage.system >= 0, delta.user >= 0 && delta.system >= 0, spin > 0],
  usageErrors: [err(() => process.cpuUsage(1)), err(() => process.cpuUsage({ user: "1", system: 0 })), err(() => process.cpuUsage({ user: -1, system: 0 }))],
  resources: [Object.keys(resources).length, resources.maxRSS > 0, resources.userCPUTime >= 0],
  tty: [typeof tty.isatty(0), err(() => new tty.ReadStream(5))[0], typeof process.abort],
};
"#,
    )
    .await;
    assert_eq!(actual["shape"], true);
    if cfg!(unix) {
        assert_eq!(actual["loopback"], json!(["127.0.0.1", "255.0.0.0", "127.0.0.1/8", "00:00:00:00:00:00"]));
        assert_eq!(actual["childPriority"], 12);
    }
    assert_eq!(actual["priority"], "number");
    assert_eq!(
        actual["priorityErrors"],
        json!([["ERR_SYSTEM_ERROR", true], ["ERR_OUT_OF_RANGE", false], ["ERR_INVALID_ARG_TYPE", false]])
    );
    assert_eq!(actual["usage"], json!([true, true, true, true]));
    assert_eq!(
        actual["usageErrors"],
        json!([["ERR_INVALID_ARG_TYPE", false], ["ERR_INVALID_ARG_TYPE", false], ["ERR_INVALID_ARG_VALUE", false]])
    );
    assert_eq!(actual["resources"], json!([16, true, true]));
    assert_eq!(actual["tty"], json!(["boolean", "ERR_TTY_INIT_FAILED", "function"]));
}

#[tokio::test]
async fn fs_watch_reports_native_events() {
    let (_dir, root) = tempdir();
    let (runtime, _capture) = runtime(&root);
    write(&root, "watched/existing.txt", "a");
    let actual = run_cjs(
        &runtime,
        &root,
        r#"
const fs = require("fs");
const path = require("path");
const dir = path.join(__dirname, "watched");
const events = [];
const watcher = fs.watch(dir);
const wait = (predicate) => new Promise((resolve) => {
  const check = () => (predicate() ? resolve() : setTimeout(check, 10));
  check();
});
watcher.on("change", (type, name) => events.push([type, name]));
await new Promise((resolve) => setTimeout(resolve, 50));
fs.writeFileSync(path.join(dir, "new.txt"), "x");
await wait(() => events.some(([type, name]) => type === "rename" && name === "new.txt"));
fs.appendFileSync(path.join(dir, "existing.txt"), "more");
await wait(() => events.some(([type, name]) => type === "change" && name === "existing.txt"));
const closed = new Promise((resolve) => watcher.on("close", () => resolve("closed")));
watcher.close();
let missing;
try { fs.watch(path.join(dir, "missing")); } catch (e) { missing = [e.code, e.syscall]; }
const controller = new AbortController();
const iterator = fs.promises.watch(path.join(dir, "existing.txt"), { signal: controller.signal });
setTimeout(() => fs.appendFileSync(path.join(dir, "existing.txt"), "!"), 50);
let first;
let aborted;
try {
  for await (const event of iterator) {
    first = event;
    controller.abort();
  }
} catch (e) {
  aborted = e.name;
}
const quiet = fs.watch(dir, { persistent: false });
quiet.close();
return { closed: await closed, missing, first: [first.eventType, first.filename], aborted, unref: quiet instanceof fs.FSWatcher };
"#,
    )
    .await;
    assert_eq!(actual["closed"], "closed");
    assert_eq!(actual["missing"], json!(["ENOENT", "watch"]));
    assert_eq!(actual["first"], json!(["change", "existing.txt"]));
    assert_eq!(actual["aborted"], "AbortError");
    assert_eq!(actual["unref"], true);
}

#[tokio::test]
async fn os_cpus_report_times() {
    let (_dir, root) = tempdir();
    let (runtime, _capture) = runtime(&root);
    let actual = run_cjs_within(
        &runtime,
        &root,
        r#"
const os = require("os");
const cpus = os.cpus();
const total = (cpu) => Object.values(cpu.times).reduce((sum, value) => sum + value, 0);
return {
  keys: cpus.every((cpu) => JSON.stringify(Object.keys(cpu)) === '["model","speed","times"]' && JSON.stringify(Object.keys(cpu.times)) === '["user","nice","sys","idle","irq"]'),
  integers: cpus.every((cpu) => Object.values(cpu.times).every((value) => Number.isInteger(value) && value >= 0)),
  counted: cpus.some((cpu) => total(cpu) > 0),
  count: cpus.length >= os.availableParallelism(),
};
"#,
        60,
    )
    .await;
    assert_eq!(actual, json!({ "keys": true, "integers": true, "counted": true, "count": true }));
}
