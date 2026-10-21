mod common;

use common::*;

// Expected values are what Node 22 prints for the same script. Only localhost is used: the
// resolve family queries a small DNS server written with dgram.
const DGRAM_SCRIPT: &str = r##"
const dgram = require("node:dgram");
const out = {};
const once = (emitter, name) => new Promise((resolve) => emitter.once(name, resolve));
const echo = dgram.createSocket("udp4");
echo.on("message", (message, rinfo) => echo.send(message, rinfo.port, rinfo.address));
await new Promise((resolve) => echo.bind(0, "127.0.0.1", resolve));
const port = echo.address().port;
out.address = [echo.address().address, echo.address().family, typeof port];
const client = dgram.createSocket({ type: "udp4" });
let sent;
const reply = new Promise((resolve) => client.once("message", (message, rinfo) => resolve([message.toString(), rinfo.address, rinfo.port === port, rinfo.size, rinfo.family])));
client.send("ping", port, "127.0.0.1", (error, bytes) => (sent = [error, bytes]));
out.echo = await reply;
out.sent = sent;
client.connect(port, "127.0.0.1");
await once(client, "connect");
out.remote = client.remoteAddress().port === port;
client.send(Buffer.from("again"));
out.connected = (await once(client, "message")).toString();
try { client.send("x", port, "127.0.0.1"); } catch (error) { out.isConnected = error.code; }
client.disconnect();
try { client.remoteAddress(); } catch (error) { out.notConnected = error.code; }
try { client.bind(0); } catch (error) { out.alreadyBound = error.code; }
out.options = [client.setTTL(64), client.setMulticastTTL(2), client.setMulticastLoopback(true), client.setBroadcast(true)];
try { client.setTTL("x"); } catch (error) { out.badTTL = error.code; }
try { dgram.createSocket("udp5"); } catch (error) { out.badType = [error.code, error.message]; }
const unbound = dgram.createSocket("udp4");
try { unbound.address(); } catch (error) { out.unbound = [error.code, error.syscall]; }
try { unbound.setTTL(1); } catch (error) { out.unboundTTL = error.code; }
unbound.close();
try { unbound.close(); } catch (error) { out.notRunning = error.code; }
const closed = [];
client.close(() => closed.push("client"));
echo.close(() => closed.push("echo"));
await once(echo, "close");
out.closed = closed;
return out;
"##;

const DGRAM_EXPECTED: &str = r##"{"address":["127.0.0.1","IPv4","number"],"echo":["ping","127.0.0.1",true,4,"IPv4"],"sent":[null,4],"remote":true,"connected":"again","isConnected":"ERR_SOCKET_DGRAM_IS_CONNECTED","notConnected":"ERR_SOCKET_DGRAM_NOT_CONNECTED","alreadyBound":"ERR_SOCKET_ALREADY_BOUND","options":[64,2,true,null],"badTTL":"ERR_INVALID_ARG_TYPE","badType":["ERR_SOCKET_BAD_TYPE","Bad socket type specified. Valid types are: udp4, udp6"],"unbound":["EBADF","getsockname"],"unboundTTL":"EBADF","notRunning":"ERR_SOCKET_DGRAM_NOT_RUNNING","closed":["client","echo"]}"##;

