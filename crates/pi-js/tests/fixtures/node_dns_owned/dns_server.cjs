// Loopback DNS server for the node_dns_owned fixtures: UDP and TCP on the same 127.0.0.1 port.
// `handler(query)` returns a reply spec, `null` to drop the query, or a promise of either.
const dgram = require("node:dgram");
const net = require("node:net");

const TYPES = { A: 1, NS: 2, CNAME: 5, SOA: 6, PTR: 12, MX: 15, TXT: 16, AAAA: 28, SRV: 33, NAPTR: 35, OPT: 41, TLSA: 52, ANY: 255, CAA: 257 };
const NAMES = Object.fromEntries(Object.entries(TYPES).map(([key, value]) => [value, key]));

const u8 = (value) => Buffer.from([value]);
const u16 = (value) => Buffer.from([value >> 8, value & 255]);
const u32 = (value) => Buffer.from([value >>> 24, (value >> 16) & 255, (value >> 8) & 255, value & 255]);
const text = (value) => Buffer.concat([u8(Buffer.byteLength(value)), Buffer.from(value)]);
const name = (value) => Buffer.concat([...value.split(".").filter(Boolean).map(text), u8(0)]);

function ipv6(value) {
  const [head, tail = ""] = value.split("::");
  const left = head ? head.split(":") : [];
  const right = tail ? tail.split(":") : [];
  const groups = value.includes("::") ? [...left, ...Array(8 - left.length - right.length).fill("0"), ...right] : left;
  return Buffer.concat(groups.map((group) => u16(parseInt(group, 16))));
}

const RDATA = {
  A: (address) => Buffer.from(address.split(".").map(Number)),
  AAAA: ipv6,
  NS: name,
  CNAME: name,
  PTR: name,
  MX: (priority, exchange) => Buffer.concat([u16(priority), name(exchange)]),
  TXT: (...chunks) => Buffer.concat(chunks.map(text)),
  SRV: (priority, weight, port, target) => Buffer.concat([u16(priority), u16(weight), u16(port), name(target)]),
  SOA: (mname, rname, ...numbers) => Buffer.concat([name(mname), name(rname), ...numbers.map(u32)]),
  NAPTR: (order, preference, flags, service, regexp, replacement) =>
    Buffer.concat([u16(order), u16(preference), text(flags), text(service), text(regexp), name(replacement)]),
  TLSA: (usage, selector, match, hex) => Buffer.concat([u8(usage), u8(selector), u8(match), Buffer.from(hex, "hex")]),
  CAA: (flags, tag, value) => Buffer.concat([u8(flags), text(tag), Buffer.from(value)]),
};

function parse(message) {
  let end = 12;
  const labels = [];
  while (message[end]) {
    labels.push(message.subarray(end + 1, end + 1 + message[end]).toString());
    end += message[end] + 1;
  }
  const type = message.readUInt16BE(end + 1);
  return { id: message.readUInt16BE(0), name: labels.join("."), type: NAMES[type] || String(type), question: message.subarray(12, end + 5) };
}

// An answer is `[owner, type, ttl, ...rdata arguments]`; owner "@" is the question name.
function encode(query, spec) {
  if (spec.raw) return spec.raw(query);
  const records = (spec.answers || []).map(([owner, type, ttl, ...args]) => {
    const data = RDATA[type](...args);
    const head = owner === "@" ? u16(0xc00c) : name(owner);
    return Buffer.concat([head, u16(TYPES[type]), u16(1), u32(ttl), u16(data.length), data]);
  });
  const flags = 0x8180 | (spec.tc ? 0x0200 : 0) | (spec.rcode || 0);
  const header = Buffer.concat([u16(query.id), u16(flags), u16(1), u16(records.length), u16(0), u16(0)]);
  return Buffer.concat([header, query.question, ...records]);
}

const pause = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

async function writeTcp(socket, reply, split) {
  const framed = Buffer.concat([u16(reply.length), reply]);
  const pieces = split ? [framed.subarray(0, 1), framed.subarray(1, 7), framed.subarray(7)] : [framed];
  for (const [index, piece] of pieces.entries()) {
    if (index) await pause(20);
    if (!socket.destroyed) socket.write(piece);
  }
}

function serveTcp(socket, server, respond) {
  socket.setNoDelay(true);
  const entry = { transport: "tcp", event: "connect", from: socket.remoteAddress };
  server.record(entry);
  let pending = Buffer.alloc(0);
  socket.on("data", (chunk) => {
    pending = Buffer.concat([pending, chunk]);
    while (pending.length >= 2 && pending.length >= 2 + pending.readUInt16BE(0)) {
      const message = pending.subarray(2, 2 + pending.readUInt16BE(0));
      pending = pending.subarray(2 + message.length);
      respond("tcp", message, socket.remoteAddress, (reply, spec) => writeTcp(socket, reply, spec.split));
    }
  });
  socket.on("error", () => {});
  socket.on("close", () => server.record({ transport: "tcp", event: "close" }));
}

// TCP picks the port and UDP takes the same one. The other order fails on Windows, where a UDP
// port can fall in a TCP range that Hyper-V or WSL reserves (listen EACCES).
async function bindPair(onConnection, host) {
  const type = host.includes(":") ? "udp6" : "udp4";
  for (let attempt = 0; ; attempt++) {
    const tcp = net.createServer(onConnection);
    await new Promise((resolve, reject) => {
      tcp.once("error", reject);
      tcp.listen(0, host, () => (tcp.off("error", reject), resolve()));
    });
    const port = tcp.address().port;
    const udp = dgram.createSocket(type);
    const bound = await new Promise((resolve) => {
      udp.once("error", (error) => resolve(error));
      udp.bind(port, host, () => resolve(null));
    });
    if (!bound) return [tcp, udp, port];
    if (attempt === 20) throw bound;
    await new Promise((resolve) => tcp.close(resolve));
  }
}

async function start(handler, host = "127.0.0.1") {
  const sockets = new Set();
  const waiters = [];
  const server = {
    log: [],
    record(entry) {
      server.log.push(entry);
      for (const waiter of [...waiters]) {
        if (waiter.match(entry)) waiters.splice(waiters.indexOf(waiter), 1), waiter.resolve(entry);
      }
    },
    // Resolves with the first entry, past or future, that `match` accepts.
    waitFor(match) {
      const found = server.log.find(match);
      return found ? Promise.resolve(found) : new Promise((resolve) => waiters.push({ match, resolve }));
    },
    queries: (name) => server.log.filter((entry) => entry.name === name).map((entry) => `${entry.transport}:${entry.type}`),
  };
  const respond = async (transport, message, from, send) => {
    const query = { ...parse(message), transport, from };
    server.record({ transport, event: "query", type: query.type, name: query.name, from });
    const spec = await handler(query);
    if (spec) send(encode(query, spec), spec);
  };
  const [tcp, udp, port] = await bindPair((socket) => {
    sockets.add(socket);
    socket.on("close", () => sockets.delete(socket));
    serveTcp(socket, server, respond);
  }, host);
  server.port = port;
  server.address = host.includes(":") ? `[${host}]:${server.port}` : `${host}:${server.port}`;
  udp.on("message", (message, rinfo) => respond("udp", message, rinfo.address, (reply) => udp.send(reply, rinfo.port, rinfo.address)));
  server.close = async () => {
    for (const socket of sockets) socket.destroy();
    await Promise.all([new Promise((resolve) => tcp.close(resolve)), new Promise((resolve) => udp.close(resolve))]);
  };
  return server;
}

module.exports = { start, pause, TYPES };
