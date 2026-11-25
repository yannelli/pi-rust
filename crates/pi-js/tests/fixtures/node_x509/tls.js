const tls = require("node:tls");
const { X509Certificate } = require("node:crypto");
const once = (emitter, name) => new Promise((resolve) => emitter.once(name, resolve));
const server = tls.createServer({ key: __LEAF_KEY__, cert: __LEAF__ + __CA__ });
const accepted = once(server, "secureConnection");
server.listen(0, "127.0.0.1");
await once(server, "listening");
const client = tls.connect({ port: server.address().port, host: "127.0.0.1", rejectUnauthorized: false });
await once(client, "secureConnect");
const serverSide = await accepted;
const peer = client.getPeerX509Certificate();
const out = {
  instance: peer instanceof X509Certificate,
  subject: peer.subject,
  fingerprint: peer.fingerprint256 === new X509Certificate(__LEAF__).fingerprint256,
  issuer: peer.issuerCertificate.subject,
  issuerOfIssuer: peer.issuerCertificate.issuerCertificate === undefined,
  signedByIssuer: peer.verify(peer.issuerCertificate.publicKey) && peer.checkIssued(peer.issuerCertificate),
  fresh: client.getPeerX509Certificate() !== peer,
  clientOwn: client.getX509Certificate() === undefined,
  serverPeer: serverSide.getPeerX509Certificate() === undefined,
};
client.end();
serverSide.end();
await new Promise((resolve) => server.close(resolve));
return out;
