const http2 = require("node:http2");
const out = {};
const once = (emitter, name) => new Promise((resolve) => emitter.once(name, resolve));
let port;
// Drop the date and the port, which change from run to run.
const plain = (headers) => Object.fromEntries(Object.entries(headers).filter(([name]) => name !== "date").map(([name, value]) => [name, name === ":authority" ? value.replace(`:${port}`, ":PORT") : value]));
const server = http2.createServer();
const serverSide = [];
server.on("stream", (stream, headers) => {
  if (headers[":path"] === "/nopush") {
    serverSide.push(["nopush", stream.pushAllowed]);
    try {
      stream.pushStream({ ":path": "/x" }, () => serverSide.push("called"));
    } catch (error) {
      serverSide.push([error.code, error.message]);
    }
    return stream.respond({ ":status": 200 }, { endStream: true });
  }
  serverSide.push(["pushAllowed", stream.pushAllowed, stream.id]);
  stream.pushStream({ ":path": "/style.css" }, (error, push, pushHeaders) => {
    serverSide.push(["pushed", error, push.id, plain(pushHeaders), push.pushAllowed]);
    try {
      push.pushStream({ ":path": "/nested" }, () => {});
    } catch (nested) {
      serverSide.push(["nested", nested.code]);
    }
    push.respond({ ":status": 200, "content-type": "text/css" });
    push.end("body { color: red }");
  });
  stream.respond({ ":status": 200, "content-type": "text/html" });
  stream.pushStream({ ":path": "/late.js", "x-extra": "1" }, (error, push) => {
    serverSide.push(["late", error, push.id]);
    push.respond({ ":status": 200 });
    push.end("late");
  });
  stream.end("<html></html>");
});
await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
port = server.address().port;
const client = http2.connect(`http://127.0.0.1:${port}`);
const pushes = [];
const pushed = new Promise((resolve) => {
  client.on("stream", (stream, requestHeaders, flags) => {
    const entry = { request: plain(requestHeaders), flags, id: stream.id, body: "" };
    pushes.push(entry);
    stream.on("push", (responseHeaders, pushFlags) => (entry.response = [plain(responseHeaders), pushFlags]));
    stream.setEncoding("utf8");
    stream.on("data", (chunk) => (entry.body += chunk));
    stream.on("end", () => pushes.filter((item) => item.ended).length === 1 ? resolve() : (entry.ended = true));
  });
});
const request = client.request({ ":path": "/" });
const events = [];
request.on("push", () => events.push("push on request"));
request.on("response", (headers) => events.push(["response", headers[":status"]]));
let body = "";
request.setEncoding("utf8");
request.on("data", (chunk) => (body += chunk));
await once(request, "end");
await pushed;
out.request = [body, events];
out.pushes = pushes.sort((a, b) => a.id - b.id).map((entry) => [entry.request, entry.flags, entry.id, entry.response, entry.body]);
client.close();
const noPush = http2.connect(`http://127.0.0.1:${port}`, { settings: { enablePush: false } });
const second = noPush.request({ ":path": "/nopush" });
second.resume();
await once(second, "close");
noPush.close();
await new Promise((resolve) => setImmediate(resolve));
out.server = serverSide;
server.close();
return out;