const DNS_SCRIPT: &str = r##"
const dns = require("node:dns");
const dgram = require("node:dgram");
const out = {};
const lookup = (...args) => new Promise((resolve) => dns.lookup(...args, (error, ...rest) => resolve(error ? [error.code] : rest)));
out.localhost4 = await lookup("localhost", 4);
out.all = (await lookup("localhost", { all: true }))[0].some((entry) => entry.address === "127.0.0.1" && entry.family === 4);
out.literal = await lookup("10.0.0.1");
out.promise = await dns.promises.lookup("localhost", { family: 4 });
out.service = typeof (await dns.promises.lookupService("127.0.0.1", 80)).hostname;
try { dns.lookup(1, () => {}); } catch (error) { out.badHost = error.code; }
try { dns.lookup("x", 5, () => {}); } catch (error) { out.badFamily = error.code; }
try { dns.lookupService("nope", 80, () => {}); } catch (error) { out.badService = error.code; }
out.order = [dns.getDefaultResultOrder(), dns.setDefaultResultOrder("ipv4first"), dns.getDefaultResultOrder()];
dns.setDefaultResultOrder("verbatim");
try { dns.setServers(["not an ip"]); } catch (error) { out.badServer = [error.code, error.message]; }
const encodeName = (name) => Buffer.concat([...name.split(".").map((label) => Buffer.concat([Buffer.from([label.length]), Buffer.from(label)])), Buffer.from([0])]);
const server = dgram.createSocket("udp4");
server.on("message", (message, rinfo) => {
  let end = 12;
  const labels = [];
  while (message[end]) {
    labels.push(message.subarray(end + 1, end + 1 + message[end]).toString());
    end += message[end] + 1;
  }
  const name = labels.join(".");
  const type = message.readUInt16BE(end + 1);
  const answers = [];
  let rcode = 0;
  if (name === "missing.test") rcode = 3;
  else if (type === 1) answers.push([1, Buffer.from([10, 1, 2, 3])], [1, Buffer.from([10, 1, 2, 4])]);
  else if (type === 16) answers.push([16, Buffer.from("\x05hello\x05world", "latin1")]);
  else if (type === 15) answers.push([15, Buffer.concat([Buffer.from([0, 10]), encodeName("mail.example.test")])]);
  const header = Buffer.alloc(12);
  header.writeUInt16BE(message.readUInt16BE(0), 0);
  header.writeUInt16BE(0x8180 | rcode, 2);
  header.writeUInt16BE(1, 4);
  header.writeUInt16BE(answers.length, 6);
  const records = answers.map(([kind, data]) => {
    const head = Buffer.alloc(12);
    head.writeUInt16BE(0xc00c, 0);
    head.writeUInt16BE(kind, 2);
    head.writeUInt16BE(1, 4);
    head.writeUInt32BE(300, 6);
    head.writeUInt16BE(data.length, 10);
    return Buffer.concat([head, data]);
  });
  server.send(Buffer.concat([header, message.subarray(12, end + 5), ...records]), rinfo.port, rinfo.address);
});
await new Promise((resolve) => server.bind(0, "127.0.0.1", resolve));
const resolver = new dns.promises.Resolver({ timeout: 2000, tries: 1 });
resolver.setServers([`127.0.0.1:${server.address().port}`]);
out.servers = resolver.getServers()[0] === `127.0.0.1:${server.address().port}`;
out.resolve4 = await resolver.resolve4("example.test");
out.ttl = await resolver.resolve4("example.test", { ttl: true });
out.txt = await resolver.resolveTxt("example.test");
out.mx = await resolver.resolve("example.test", "MX");
try { await resolver.resolve4("missing.test"); } catch (error) { out.missing = [error.code, error.syscall, error.hostname, error.message]; }
try { await resolver.resolve6("example.test"); } catch (error) { out.noData = [error.code, error.syscall]; }
const callbackResolver = new dns.Resolver();
callbackResolver.setServers([`127.0.0.1:${server.address().port}`]);
out.callback = await new Promise((resolve) => callbackResolver.resolve4("example.test", (error, list) => resolve([error, list])));
try { resolver.resolve("example.test", "BOGUS"); } catch (error) { out.badType = error.code; }
server.close();
return out;
"##;

const DNS_EXPECTED: &str = r##"{"localhost4":["127.0.0.1",4],"all":true,"literal":["10.0.0.1",4],"promise":{"address":"127.0.0.1","family":4},"service":"string","badHost":"ERR_INVALID_ARG_TYPE","badFamily":"ERR_INVALID_ARG_VALUE","badService":"ERR_INVALID_ARG_VALUE","order":["verbatim",null,"ipv4first"],"badServer":["ERR_INVALID_IP_ADDRESS","Invalid IP address: not an ip"],"servers":true,"resolve4":["10.1.2.3","10.1.2.4"],"ttl":[{"address":"10.1.2.3","ttl":300},{"address":"10.1.2.4","ttl":300}],"txt":[["hello","world"]],"mx":[{"exchange":"mail.example.test","priority":10}],"missing":["ENOTFOUND","queryA","missing.test","queryA ENOTFOUND missing.test"],"noData":["ENODATA","queryAaaa"],"callback":[null,["10.1.2.3","10.1.2.4"]],"badType":"ERR_INVALID_ARG_VALUE"}"##;

#[tokio::test]
async fn dgram_matches_node() {
    let (_dir, root) = tempdir();
    let (runtime, capture) = runtime(&root);
    let actual = run_cjs(&runtime, &root, DGRAM_SCRIPT).await;
    assert_same_keys(&actual, &serde_json::from_str(DGRAM_EXPECTED).unwrap());
    assert!(capture.uncaught.lock().unwrap().is_empty());
}

#[tokio::test]
async fn dns_matches_node() {
    let (_dir, root) = tempdir();
    let (runtime, capture) = runtime(&root);
    let actual = run_cjs(&runtime, &root, DNS_SCRIPT).await;
    assert_same_keys(&actual, &serde_json::from_str(DNS_EXPECTED).unwrap());
    assert!(capture.uncaught.lock().unwrap().is_empty());
}

