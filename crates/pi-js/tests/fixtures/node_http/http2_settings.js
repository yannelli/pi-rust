const http2 = require("node:http2");
const out = { server: [], client: [], errors: [] };
const once = (emitter, name) => new Promise((resolve) => emitter.once(name, resolve));
const attempt = (fn) => {
  try {
    fn();
    out.errors.push("ok");
  } catch (error) {
    out.errors.push([error.name, error.code, error.message]);
  }
};
attempt(() => http2.createServer({ remoteCustomSettings: Array.from({ length: 11 }, (_, i) => i) }));
attempt(() => http2.createServer({ remoteCustomSettings: 5 }));
const server = http2.createServer({ settings: { customSettings: { 1000: 42 } }, remoteCustomSettings: [2000, 2001] });
const serverClosed = new Promise((resolve) => {
  server.on("session", (session) => {
    session.on("remoteSettings", (s) => out.server.push(["remote", s.customSettings ?? null, s.enablePush]));
    session.on("localSettings", (s) => out.server.push(["local", s.customSettings ?? null]));
    session.on("close", resolve);
  });
});
server.on("stream", (stream) => {
  stream.respond({ ":status": 200 });
  stream.end("ok");
});
await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
const client = http2.connect(`http://127.0.0.1:${server.address().port}`, { settings: { customSettings: { 2000: 7 } }, remoteCustomSettings: [1000] });
client.on("remoteSettings", (s) => out.client.push(["remote", s.customSettings ?? null, s.maxConcurrentStreams]));
client.on("localSettings", (s) => out.client.push(["local", s.customSettings ?? null]));
await once(client, "remoteSettings");
out.pending = client.pendingSettingsAck;
const acked = new Promise((resolve) => client.settings({ customSettings: { 2001: 9 } }, (error, settings) => resolve([error, settings.customSettings, typeof settings.initialWindowSize])));
out.pendingAfter = client.pendingSettingsAck;
out.ack = await acked;
out.afterAck = client.pendingSettingsAck;
const many = Object.fromEntries(Array.from({ length: 11 }, (_, i) => [1000 + i, 1]));
for (const customSettings of [many, { 70000: 1 }, { 1000: -1 }, { 1000: 2 ** 32 }, 5, { 1000: "x" }, { 1000: 0 }]) {
  attempt(() => client.settings({ customSettings }));
}
const request = client.request({ ":path": "/" });
let body = "";
request.on("data", (chunk) => (body += chunk));
await once(request, "end");
out.body = body;
out.local = client.localSettings.customSettings ?? null;
out.remote = client.remoteSettings.customSettings ?? null;
client.close();
await serverClosed;
await new Promise((resolve) => server.close(resolve));
return out;
