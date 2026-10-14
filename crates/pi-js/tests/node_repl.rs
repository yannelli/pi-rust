//! node:repl over stream pairs: evaluation, multi-line input, errors, dot commands, custom eval
//! and writer, useGlobal, and top-level await. Expected values come from Node 22.

mod common;

use common::*;

const SCRIPT: &str = r##"
const repl = require("node:repl");
const { PassThrough } = require("node:stream");
const fs = require("fs");
const path = require("path");
fs.writeFileSync(path.join(__dirname, "load.js"), "const loaded = 3;\nloaded * 2\n");
function open(options) {
  const input = new PassThrough();
  const output = new PassThrough();
  const state = { out: "" };
  output.on("data", (chunk) => (state.out += chunk));
  const server = repl.start({ input, output, ...options });
  state.exited = new Promise((resolve) => server.on("exit", resolve));
  return { input, server, state };
}
async function session(options, lines, setup) {
  const { input, server, state } = open(options);
  if (setup) setup(server);
  for (const line of lines) input.write(line + "\n");
  input.end();
  await state.exited;
  return state.out;
}
const out = {};
out.basic = await session({ prompt: "> " }, [
  "1 + 2", "answer", "let x = 5", "x * 2", "_", "function f() {", "  return 7;", "}", "f()",
  "throw new TypeError(\"top\")", "_error.message", "throw 5", "throw { a: 1 }", "undefined", "\"str\"",
  "({ a: 1, b: [1, 2] })", "{ a: 1 }", "{ a: 1 };", "[1, 2].map((v) => v * 2)", "var v = 1; v + 1",
  "typeof fs.readFileSync", "typeof require", "typeof module", "`multi", "line`", "", ".break", "nope_" + "missing",
  ".help", ".exit",
], (server) => (server.context.answer = 42));
out.custom = await session({ prompt: "", eval: (cmd, ctx, file, cb) => cb(null, { cmd, file: file.replace(/\d+/, "N"), ctx: typeof ctx }), writer: (o) => "W:" + JSON.stringify(o) }, ["hello", "second line"]);
let pending = "";
out.recoverable = await session({ prompt: "> ", eval: (cmd, ctx, file, cb) => {
  pending += cmd;
  if (!pending.includes(";")) return cb(new repl.Recoverable(new Error("more")));
  const done = pending;
  pending = "";
  cb(null, done);
} }, ["a", "b;", "c;"]);
out.ignore = await session({ prompt: "> ", ignoreUndefined: true }, ["undefined", "void 0", "1"]);
out.commands = (await session({ prompt: "> " }, ["let keep = 1", ".clear", "typeof keep", `.load ${path.join(__dirname, "load.js")}`, `.save ${path.join(__dirname, "saved.js")}`, ".nope", "1 +", ".break", "2"])).split(__dirname).join("DIR");
out.saved = fs.readFileSync(path.join(__dirname, "saved.js"), "utf8");
out.define = await session({ prompt: "> " }, [".sayhi Bob", ".help"], (server) => server.defineCommand("sayhi", {
  help: "Say hello",
  action(name) {
    this.clearBufferedCommand();
    this.output.write(`Hello, ${name}!\n`);
    this.displayPrompt();
  },
}));
globalThis.fromOutside = "outside";
let globalServer;
out.useGlobal = [await session({ prompt: "> ", useGlobal: true }, ["fromOutside", "var madeInside = 8"], (server) => (globalServer = server)), globalServer.context === globalThis, globalThis.madeInside];
const resets = [];
out.reset = await session({ prompt: "> " }, ["var r = 1", ".clear", "typeof r"], (server) => server.on("reset", (context) => resets.push(typeof context)));
out.resets = resets;
const waiting = open({ prompt: "> " });
for (const line of ["const awaited = await Promise.resolve(9)", "awaited + 1", "await new Promise((resolve) => setTimeout(() => resolve('later'), 5))"]) {
  const before = waiting.state.out.length;
  waiting.input.write(line + "\n");
  while (!(waiting.state.out.length > before && waiting.state.out.endsWith("> "))) await new Promise((resolve) => setTimeout(resolve, 1));
}
waiting.input.end();
await waiting.state.exited;
out.await = waiting.state.out;
out.api = [typeof repl.REPLServer, typeof repl.Recoverable, String(repl.REPL_MODE_SLOPPY), String(repl.REPL_MODE_STRICT), repl.builtinModules.includes("fs"), repl.writer({ a: [1] }), new repl.Recoverable(new Error("e")) instanceof SyntaxError];
return out;
"##;

