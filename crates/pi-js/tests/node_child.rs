#![cfg(unix)]

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

#[tokio::test]
async fn child_process_matches_node() {
    let (_dir, root) = tempdir();
    let (runtime, capture) = runtime(&root);
    let actual = run_cjs(&runtime, &root, CHILD_SCRIPT).await;
    let expected: serde_json::Value = serde_json::from_str(CHILD_EXPECTED).unwrap();
    assert_eq!(actual, expected);
    assert_eq!(capture.stdout(), "visible\n");
}

// Expected values come from running the same script under Node 22.
const STDIO_SCRIPT: &str = r##"
const cp = require("node:child_process");
const fs = require("fs");
const net = require("net");
const out = {};
const child = cp.spawn("sh", ["-c", "echo three >&3; read line <&4; echo \"got $line\" >&3"], { stdio: ["ignore", "ignore", "ignore", "pipe", "pipe", "ignore", undefined] });
out.shape = [child.stdio.length, child.stdio.map((s) => (s === null ? null : s instanceof net.Socket)), child.stdin, child.stdout, child.stderr];
let three = "";
child.stdio[3].setEncoding("utf8");
child.stdio[3].on("data", (chunk) => {
  three += chunk;
  if (three === "three\n") child.stdio[4].end("hi\n");
});
fs.appendFileSync("stage", "extra pipes\n");
out.extra = await new Promise((resolve) => child.on("close", (code) => resolve([code, three, child.stdio[3].destroyed, child.stdio[4].destroyed])));
fs.writeFileSync("in.txt", "from-file\n");
const input = fs.openSync("in.txt", "r");
const output = fs.openSync("out.txt", "w");
const sync = cp.spawnSync("sh", ["-c", "cat; echo err >&2"], { stdio: [input, output, "pipe"] });
fs.closeSync(input);
fs.closeSync(output);
out.fdSync = [fs.readFileSync("out.txt", "utf8"), sync.stdout, sync.stderr.toString(), sync.output.length, sync.status];
const extraSync = cp.spawnSync("sh", ["-c", "echo x >&3"], { stdio: ["ignore", "ignore", "ignore", "pipe"], encoding: "utf8" });
out.extraSync = extraSync.output;
fs.appendFileSync("stage", "fs stream\n");
const ws = fs.createWriteStream("stream.txt");
await new Promise((resolve) => ws.on("open", resolve));
const viaStream = cp.spawn("sh", ["-c", "echo via-stream; echo fd3 >&3"], { stdio: ["ignore", ws, "ignore", ws] });
out.streamShape = viaStream.stdio.map((s) => s === null);
await new Promise((resolve) => viaStream.on("close", resolve));
await new Promise((resolve) => ws.end(resolve));
out.stream = fs.readFileSync("stream.txt", "utf8");
fs.appendFileSync("stage", "socket\n");
let done;
const finished = new Promise((resolve) => (done = resolve));
const server = net.createServer((socket) => {
  const echo = cp.spawn("sh", ["-c", "read line; echo \"echo:$line\""], { stdio: [socket, socket, "ignore"] });
  out.socketShape = [echo.stdin, echo.stdout, echo.stdio.length];
  echo.on("close", (code) => {
    out.socketCode = code;
    socket.destroy();
    server.close();
  });
});
server.listen(0, "127.0.0.1", () => {
  const client = net.connect(server.address().port, "127.0.0.1", () => client.write("ping\n"));
  let reply = "";
  client.on("data", (chunk) => (reply += chunk));
  client.on("close", () => done(reply));
});
out.socket = await finished;
const errors = [];
for (const stdio of [[{}], ["pipe", "pipe", "pipe", "bogus"], "bogus", [Buffer.from("x")]]) {
  try { cp.spawn("sh", ["-c", "true"], { stdio }); errors.push(null); } catch (error) { errors.push([error.code, error.message]); }
}
out.errors = errors;
return out;
"##;

const STDIO_EXPECTED: &str = r##"{"shape":[7,[null,null,null,true,true,null,null],null,null,null],"extra":[0,"three\ngot hi\n",true,true],"fdSync":["from-file\n",null,"err\n",3,0],"extraSync":[null,null,null,"x\n"],"streamShape":[true,true,true,true],"stream":"via-stream\nfd3\n","socketShape":[null,null,3],"socketCode":0,"socket":"echo:ping\n","errors":[["ERR_INVALID_ARG_VALUE","The argument 'stdio' is invalid. Received {}"],["ERR_INVALID_SYNC_FORK_INPUT","Asynchronous forks do not support Buffer, TypedArray, DataView or string input: 'bogus'"],["ERR_INVALID_ARG_VALUE","The argument 'stdio' is invalid. Received 'bogus'"],["ERR_INVALID_SYNC_FORK_INPUT","Asynchronous forks do not support Buffer, TypedArray, DataView or string input: <Buffer 78>"]]}"##;

#[tokio::test]
async fn child_process_stdio_entries_match_node() {
    let (_dir, root) = tempdir();
    let (runtime, _capture) = runtime(&root);
    let actual = run_cjs_within(&runtime, &root, STDIO_SCRIPT, 60).await;
    assert_same_keys(&actual, &serde_json::from_str(STDIO_EXPECTED).unwrap());
}
