mod common;

use common::*;

// Expected values are what Node 22 prints for the same script. Everything stays on localhost.
const NET_SCRIPT: &str = r##"
const net = require("node:net");
const out = {};
const once = (emitter, name) => new Promise((resolve) => emitter.once(name, resolve));
const mark = (text) => require("fs").appendFileSync(require("path").join(__dirname, "stage"), text + "\n");
let echoClosed;
const echo = (path) => net.createServer((socket) => {
  echoClosed = once(socket, "close");
  socket.setNoDelay(true);
  socket.setKeepAlive(true, 1000);
  socket.on("data", (chunk) => socket.write(chunk));
  socket.on("end", () => socket.end());
});
async function roundTrip(server, connectArgs) {
  const client = net.connect(...connectArgs);
  const states = [client.readyState];
  await once(client, "connect");
  states.push(client.readyState);
  client.setEncoding("utf8");
  let text = "";
  client.on("data", (chunk) => (text += chunk));
  client.write("hello ");
  client.write(Buffer.from("world"));
  client.end();
  const hadError = await once(client, "close");
  states.push(client.readyState);
  return [text, hadError, states, client.bytesRead, client.bytesWritten];
}
mark("tcp");
const server = echo();
server.listen(0, "127.0.0.1");
await once(server, "listening");
const { port, family, address } = server.address();
out.listening = [server.listening, family, address, typeof port];
const conn = once(server, "connection");
out.tcp = await roundTrip(server, [port, "127.0.0.1"]);
const accepted = await conn;
out.accepted = [accepted.remoteAddress, accepted.localPort === port, accepted.remoteFamily];
mark("inUse");
const busy = net.createServer();
busy.listen(port, "127.0.0.1");
const inUse = await once(busy, "error");
out.inUse = [inUse.code, inUse.syscall, inUse.message, inUse.port];
busy.close();
server.close();
await once(server, "close");
out.closed = [server.listening, server.address()];
mark("refused");
const refusedClient = net.connect(port, "127.0.0.1");
const refusedError = await once(refusedClient, "error");
out.refused = [refusedError.code, refusedError.syscall, refusedError.address, refusedError.message === `connect ECONNREFUSED 127.0.0.1:${port}`];
const dir = require("fs").mkdtempSync(require("path").join(require("os").tmpdir(), "pi-net-"));
const path = process.platform === "win32" ? `\\\\.\\pipe\\pi-net-${process.pid}` : require("path").join(dir, "echo.sock");
mark("pipe");
const pipeServer = echo();
pipeServer.listen(path);
await once(pipeServer, "listening");
out.pipeAddress = pipeServer.address() === path;
out.pipe = (await roundTrip(pipeServer, [path])).slice(0, 2);
await echoClosed;
const counted = await new Promise((resolve) => pipeServer.getConnections((error, count) => resolve([error, count])));
out.connections = counted;
mark("pipe close");
pipeServer.close();
await once(pipeServer, "close");
mark("timeout");
const timed = net.createServer().listen(0, "127.0.0.1");
await once(timed, "listening");
const idle = net.connect(timed.address().port, "127.0.0.1");
idle.setTimeout(30);
await once(idle, "timeout");
out.timeout = idle.timeout;
idle.destroy();
timed.close();
mark("unread end");
const quiet = net.createServer((socket) => socket.end()).listen(0, "127.0.0.1");
await once(quiet, "listening");
const unread = net.connect(quiet.address().port, "127.0.0.1");
out.unreadEnd = [await once(unread, "close"), unread.readableEnded];
quiet.close();
mark("helpers");
const unrefed = net.createServer().listen(0, "127.0.0.1");
unrefed.unref();
out.helpers = [net.isIP("::1"), net.isIP("1.2.3.4"), net.isIP("x"), net.isIPv4("10.0.0.1"), net.isIPv6("fe80::1%eth0")];
const list = new net.BlockList();
list.addAddress("10.0.0.1");
list.addRange("10.0.1.1", "10.0.1.10");
list.addSubnet("8592:757c:efae:4e45::", 64, "ipv6");
out.blockList = [list.check("10.0.0.1"), list.check("10.0.1.5"), list.check("10.0.2.1"), list.check("8592:757c:efae:4e45:f::", "ipv6"), list.check("::ffff:10.0.0.1", "ipv6"), list.rules];
const socketAddress = net.SocketAddress.parse("[::1]:8080");
out.socketAddress = [socketAddress.address, socketAddress.port, socketAddress.family, new net.SocketAddress().toJSON()];
try { net.connect(70000); } catch (error) { out.badPort = [error.code, error.message]; }
out.autoSelect = [net.getDefaultAutoSelectFamily(), net.setDefaultAutoSelectFamily(false), net.getDefaultAutoSelectFamily()];
return out;
"##;