// Joining a source-specific group on the loopback interface is compared on Linux only.
const SOURCE_MEMBERSHIP_SCRIPT: &str = r##"
const dgram = require("node:dgram");
const out = {};
const attempt = (fn) => {
  try {
    fn();
    return "ok";
  } catch (error) {
    return [error.name, error.code, error.message, error.syscall ?? null, typeof error.errno];
  }
};
const socket = dgram.createSocket("udp4");
await new Promise((resolve) => socket.bind(0, "127.0.0.1", resolve));
out.noArgs = attempt(() => socket.addSourceSpecificMembership());
out.groupType = attempt(() => socket.addSourceSpecificMembership("127.0.0.1", 5));
out.dropNoArgs = attempt(() => socket.dropSourceSpecificMembership());
out.badSource = attempt(() => socket.addSourceSpecificMembership("nope", "232.1.1.1"));
out.badGroup = attempt(() => socket.addSourceSpecificMembership("127.0.0.1", "nope"));
out.badInterface = attempt(() => socket.addSourceSpecificMembership("127.0.0.1", "232.1.1.1", "nope"));
out.mixedFamilies = attempt(() => socket.addSourceSpecificMembership("127.0.0.1", "ff3e::1234"));
out.dropBad = attempt(() => socket.dropSourceSpecificMembership("127.0.0.1", "nope"));
if (process.platform === "linux") {
  out.join = attempt(() => socket.addSourceSpecificMembership("127.0.0.1", "232.1.1.1", "127.0.0.1"));
  out.drop = attempt(() => socket.dropSourceSpecificMembership("127.0.0.1", "232.1.1.1", "127.0.0.1"));
  out.dropAgain = attempt(() => socket.dropSourceSpecificMembership("127.0.0.1", "232.1.1.1", "127.0.0.1"));
  out.notMulticast = attempt(() => socket.addSourceSpecificMembership("127.0.0.1", "10.1.1.1", "127.0.0.1"));
}
await new Promise((resolve) => socket.close(resolve));
out.closed = attempt(() => socket.addSourceSpecificMembership("127.0.0.1", "232.1.1.1"));
return out;
"##;

const SOURCE_MEMBERSHIP_EXPECTED: &str = r##"{"noArgs":["TypeError","ERR_INVALID_ARG_TYPE","The \"sourceAddress\" argument must be of type string. Received undefined",null,"undefined"],"groupType":["TypeError","ERR_INVALID_ARG_TYPE","The \"groupAddress\" argument must be of type string. Received type number (5)",null,"undefined"],"dropNoArgs":["TypeError","ERR_INVALID_ARG_TYPE","The \"sourceAddress\" argument must be of type string. Received undefined",null,"undefined"],"badSource":["Error","EINVAL","addSourceSpecificMembership EINVAL","addSourceSpecificMembership","number"],"badGroup":["Error","EINVAL","addSourceSpecificMembership EINVAL","addSourceSpecificMembership","number"],"badInterface":["Error","EINVAL","addSourceSpecificMembership EINVAL","addSourceSpecificMembership","number"],"mixedFamilies":["Error","EINVAL","addSourceSpecificMembership EINVAL","addSourceSpecificMembership","number"],"dropBad":["Error","EINVAL","dropSourceSpecificMembership EINVAL","dropSourceSpecificMembership","number"],"join":"ok","drop":"ok","dropAgain":["Error","EINVAL","dropSourceSpecificMembership EINVAL","dropSourceSpecificMembership","number"],"notMulticast":["Error","EINVAL","addSourceSpecificMembership EINVAL","addSourceSpecificMembership","number"],"closed":["Error","ERR_SOCKET_DGRAM_NOT_RUNNING","Not running",null,"undefined"]}"##;

#[tokio::test]
async fn dgram_source_specific_membership_matches_node() {
    let (_dir, root) = tempdir();
    let (runtime, capture) = runtime(&root);
    let actual = run_cjs_within(&runtime, &root, SOURCE_MEMBERSHIP_SCRIPT, 60).await;
    let mut expected: serde_json::Value = serde_json::from_str(SOURCE_MEMBERSHIP_EXPECTED).unwrap();
    if !cfg!(target_os = "linux") {
        for key in ["join", "drop", "dropAgain", "notMulticast"] {
            expected.as_object_mut().unwrap().remove(key);
        }
    }
    assert_same_keys(&actual, &expected);
    assert!(capture.uncaught.lock().unwrap().is_empty());
}
