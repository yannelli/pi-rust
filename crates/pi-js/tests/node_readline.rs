//! readline terminal mode (raw input, keypress events, line editing, and history) and the REPL
//! on top of it. Expected values, including the bytes written to the output, come from Node 22.

mod common;

use common::*;

const READLINE_SCRIPT: &str = r##"
const readline = require("readline");
const { PassThrough, Writable } = require("stream");
const out = {};
  const input = new PassThrough();
  const raw = [];
  input.isTTY = true;
  input.isRaw = false;
  input.setRawMode = (mode) => { raw.push(mode); input.isRaw = mode; };
  let written = "";
  const output = new Writable({ write(chunk, enc, cb) { written += chunk.toString(); cb(); } });
  output.isTTY = true;
  output.columns = 80;
  const rl = readline.createInterface({ input, output, prompt: "> ", history: ["older"], historySize: 3 });
  const lines = [];
  const histories = [];
  rl.on("line", (line) => lines.push(line));
  rl.on("history", (h) => histories.push([...h]));
  out.terminal = [rl.terminal, raw.slice(), rl.historySize];
  const send = async (data) => { input.write(data); await new Promise((r) => setImmediate(r)); };
  rl.prompt();
  await send("hello");
  await send("\x1b[D\x1b[D");
  await send("X");
  await send("\x1b[H");
  await send("<");
  await send("\x1b[F");
  await send("\x7f");
  await send("\x1b[3~");
  await send("\x1b[D\x1b[3~");
  await send("\r");
  out.afterFirst = [lines.slice(), rl.line, rl.cursor, rl.getCursorPos()];
  await send("second word\r");
  await send("third\r");
  await send("fourth\r");
  out.history = [rl.history.slice(), histories.length, histories[0]];
  await send("\x1b[A\x1b[A");
  out.up = [rl.line, rl.cursor];
  await send("\x1b[B");
  out.down = rl.line;
  await send("\x1b[B\x1b[B");
  out.downEmpty = rl.line;
  await send("ab cd ef");
  await send("\x17");
  out.ctrlW = rl.line;
  await send("\x01");
  out.ctrlA = rl.cursor;
  await send("\x05\x1bb");
  out.metaB = rl.cursor;
  await send("\x0b");
  out.ctrlK = rl.line;
  await send("\x15");
  out.ctrlU = rl.line;
  await send("é漢\x1b[D");
  out.wide = [rl.line, rl.cursor, rl.getCursorPos()];
  await send("\r");
  const answer = new Promise((resolve) => rl.question("name? ", resolve));
  await send("bob\r");
  out.answer = [await answer, rl.getPrompt()];
  const keys = [];
  const kin = new PassThrough();
  readline.emitKeypressEvents(kin);
  kin.on("keypress", (s, key) => keys.push([s === undefined ? null : s, key.name, key.ctrl, key.meta, key.shift, key.sequence, key.code === undefined ? null : key.code]));
  kin.write("a\x1b[A\x1b[1;5C\x03\x1bf\x1b[3~ Z\x1b[24~\r\t\x7f");
  await new Promise((r) => setImmediate(r));
  out.keys = keys;
  let sigint = 0;
  rl.on("SIGINT", () => sigint++);
  await send("\x03");
  out.sigint = sigint;
  await send("\x04");
  out.closed = [rl.closed, raw];
  out.lines = lines;
  out.written = written;
return out;
"##;

