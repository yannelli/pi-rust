mod common;

use common::*;

// Expected values come from running the same script under Node 22. The script uses `sh`.
const CHILD_SCRIPT: &str = r##"
const cp = require("node:child_process");
const { promisify } = require("util");
const fs = require("fs");
const out = {};
const events = [];
const child = cp.spawn("sh", ["-c", "echo out; echo err >&2; exit 3"]);
let stdout = "", stderr = "";
child.stdout.on("data", (c) => (stdout += c));
child.stderr.on("data", (c) => (stderr += c));
child.on("spawn", () => events.push("spawn"));
child.on("exit", (code, signal) => events.push(["exit", code, signal]));
const closed = await new Promise((r) => child.on("close", (code, signal) => r([code, signal])));
out.spawn = [stdout, stderr, closed, events, typeof child.pid, child.exitCode];
const cat = cp.spawn("cat");
let catOut = "";
cat.stdout.setEncoding("utf8");
cat.stdout.on("data", (c) => (catOut += c));
cat.stdin.write("hello ");
cat.stdin.end(Buffer.from("world"));
await new Promise((r) => cat.on("close", r));
out.stdin = catOut;
out.exec = await new Promise((r) => cp.exec("echo $FOO", { env: { ...process.env, FOO: "bar" } }, (e, so, se) => r([e, so, se])));
out.execFail = await new Promise((r) => cp.exec("echo bad >&2; exit 2", (e, so, se) => r([e.code, e.killed, e.signal, e.cmd, e.message, so, se])));
const { stdout: promised } = await promisify(cp.exec)("printf 'a b'");
out.promisified = promised;
try { await promisify(cp.execFile)("sh", ["-c", "exit 4"]); } catch (e) { out.promisifiedFail = [e.code, e.stdout, e.stderr]; }
out.execFile = await new Promise((r) => cp.execFile("sh", ["-c", "echo $0 $1", "x", "y"], (e, so) => r(so)));
const sync = cp.spawnSync("sh", ["-c", "cat; echo x >&2; exit 5"], { input: "in" });
out.spawnSync = [sync.stdout.toString(), sync.stderr.toString(), sync.status, sync.signal, sync.output.length];
out.execSync = cp.execSync("echo hi", { encoding: "utf8" });
try { cp.execSync("exit 7", { stdio: "pipe" }); } catch (e) { out.execSyncFail = [e.status, e.message.split("\n")[0]]; }
out.execFileSync = cp.execFileSync("sh", ["-c", "printf $X"], { env: { X: "env-only" } }).toString();
fs.mkdirSync("sub", { recursive: true });
out.cwd = cp.execSync("basename $(pwd)", { cwd: "sub" }).toString().trim();
out.missing = await new Promise((r) => cp.spawn("definitely-not-a-command-xyz").on("error", (e) => r([e.code, e.syscall, e.path, e.message])));
const missingSync = cp.spawnSync("definitely-not-a-command-xyz");
out.missingSync = [missingSync.error.code, missingSync.status];
const sleeper = cp.spawn("sleep", ["10"]);
setTimeout(() => sleeper.kill(), 20);
out.kill = await new Promise((r) => sleeper.on("exit", (code, signal) => r([code, signal, sleeper.killed])));
out.timeout = await new Promise((r) => cp.exec("sleep 5", { timeout: 50 }, (e) => r([e.killed, e.signal, e.code])));
const timedSync = cp.spawnSync("sleep", ["5"], { timeout: 50 });
out.timeoutSync = [timedSync.error.code, timedSync.signal];
const inherit = cp.spawnSync("sh", ["-c", "echo visible"], { stdio: "inherit" });
out.inherit = [inherit.stdout, inherit.status];
return out;
"##;

const CHILD_EXPECTED: &str = r##"{"spawn":["out\n","err\n",[3,null],["spawn",["exit",3,null]],"number",3],"stdin":"hello world","exec":[null,"bar\n",""],"execFail":[2,false,null,"echo bad >&2; exit 2","Command failed: echo bad >&2; exit 2\nbad\n","","bad\n"],"promisified":"a b","promisifiedFail":[4,"",""],"execFile":"x y\n","spawnSync":["in","x\n",5,null,3],"execSync":"hi\n","execSyncFail":[7,"Command failed: exit 7"],"execFileSync":"env-only","cwd":"sub","missing":["ENOENT","spawn definitely-not-a-command-xyz","definitely-not-a-command-xyz","spawn definitely-not-a-command-xyz ENOENT"],"missingSync":["ENOENT",null],"kill":[null,"SIGTERM",true],"timeout":[true,"SIGTERM",null],"timeoutSync":["ETIMEDOUT","SIGTERM"],"inherit":[null,0]}"##;

#[cfg(unix)]
#[tokio::test]
async fn child_process_matches_node() {
    let (_dir, root) = tempdir();
    let (runtime, capture) = runtime(&root);
    let actual = run_cjs(&runtime, &root, CHILD_SCRIPT).await;
    let expected: serde_json::Value = serde_json::from_str(CHILD_EXPECTED).unwrap();
    assert_eq!(actual, expected);
    assert_eq!(capture.stdout(), "visible\n");
}
