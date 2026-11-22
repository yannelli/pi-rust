const dns = require("node:dns");
const path = require("node:path");
const { start } = require("./dns_server.cjs");
const mark = (text) => require("fs").appendFileSync(path.join(__dirname, "stage"), `${text}\n`);
const out = {};
const zone = {
  "xn--mnich-kva.test": { A: [["@", "A", 60, "192.0.2.55"]] },
  "cname-self.test": { CNAME: [["@", "A", 60, "192.0.2.56"]] },
  "tlsa-any.test": { ANY: [["@", "A", 21, "192.0.2.57"], ["@", "TLSA", 60, 3, 1, 1, "abcd"]] },
  "v6.test": { AAAA: [["@", "AAAA", 120, "2001:db8::1"], ["@", "AAAA", 90, "2001:db8:0:0:1:0:0:2"]] },
  "alias.test": {
    A: [["@", "CNAME", 50, "mid.test"], ["mid.test", "CNAME", 400, "end.test"], ["end.test", "A", 300, "192.0.2.10"], ["end.test", "A", 30, "192.0.2.11"]],
    AAAA: [["@", "CNAME", 50, "end.test"], ["end.test", "AAAA", 300, "2001:db8::a"]],
    CNAME: [["@", "CNAME", 50, "mid.test"]],
  },
  "caa.test": { CAA: [["@", "CAA", 60, 0, "issue", "ca.example.test"], ["@", "CAA", 60, 128, "iodef", "mailto:sec@example.test"], ["@", "CAA", 60, 0, "issuewild", ";"]] },
  "ns.test": { NS: [["@", "NS", 60, "ns1.example.test"], ["@", "NS", 60, "ns2.example.test"]] },
  "ptr.test": { PTR: [["@", "PTR", 60, "host.example.test"], ["@", "PTR", 60, "other.example.test"]] },
  "soa.test": { SOA: [["@", "SOA", 60, "ns1.example.test", "admin.example.test", 2026112101, 7200, 900, 1209600, 300]] },
  "_sip._udp.srv.test": { SRV: [["@", "SRV", 60, 10, 60, 5060, "sip1.example.test"], ["@", "SRV", 60, 20, 0, 5061, "sip2.example.test"]] },
  "naptr.test": { NAPTR: [["@", "NAPTR", 60, 100, 10, "S", "SIP+D2U", "", "_sip._udp.example.test"], ["@", "NAPTR", 60, 101, 20, "U", "E2U+sip", "!^.*$!sip:info@example.test!", "."]] },
  "_443._tcp.tlsa.test": { TLSA: [["@", "TLSA", 60, 3, 1, 1, "0123456789abcdef"]] },
  "txt.test": { TXT: [["@", "TXT", 60, "v=spf1", " -all"], ["@", "TXT", 60, ""]] },
  "any.test": {
    ANY: [
      ["@", "A", 61, "192.0.2.20"],
      ["@", "AAAA", 62, "2001:db8::20"],
      ["@", "MX", 63, 5, "mx.example.test"],
      ["@", "TXT", 64, "one", "two"],
      ["@", "NS", 65, "ns.example.test"],
      ["@", "SOA", 66, "ns.example.test", "root.example.test", 7, 1, 2, 3, 4],
      ["@", "SRV", 67, 1, 2, 3, "srv.example.test"],
      ["@", "CAA", 68, 0, "issue", "ca.example.test"],
      ["@", "NAPTR", 69, 1, 2, "a", "b", "c", "d.example.test"],
      ["@", "PTR", 70, "ptr.example.test"],
    ],
  },
  "anycname.test": { ANY: [["@", "CNAME", 71, "target.test"], ["target.test", "A", 72, "192.0.2.21"], ["@", "A", 73, "192.0.2.22"], ["@", "MX", 74, 1, "mx.example.test"]] },
  "10.2.0.192.in-addr.arpa": { PTR: [["@", "PTR", 60, "v4.example.test"], ["@", "PTR", 60, "v4-alias.example.test"]] },
  "11.2.0.192.in-addr.arpa": { PTR: [["@", "CNAME", 60, "11.0/26.2.0.192.in-addr.arpa"], ["11.0/26.2.0.192.in-addr.arpa", "PTR", 60, "classless.example.test"]] },
  "1.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.8.b.d.0.1.0.0.2.ip6.arpa": { PTR: [["@", "PTR", 60, "v6.example.test"]] },
};
const server = await start((query) => {
  const answers = (zone[query.name] || {})[query.type];
  return answers ? { answers } : { rcode: zone[query.name] ? 0 : 3 };
});
const hex = (value) => Buffer.from(value).toString("hex");
const tlsa = (list) => list.map((entry) => ({ ...entry, data: [entry.data instanceof ArrayBuffer, hex(entry.data)] }));
const failure = (error) => [error.code, error.syscall, error.hostname, error.message];
const resolver = new dns.promises.Resolver({ timeout: 3000, tries: 1 });
resolver.setServers([server.address]);
mark("promises");
out.idna = await resolver.resolve4("Münich.test");
out.cnameSelf = await resolver.resolveCname("cname-self.test");
const mixed = await resolver.resolveAny("tlsa-any.test");
out.tlsaAny = [Object.keys(mixed), mixed.map((row) => row.data ? tlsa([row])[0] : row)];
out.aaaa = await resolver.resolve6("v6.test");
out.aaaaTtl = await resolver.resolve6("v6.test", { ttl: true });
out.aliasA = await resolver.resolve4("alias.test");
out.aliasATtl = await resolver.resolve4("alias.test", { ttl: true });
out.aliasAaaaTtl = await resolver.resolve6("alias.test", { ttl: true });
out.cname = await resolver.resolveCname("alias.test");
out.caa = await resolver.resolveCaa("caa.test");
out.ns = await resolver.resolveNs("ns.test");
out.ptr = await resolver.resolvePtr("ptr.test");
out.soa = await resolver.resolveSoa("soa.test");
out.srv = await resolver.resolveSrv("_sip._udp.srv.test");
out.naptr = await resolver.resolveNaptr("naptr.test");
out.tlsa = tlsa(await resolver.resolveTlsa("_443._tcp.tlsa.test"));
out.txt = await resolver.resolveTxt("txt.test");
out.any = await resolver.resolveAny("any.test");
out.anyCname = await resolver.resolveAny("anycname.test");
mark("reverse");
out.reverse4 = await resolver.reverse("192.0.2.10");
out.reverseAlias = await resolver.reverse("192.0.2.11");
out.reverse6 = await resolver.reverse("2001:db8::1");
out.reverseMissing = await resolver.reverse("192.0.2.12").catch(failure);
const invalid = (error) => [error.name, error.code, error.message, error.syscall, error.hostname, typeof error.errno];
out.reverseInvalid = await resolver.reverse("not-an-ip").then(() => "resolved", invalid);
try {
  new dns.Resolver().reverse("not-an-ip", () => {});
} catch (error) {
  out.cbReverseInvalid = invalid(error);
}
mark("generic");
out.generic = {};
for (const [type, name] of [["AAAA", "v6.test"], ["CNAME", "alias.test"], ["CAA", "caa.test"], ["NS", "ns.test"], ["PTR", "ptr.test"], ["SOA", "soa.test"], ["SRV", "_sip._udp.srv.test"], ["NAPTR", "naptr.test"], ["TLSA", "_443._tcp.tlsa.test"], ["ANY", "any.test"]]) {
  const value = await resolver.resolve(name, type);
  out.generic[type] = type === "TLSA" ? tlsa(value) : value;
}
mark("callbacks");
const callback = new dns.Resolver({ timeout: 3000, tries: 1 });
callback.setServers([server.address]);
const call = (method, ...args) => new Promise((resolve) => {
  const returned = callback[method](...args, (...result) => resolve([typeof returned, result.length, result[0] && failure(result[0]), ...result.slice(1)]));
});
out.cbAaaa = await call("resolve6", "v6.test");
out.cbTtl = await call("resolve4", "alias.test", { ttl: true });
out.cbSoa = await call("resolveSoa", "soa.test");
out.cbGeneric = await call("resolve", "_sip._udp.srv.test", "SRV");
out.cbDefaultType = await call("resolve", "alias.test");
out.cbReverse = await call("reverse", "2001:db8::1");
out.cbMissing = await call("resolveNs", "nowhere.test");
mark("defaults");
dns.setServers([server.address]);
out.defaultServers = dns.getServers().map((entry) => entry.replace(String(server.port), "PORT"));
out.defaultPromiseServers = dns.promises.getServers().map((entry) => entry.replace(String(server.port), "PORT"));
out.defaultCallback = await new Promise((resolve) => dns.resolveNaptr("naptr.test", (error, list) => resolve([error, list])));
out.defaultCallbackAny = await new Promise((resolve) => dns.resolve("v6.test", "AAAA", (error, list) => resolve([error, list])));
out.defaultReverse = await new Promise((resolve) => dns.reverse("192.0.2.10", (error, list) => resolve([error, list])));
out.defaultPromise = await dns.promises.resolveSrv("_sip._udp.srv.test");
out.defaultPromiseCaa = await dns.promises.resolve("caa.test", "CAA");
out.defaultPromiseMissing = await dns.promises.resolveTxt("nowhere.test").catch(failure);
out.queries = server.queries("alias.test");
await server.close();
return out;
