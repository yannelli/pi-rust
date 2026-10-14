// HTTP/1.1 server and client round trips on localhost. The test compares the result with Node's.
const http = require("node:http");
const net = require("node:net");
const out = {};
const once = (emitter, name) => new Promise((resolve) => emitter.once(name, (...args) => resolve(args)));
const text = (stream) => new Promise((resolve, reject) => {
  let data = "";
  stream.setEncoding("utf8");
  stream.on("data", (chunk) => (data += chunk));
  stream.on("end", () => resolve(data));
  stream.on("error", reject);
});
const server = http.createServer(async (req, res) => {
  const body = await text(req);
  if (req.url === "/chunked") {
    res.setHeader("X-Custom", ["a", "b"]);
    res.setHeader("Trailer", "X-Sum");
    res.write(`got ${req.method} ${body.length} `);
    res.write(JSON.stringify(req.trailers));
    res.addTrailers({ "X-Sum": "42" });
    res.end();
  } else if (req.url === "/fixed") {
    res.writeHead(201, "Made", { "Content-Type": "text/plain", "x-port": String(req.socket.remotePort === undefined) });
    res.end(`${req.headers["x-dup"]}|${req.rawHeaders.filter((_, i) => i % 2 === 0).join(",")}`);
  } else if (req.url === "/continue") {
    res.end(`continued ${body}`);
  } else if (req.url === "/slow") {
    setTimeout(() => res.end("late"), 300);
  } else if (req.url === "/hints") {
    res.writeEarlyHints({ link: "</a.css>; rel=preload" });
    res.end("hinted");
  } else {
    res.statusCode = 404;
    res.end();
  }
});
server.on("checkContinue", (req, res) => {
  out.checkContinue = req.headers.expect;
  res.writeContinue();
  server.emit("request", req, res);
});
server.on("upgrade", (req, socket, head) => {
  socket.write("HTTP/1.1 101 Switching Protocols\r\nUpgrade: echo\r\nConnection: Upgrade\r\n\r\n");
  socket.write(`head:${head.toString()}`);
  socket.on("data", (chunk) => socket.end(`echo:${chunk}`));
});
server.on("connect", (req, socket, head) => {
  socket.write("HTTP/1.1 200 Connection Established\r\n\r\n");
  socket.end(`tunnel to ${req.url}`);
});
server.listen(0, "127.0.0.1");
await once(server, "listening");
const port = server.address().port;
const base = `http://127.0.0.1:${port}`;

const agent = new http.Agent({ keepAlive: true, maxSockets: 1 });
const chunked = http.request(`${base}/chunked`, { method: "POST", agent, headers: { "Content-Type": "text/plain" } });
chunked.setHeader("Trailer", "X-Req");
chunked.write("hello ");
chunked.write(Buffer.from("world"));
chunked.addTrailers({ "X-Req": "yes" });
chunked.end();
const [res1] = await once(chunked, "response");
const body1 = await text(res1);
out.chunked = [res1.statusCode, res1.statusMessage, res1.httpVersion, res1.headers["transfer-encoding"], res1.headers["x-custom"], body1, res1.trailers, res1.rawTrailers, chunked.reusedSocket];
const fixed = http.request({ host: "127.0.0.1", port, path: "/fixed", agent, headers: [["Host", "h"], ["X-Dup", "1"], ["x-dup", "2"]] });
fixed.end();
const [res2] = await once(fixed, "response");
out.fixed = [res2.statusCode, res2.statusMessage, res2.headers["content-type"], res2.headers["content-length"], await text(res2), fixed.reusedSocket, res2.headers.connection, res2.headers["keep-alive"]];
await new Promise((resolve) => setImmediate(resolve));
out.agent = [Object.keys(agent.freeSockets).length, agent.getName({ host: "localhost", port: 80, family: 4 })];
agent.destroy();

const cont = http.request({ host: "127.0.0.1", port, path: "/continue", method: "PUT", headers: { Expect: "100-continue", "Content-Length": 4 } });
const events = [];
cont.on("continue", () => {
  events.push("continue");
  cont.end("data");
});
cont.on("information", (info) => events.push(info.statusCode));
const [res3] = await once(cont, "response");
out.continue = [events, await text(res3)];

const hints = http.get(`${base}/hints`);
const infos = [];
hints.on("information", (info) => infos.push([info.statusCode, info.headers.link]));
const [res4] = await once(hints, "response");
out.hints = [infos, await text(res4)];

const up = http.request({ host: "127.0.0.1", port, headers: { Connection: "Upgrade", Upgrade: "echo" } });
up.end();
const [upRes, upSocket, upHead] = await once(up, "upgrade");
upSocket.write("ping");
let upData = upHead.toString();
upSocket.on("data", (chunk) => (upData += chunk));
await once(upSocket, "end");
out.upgrade = [upRes.statusCode, upRes.headers.upgrade, upData];
upSocket.destroy();

const tunnel = http.request({ host: "127.0.0.1", port, method: "CONNECT", path: "example.com:443" });
tunnel.end();
const [tunnelRes, tunnelSocket, tunnelHead] = await once(tunnel, "connect");
let tunnelData = tunnelHead.toString();
tunnelSocket.on("data", (chunk) => (tunnelData += chunk));
await once(tunnelSocket, "end");
out.connect = [tunnelRes.statusCode, tunnelRes.statusMessage, tunnelData];

const slow = http.get({ host: "127.0.0.1", port, path: "/slow", timeout: 50, agent: false });
await once(slow, "timeout");
slow.destroy();
const [slowError] = await once(slow, "error");
out.timeout = [slowError.code, slowError.message];

const aborted = http.get({ host: "127.0.0.1", port, path: "/slow" });
const order = [];
aborted.on("abort", () => order.push("abort"));
aborted.on("error", (error) => order.push(error.code));
aborted.on("close", () => order.push("close"));
aborted.once("socket", () => setTimeout(() => aborted.abort(), 20));
await once(aborted, "close");
out.abort = [order, aborted.aborted, aborted.destroyed];

const raw = net.connect(port, "127.0.0.1");
raw.write("GET /fixed HTTP/1.0\r\n\r\n");
out.http10 = (await text(raw)).replace(/Date: .*\r\n/, "");
const bad = net.connect(port, "127.0.0.1");
bad.write("BAD\x01 / HTTP/1.1\r\n\r\n");
out.bad = await text(bad);

out.errors = [];
for (const action of [
  () => http.request({ protocol: "https:", host: "x" }),
  () => http.request({ path: "/a b" }),
  () => http.request({ method: "GE T" }),
  () => http.validateHeaderName("bad name"),
  () => http.validateHeaderValue("x", "a\nb"),
  () => http.validateHeaderValue("x", undefined),
  () => new http.ServerResponse({ method: "GET", httpVersionMajor: 1, httpVersionMinor: 1, headers: {} }).writeHead(1000),
]) {
  try { action(); out.errors.push(null); } catch (error) { out.errors.push([error.name, error.code, error.message]); }
}
out.constants = [http.METHODS.length, http.METHODS.includes("PATCH"), http.STATUS_CODES[418], http.maxHeaderSize, http.globalAgent.keepAlive, http.globalAgent.options.timeout];
server.close();
await once(server, "close");
return out;
