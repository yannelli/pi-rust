// HTTP/2 over plaintext (prior knowledge) and TLS (ALPN h2) on localhost, compared with Node.
const http2 = require("node:http2");
const out = {};
const once = (emitter, name) => new Promise((resolve) => emitter.once(name, (...args) => resolve(args)));
const text = (stream) => new Promise((resolve) => {
  let data = "";
  stream.setEncoding("utf8");
  stream.on("data", (chunk) => (data += chunk));
  stream.on("end", () => resolve(data));
});
const pick = (headers, names) => names.map((name) => headers[name]);

const server = http2.createServer({ settings: { maxConcurrentStreams: 50 } });
const serverEvents = [];
let serverSession;
server.on("session", (session) => {
  serverSession = session;
  serverEvents.push(["session", session.alpnProtocol, session.encrypted]);
  session.on("goaway", (code) => serverEvents.push(["goaway", code]));
});
server.on("stream", async (stream, headers, flags, rawHeaders) => {
  const body = await text(stream);
  if (headers[":path"].startsWith("/reset")) {
    stream.on("error", (error) => serverEvents.push(["streamError", error.code, error.message]));
    return stream.close(Number(headers[":path"].slice(6)));
  }
  stream.additionalHeaders({ ":status": 103, link: "</a.css>" });
  stream.respond({ ":status": 200, "content-type": "text/plain", "x-raw": rawHeaders.filter((_, i) => i % 2 === 0).join(",") }, { waitForTrailers: true });
  stream.on("wantTrailers", () => stream.sendTrailers({ "x-trailer": "done" }));
  stream.write(`${headers[":method"]} ${headers[":path"]} `);
  stream.end(`${body} ${stream.id} ${pick(headers, [":scheme", ":authority", "x-multi"]).join("|").replace(/:\d+\|/, ":PORT|")}`);
});
server.listen(0, "127.0.0.1");
await once(server, "listening");
const port = server.address().port;
const client = http2.connect(`http://127.0.0.1:${port}`);
const [connected] = await once(client, "connect");
out.connect = [connected === client, client.encrypted, client.alpnProtocol, client.type];
const req = client.request({ ":method": "POST", ":path": "/echo", "x-multi": ["a", "b"] });
const infos = [];
req.on("headers", (headers) => infos.push(headers[":status"]));
req.end("payload");
const [headers, flags] = await once(req, "response");
const trailers = once(req, "trailers");
const body = await text(req);
out.stream = [headers[":status"], headers["content-type"], headers["x-raw"], flags, infos, body, (await trailers)[0]["x-trailer"]];
await once(req, "close");
out.closed = [req.closed, req.rstCode, req.destroyed];
out.reset = [];
for (const code of [8, 2]) {
  const reset = client.request({ ":path": `/reset${code}` });
  let resetError = null;
  reset.on("error", (error) => (resetError = [error.code, error.message]));
  reset.resume();
  await once(reset, "close");
  out.reset.push([resetError, reset.rstCode, reset.aborted]);
}
const [pingError, duration, payload] = await new Promise((resolve) => client.ping(Buffer.from("12345678"), (...args) => resolve(args)));
out.ping = [pingError, typeof duration, payload.toString()];
const [settingsError, settings] = await new Promise((resolve) => client.settings({ initialWindowSize: 100000 }, (...args) => resolve(args)));
out.settings = [settingsError, settings.initialWindowSize, client.localSettings.initialWindowSize, typeof client.remoteSettings.maxConcurrentStreams];
try { client.ping(Buffer.alloc(3), () => {}); } catch (error) { out.pingLength = [error.code, error.message]; }
try { client.settings({ maxFrameSize: 1 }); } catch (error) { out.badSetting = [error.code, error.message]; }
try { client.request({ connection: "close" }); } catch (error) { out.connectionHeader = [error.code, error.message]; }
const goaway = once(client, "goaway");
serverSession.goaway(0);
out.goaway = (await Promise.race([goaway, new Promise((resolve) => setTimeout(() => resolve(["timeout"]), 2000))]))[0];
out.afterGoaway = [client.closed, client.destroyed];
client.destroy();
server.close();
const sessionClosed = once(serverSession, "close");
serverSession.destroy();
await sessionClosed;
out.serverEvents = serverEvents;

const secure = http2.createSecureServer({ key: __KEY__, cert: __CERT__ }, async (req, res) => {
  const body = await text(req);
  res.setHeader("X-Info", `${req.httpVersion} ${req.method} ${req.url} ${req.authority.replace(/:\d+$/, ":PORT")} ${req.scheme}`);
  res.addTrailers({ "x-done": "yes" });
  res.writeHead(201, { "content-type": "text/plain" });
  res.end(`secure ${body} ${req.socket.alpnProtocol}`);
});
secure.listen(0, "127.0.0.1");
await once(secure, "listening");
const tlsClient = http2.connect(`https://localhost:${secure.address().port}`, { ca: __CERT__ });
await once(tlsClient, "connect");
out.tls = [tlsClient.encrypted, tlsClient.alpnProtocol, tlsClient.socket.alpnProtocol];
const tlsReq = tlsClient.request({ ":method": "PUT", ":path": "/compat?x=1" });
tlsReq.end("data");
const [tlsHeaders] = await once(tlsReq, "response");
const tlsTrailers = once(tlsReq, "trailers");
out.compat = [tlsHeaders[":status"], tlsHeaders["x-info"], tlsHeaders["content-type"], typeof tlsHeaders.date, await text(tlsReq), (await tlsTrailers)[0]["x-done"]];
tlsClient.close();
await once(tlsClient, "close");
secure.close();
await once(secure, "close");

out.defaults = http2.getDefaultSettings();
const packed = http2.getPackedSettings({ enablePush: false, initialWindowSize: 1000, maxFrameSize: 20000 });
out.packed = [packed.toString("hex"), http2.getUnpackedSettings(packed)];
const c = http2.constants;
out.constants = [c.NGHTTP2_CANCEL, c.HTTP2_HEADER_STATUS, c.HTTP2_HEADER_CONTENT_TYPE, c.HTTP2_METHOD_GET, c.HTTP_STATUS_OK, c.HTTP_STATUS_TEAPOT, c.NGHTTP2_SETTINGS_ENABLE_PUSH];
try { http2.connect("ftp://localhost"); } catch (error) { out.protocol = [error.code, error.message]; }
return out;