const NET_EXPECTED: &str = r##"{"listening":[true,"IPv4","127.0.0.1","number"],"tcp":["hello world",false,["opening","open","closed"],11,11],"accepted":["127.0.0.1",true,"IPv4"],"inUse":["EADDRINUSE","listen","listen EADDRINUSE: address already in use 127.0.0.1:PORT",PORTNUM],"closed":[false,null],"refused":["ECONNREFUSED","connect","127.0.0.1",true],"pipeAddress":true,"pipe":["hello world",false],"connections":[null,0],"timeout":30,"unreadEnd":[false,true],"helpers":[6,4,0,true,true],"blockList":[true,true,false,true,true,["Subnet: IPv6 8592:757c:efae:4e45::/64","Range: IPv4 10.0.1.1-10.0.1.10","Address: IPv4 10.0.0.1"]],"socketAddress":["::1",8080,"ipv6",{"address":"127.0.0.1","port":0,"family":"ipv4","flowlabel":0}],"badPort":["ERR_SOCKET_BAD_PORT","Port should be >= 0 and < 65536. Received type number (70000)."],"autoSelect":[true,null,false]}"##;

#[tokio::test]
async fn net_matches_node() {
    let (_dir, root) = tempdir();
    let (runtime, capture) = runtime(&root);
    let mut actual = run_cjs_within(&runtime, &root, NET_SCRIPT, 120).await;
    let port = actual["inUse"][3].clone();
    let message = actual["inUse"][2].as_str().unwrap().replace(&format!(":{port}"), ":PORT");
    actual["inUse"][2] = message.into();
    actual["inUse"][3] = "PORTNUM".into();
    let expected: serde_json::Value = serde_json::from_str(&NET_EXPECTED.replace("PORTNUM", "\"PORTNUM\"")).unwrap();
    assert_same_keys(&actual, &expected);
    assert!(capture.uncaught.lock().unwrap().is_empty());
}

#[tokio::test]
async fn auto_select_family_matches_node() {
    let (_dir, root) = tempdir();
    let (runtime, capture) = runtime(&root);
    let actual = run_cjs_within(&runtime, &root, include_str!("fixtures/node_net/autoselect.js"), 120).await;
    let expected = serde_json::from_str(include_str!("fixtures/node_net/autoselect.json")).unwrap();
    assert_same_keys(&actual, &expected);
    assert!(capture.uncaught.lock().unwrap().is_empty());
}

// reusePort spreads connections only on Linux; elsewhere libuv and pi-js fail with ENOTSUP.
#[cfg(target_os = "linux")]
const REUSE_PORT_SCRIPT: &str = r##"
const net = require("node:net");
const out = {};
const listen = (options) => new Promise((resolve) => {
  const server = net.createServer((socket) => socket.end(String(options.tag)));
  server.on("error", (error) => resolve([server, [error.code, error.message.replace(/:\d+$/, ":PORT"), error.syscall, error.address]]));
  server.listen(options, () => resolve([server, "listening"]));
});
const read = (port) => new Promise((resolve) => net.connect(port, "127.0.0.1").on("data", (data) => resolve(String(data))));
const [first, a] = await listen({ port: 0, host: "127.0.0.1", reusePort: true, tag: 1 });
const port = first.address().port;
const [second, b] = await listen({ port, host: "127.0.0.1", reusePort: true, tag: 2 });
const [, c] = await listen({ port, host: "127.0.0.1", tag: 3 });
const [, d] = await listen({ port, host: "127.0.0.1", reusePort: "yes", tag: 4 });
out.listen = [a, b, c, d, second.address().port === port];
// The kernel spreads connections by their source port, so both servers answer within a few tries.
const tags = new Set();
for (let i = 0; i < 40 && tags.size < 2; i++) tags.add(await read(port));
out.spread = [...tags].sort();
await new Promise((resolve) => first.close(resolve));
out.afterClose = await read(port);
await new Promise((resolve) => second.close(resolve));
const [any, e] = await listen({ port: 0, reusePort: true, tag: 5 });
out.anyHost = e;
any.close();
return out;
"##;

#[cfg(target_os = "linux")]
const REUSE_PORT_EXPECTED: &str = r##"{"listen":["listening","listening",["EADDRINUSE","listen EADDRINUSE: address already in use 127.0.0.1:PORT","listen","127.0.0.1"],["EADDRINUSE","listen EADDRINUSE: address already in use 127.0.0.1:PORT","listen","127.0.0.1"],true],"spread":["1","2"],"afterClose":"2","anyHost":"listening"}"##;

#[cfg(target_os = "linux")]
#[tokio::test]
async fn reuse_port_matches_node() {
    let (_dir, root) = tempdir();
    let (runtime, capture) = runtime(&root);
    let actual = run_cjs_within(&runtime, &root, REUSE_PORT_SCRIPT, 60).await;
    assert_same_keys(&actual, &serde_json::from_str(REUSE_PORT_EXPECTED).unwrap());
    assert!(capture.uncaught.lock().unwrap().is_empty());
}