const EXPECTED: &str = r##"{"basic":"> 3\n> 42\n> undefined\n> 10\n> 10\n> ... ... undefined\n> 7\n> Uncaught TypeError: top\n> 'top'\n> Uncaught 5\n> Uncaught { a: 1 }\n> undefined\n> 'str'\n> { a: 1, b: [ 1, 2 ] }\n> { a: 1 }\n> 1\n> [ 2, 4 ]\n> 2\n> 'function'\n> 'function'\n> 'object'\n> ... 'multi\\nline'\n> > > Uncaught ReferenceError: nope_missing is not defined\n> .break   Sometimes you get stuck, this gets you out\n.clear   Break, and also clear the local context\n.exit    Exit the REPL\n.help    Print this help message\n.load    Load JS from a file into the REPL session\n.save    Save all evaluated commands in this REPL session to a file\n\nPress Ctrl+C to abort current expression, Ctrl+D to exit the REPL\n> ","custom":"W:{\"cmd\":\"hello\\n\",\"file\":\"REPLN\",\"ctx\":\"object\"}\nW:{\"cmd\":\"second line\\n\",\"file\":\"REPLN\",\"ctx\":\"object\"}\n","recoverable":"> ... 'a\\na\\nb;\\n'\n> 'c;\\n'\n> ","ignore":"> > > 1\n> ","commands":"> undefined\n> Clearing context...\n> 'undefined'\n> 6\n> > Session saved to: DIR/saved.js\n> Invalid REPL keyword\n> ... > 2\n> ","saved":"typeof keep\nconst loaded = 3;\nloaded * 2\n","define":"> Hello, Bob!\n> .break   Sometimes you get stuck, this gets you out\n.clear   Break, and also clear the local context\n.exit    Exit the REPL\n.help    Print this help message\n.load    Load JS from a file into the REPL session\n.save    Save all evaluated commands in this REPL session to a file\n.sayhi   Say hello\n\nPress Ctrl+C to abort current expression, Ctrl+D to exit the REPL\n> ","useGlobal":["> 'outside'\n> undefined\n> ",true,8],"reset":"> undefined\n> Clearing context...\n> 'undefined'\n> ","resets":["object"],"await":"> undefined\n> 10\n> 'later'\n> ","api":["function","function","Symbol(repl-sloppy)","Symbol(repl-strict)",true,"{ a: [ 1 ] }",true]}"##;

#[tokio::test]
async fn repl_sessions_match_node() {
    let (_dir, root) = tempdir();
    let (runtime, _capture) = runtime(&root);
    let actual = run_cjs(&runtime, &root, SCRIPT).await;
    let expected: serde_json::Value = serde_json::from_str(EXPECTED).unwrap();
    assert_same_keys(&actual, &expected);
}

#[tokio::test]
async fn repl_errors_and_unsupported_list() {
    let (_dir, root) = tempdir();
    let (runtime, _capture) = runtime(&root);
    let actual = run_cjs(
        &runtime,
        &root,
        r#"
const repl = require("repl");
const { PassThrough } = require("stream");
const input = new PassThrough();
const output = new PassThrough();
let out = "";
output.on("data", (chunk) => (out += chunk));
const server = repl.start({ input, output, prompt: "> " });
const exited = new Promise((resolve) => server.on("exit", resolve));
input.write("function g() { throw new RangeError('in g'); }\n");
input.write("g()\n");
input.write("_error instanceof RangeError\n");
input.end();
await exited;
const lines = out.split("\n");
return [lines[1], /^    at g \(REPL\d+:1:\d+\)$/.test(lines[2]), lines[3], lines.length];
"#,
    )
    .await;
    assert_eq!(actual, serde_json::json!(["> Uncaught RangeError: in g", true, "> true", 5]));
}
