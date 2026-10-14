// HTTPS on localhost with the group B test certificate. The test compares the result with Node's.
const https = require("node:https");
const out = {};
const once = (emitter, name) => new Promise((resolve) => emitter.once(name, (...args) => resolve(args)));
const text = (stream) => new Promise((resolve) => {
  let data = "";
  stream.setEncoding("utf8");
  stream.on("data", (chunk) => (data += chunk));
  stream.on("end", () => resolve(data));
});
const server = https.createServer({ key: __KEY__, cert: __CERT__ }, async (req, res) => {
  const body = await text(req);
  res.setHeader("X-Proto", `${req.socket.encrypted} ${req.socket.alpnProtocol}`);
  res.end(`${req.method} ${req.url} ${body}`);
});
server.listen(0, "127.0.0.1");
await once(server, "listening");
const port = server.address().port;
const agent = new https.Agent({ keepAlive: true, ca: __CERT__ });
const results = [];
for (const path of ["/one", "/two"]) {
  const req = https.request({ host: "127.0.0.1", port, path, method: "POST", agent, servername: "localhost" });
  req.end("body");
  const [res] = await once(req, "response");
  const authorized = res.socket.authorized;
  results.push([res.statusCode, res.headers["x-proto"], await text(res), req.reusedSocket, authorized]);
}
out.agent = results;
out.name = agent.getName({ host: "localhost", port, servername: "localhost", rejectUnauthorized: false }).replace(String(port), "PORT");
agent.destroy();
const untrusted = https.get({ host: "127.0.0.1", port, path: "/x", agent: false });
const [error] = await once(untrusted, "error");
out.untrusted = error.code;
const loose = https.get(`https://127.0.0.1:${port}/loose`, { rejectUnauthorized: false });
const [looseRes] = await once(loose, "response");
out.loose = [looseRes.statusCode, await text(looseRes)];
out.defaults = [https.globalAgent.defaultPort, https.globalAgent.protocol, new https.Agent().maxCachedSessions];
server.close();
https.globalAgent.destroy();
await once(server, "close");
return out;