const READLINE_EXPECTED: &str = r##"{"terminal":[true,[true],3],"afterFirst":[["<helX"],"",0,{"cols":2,"rows":0}],"history":[["fourth","third","second word"],4,["<helX","older"]],"up":["third",5],"down":"fourth","downEmpty":"","ctrlW":"ab cd ","ctrlA":0,"metaB":3,"ctrlK":"ab ","ctrlU":"","wide":["é漢",1,{"cols":3,"rows":0}],"answer":["bob","> "],"keys":[["a","a",false,false,false,"a",null],[null,"up",false,false,false,"\u001b[A","[A"],[null,"right",true,false,false,"\u001b[1;5C","[C"],["\u0003","c",true,false,false,"\u0003",null],[null,"f",false,true,false,"\u001bf",null],[null,"delete",false,false,false,"\u001b[3~","[3~"],[" ","space",false,false,false," ",null],["Z","z",false,false,true,"Z",null],[null,"f12",false,false,false,"\u001b[24~","[24~"],["\r","return",false,false,false,"\r",null],["\t","tab",false,false,false,"\t",null],["","backspace",false,false,false,"",null]],"sigint":1,"closed":[true,[true,false]],"lines":["<helX","second word","third","fourth","é漢"],"written":"\u001b[1G\u001b[0J> \u001b[3Ghello\u001b[1D\u001b[1D\u001b[1G\u001b[0J> helXlo\u001b[7G\u001b[4D\u001b[1G\u001b[0J> <helXlo\u001b[4G\u001b[6C\u001b[1G\u001b[0J> <helXl\u001b[9G\u001b[1D\u001b[1G\u001b[0J> <helX\u001b[8G\r\nsecond word\r\nthird\r\nfourth\r\n\u001b[1G\u001b[0J> fourth\u001b[9G\u001b[1G\u001b[0J> third\u001b[8G\u001b[1G\u001b[0J> fourth\u001b[9G\u001b[1G\u001b[0J> \u001b[3Gab cd ef\u001b[1G\u001b[0J> ab cd \u001b[9G\u001b[6D\u001b[6C\u001b[3D\u001b[1G\u001b[0J> ab \u001b[6G\u001b[1G\u001b[0J> \u001b[3Gé漢\u001b[2D\u001b[2C\r\n\u001b[1G\u001b[0Jname? \u001b[7Gbob\r\n"}"##;

#[tokio::test]
async fn readline_terminal_editing_matches_node() {
    let (_dir, root) = tempdir();
    let (runtime, _capture) = runtime(&root);
    let actual = run_cjs_within(&runtime, &root, READLINE_SCRIPT, 30).await;
    assert_same_keys(&actual, &serde_json::from_str(READLINE_EXPECTED).unwrap());
}

const REPL_SCRIPT: &str = r##"
const repl = require("repl");
const { PassThrough, Writable } = require("stream");
  const input = new PassThrough();
  const raw = [];
  input.isTTY = true;
  input.setRawMode = (mode) => raw.push(mode);
  let written = "";
  const output = new Writable({ write(chunk, enc, cb) { written += chunk.toString(); cb(); } });
  output.isTTY = true;
  output.columns = 80;
  const server = repl.start({ input, output, terminal: true, useColors: false, preview: false, prompt: "> " });
  const send = async (data) => { input.write(data); for (let i = 0; i < 3; i++) await new Promise((r) => setImmediate(r)); };
  await send("1 + 2\r");
  await send("\x1b[A\x1b[D\x1b[D\x7f*\r");
  await send("'abc'.length\r");
  await send("\x1b[A\x1b[A\x1b[B\r");
  await send("xyz\x03");
  await send("\x03");
  const closed = new Promise((resolve) => server.on("exit", resolve));
  await send("\x03");
  await closed;
return { raw, history: server.history, written, terminal: server.terminal };
"##;

const REPL_EXPECTED: &str = r##"{"raw":[true,false],"history":["'abc'.length","1 * 2","1 + 2"],"written":"\u001b[1G\u001b[0J> \u001b[3G1 + 2\r\n3\n\u001b[1G\u001b[0J> \u001b[3G\u001b[1G\u001b[0J> 1 + 2\u001b[8G\u001b[1D\u001b[1D\u001b[1G\u001b[0J> 1  2\u001b[5G*\u001b[2C\r\n2\n\u001b[1G\u001b[0J> \u001b[3G'abc'.length\r\n3\n\u001b[1G\u001b[0J> \u001b[3G\u001b[1G\u001b[0J> 'abc'.length\u001b[15G\u001b[1G\u001b[0J> 1 * 2\u001b[8G\u001b[1G\u001b[0J> 'abc'.length\u001b[15G\r\n3\n\u001b[1G\u001b[0J> \u001b[3Gxyz\r\n\u001b[1G\u001b[0J> \u001b[3G\r\n(To exit, press Ctrl+C again or Ctrl+D or type .exit)\n\u001b[1G\u001b[0J> \u001b[3G\r\n","terminal":true}"##;

#[tokio::test]
async fn repl_terminal_editing_matches_node() {
    let (_dir, root) = tempdir();
    let (runtime, _capture) = runtime(&root);
    let actual = run_cjs_within(&runtime, &root, REPL_SCRIPT, 30).await;
    assert_same_keys(&actual, &serde_json::from_str(REPL_EXPECTED).unwrap());
}
