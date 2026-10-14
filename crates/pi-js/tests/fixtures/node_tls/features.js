const tls = require("node:tls");
const net = require("node:net");
const { Duplex } = require("node:stream");
const key = __KEY__, cert = __CERT__, sniKey = __SNI_KEY__, sniCert = __SNI_CERT__;
const out = {};
const once = (emitter, name) => new Promise((resolve) => emitter.once(name, resolve));
const mark = (text) => require("fs").appendFileSync(require("path").join(__dirname, "stage"), text + "\n");
const served = [];
// OpenSSL skips SNICallback when a session resumes, and rustls cannot know that in advance, so
// only the SNI steps record the calls.
let logSni = true;
const server = tls.createServer({
  key,
  cert,
  SNICallback(name, callback) {
    if (logSni) served.push(["sni", name]);
    if (name === "bad.test") return callback(new Error("no context"));
    callback(null, name === "sni.test" ? tls.createSecureContext({ key: sniKey, cert: sniCert }) : undefined);
  },
}, (socket) => {
  served.push(["secureConnection", socket.servername, socket.isSessionReused()]);
  socket.end(`hi ${socket.servername}`);
});
server.on("tlsClientError", (error) => served.push(["tlsClientError", error.message]));
await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
const port = server.address().port;
async function finish(socket) {
  let text = "";
  socket.setEncoding("utf8");
  socket.on("data", (chunk) => (text += chunk));
  await once(socket, "close");
  return text;
}

mark("sni");
const picked = tls.connect({ port, host: "127.0.0.1", servername: "sni.test", ca: [cert, sniCert] });
await once(picked, "secureConnect");
out.sni = [picked.authorized, picked.getPeerCertificate().subject.CN, await finish(picked)];
const fallback = tls.connect({ port, host: "127.0.0.1", servername: "localhost", ca: [cert] });
await once(fallback, "secureConnect");
out.sniDefault = [fallback.authorized, fallback.getPeerCertificate().subject.CN, await finish(fallback)];
const refused = tls.connect({ port, host: "127.0.0.1", servername: "bad.test", ca: [cert] });
const refusedError = await once(refused, "error");
out.sniError = [refusedError.code, refusedError.message];
const noName = tls.connect({ port, host: "127.0.0.1", ca: [cert], checkServerIdentity: () => undefined });
await once(noName, "secureConnect");
out.noServername = [noName.getPeerCertificate().subject.CN, await finish(noName)];

mark("sessions");
logSni = false;
const sessions = [];
const first = tls.connect({ port, host: "127.0.0.1", servername: "localhost", ca: [cert] });
first.on("session", (session) => sessions.push(session));
await once(first, "secureConnect");
out.firstSession = [first.isSessionReused(), Buffer.isBuffer(first.getSession())];
await finish(first);
out.sessions = [sessions.length > 0, sessions.every(Buffer.isBuffer), first.getSession()];
const resumed = tls.connect({ port, host: "127.0.0.1", servername: "localhost", ca: [cert], session: sessions[0] });
await once(resumed, "secureConnect");
out.resumed = [resumed.isSessionReused(), resumed.authorized, await finish(resumed)];
const again = tls.connect({ port, host: "127.0.0.1", servername: "localhost", ca: [cert], session: sessions[0] });
await once(again, "secureConnect");
out.resumedAgain = [again.isSessionReused(), await finish(again)];
const order = [];
const old = tls.connect({ port, host: "127.0.0.1", servername: "localhost", ca: [cert], maxVersion: "TLSv1.2" });
let oldSession;
old.on("session", (session) => {
  oldSession = session;
  order.push(["session", old._secureEstablished]);
});
old.on("secureConnect", () => order.push("secureConnect"));
await finish(old);
const oldResumed = tls.connect({ port, host: "127.0.0.1", servername: "localhost", ca: [cert], maxVersion: "TLSv1.2", session: oldSession });
await once(oldResumed, "secureConnect");
out.tls12 = [order, oldResumed.isSessionReused(), oldResumed.getProtocol(), await finish(oldResumed)];

mark("wrap");
const raw = net.connect(port, "127.0.0.1");
const wrapped = tls.connect({ socket: raw, servername: "localhost", ca: [cert] });
const events = [];
for (const name of ["connect", "secure", "secureConnect"]) wrapped.on(name, () => events.push(name));
raw.on("data", () => events.push("raw data"));
await once(wrapped, "secureConnect");
out.wrap = [wrapped.authorized, wrapped.encrypted, wrapped.remotePort === port, wrapped.remoteAddress, wrapped.connecting, wrapped._parent === raw];
out.wrapText = [await finish(wrapped), events, raw.destroyed];
const connected = net.connect(port, "127.0.0.1");
await once(connected, "connect");
const late = tls.connect({ socket: connected, servername: "sni.test", ca: [sniCert] });
const lateEvents = [];
for (const name of ["connect", "secure", "secureConnect"]) late.on(name, () => lateEvents.push(name));
await once(late, "secureConnect");
out.wrapConnected = [late.getPeerCertificate().subject.CN, await finish(late), lateEvents];

mark("duplex");
const carrier = net.connect(port, "127.0.0.1");
const duplex = new Duplex({
  read() {},
  write(chunk, encoding, callback) {
    carrier.write(chunk, callback);
  },
});
carrier.on("data", (chunk) => duplex.push(chunk));
carrier.on("end", () => duplex.push(null));
const overStream = tls.connect({ socket: duplex, servername: "localhost", ca: [cert] });
await once(overStream, "secureConnect");
let streamText = "";
overStream.on("data", (chunk) => (streamText += chunk));
await once(overStream, "end");
out.duplex = [overStream.authorized, streamText];
overStream.destroy();
carrier.destroy();

mark("server side");
const plain = net.createServer((socket) => {
  const secure = new tls.TLSSocket(socket, { isServer: true, key, cert });
  secure.on("secure", () => {
    served.push(["server secure", secure.servername, secure.authorized, secure.encrypted]);
    secure.end("from server");
  });
});
await new Promise((resolve) => plain.listen(0, "127.0.0.1", resolve));
const toPlain = tls.connect({ port: plain.address().port, host: "127.0.0.1", servername: "localhost", ca: [cert] });
out.serverSide = await finish(toPlain);

mark("addContext");
const named = tls.createServer({ key, cert }, (socket) => socket.end(socket.servername));
named.addContext("*.test", { key: sniKey, cert: sniCert });
await new Promise((resolve) => named.listen(0, "127.0.0.1", resolve));
const viaContext = tls.connect({ port: named.address().port, host: "127.0.0.1", servername: "sni.test", ca: [sniCert] });
await once(viaContext, "secureConnect");
out.addContext = [viaContext.getPeerCertificate().subject.CN, await finish(viaContext)];
let missingName;
try {
  named.addContext("", {});
} catch (error) {
  missingName = error.code;
}
out.addContextError = missingName;

plain.close();
named.close();
server.close();
out.served = served;
return out;
