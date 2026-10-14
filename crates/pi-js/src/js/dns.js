// node:dns and dns/promises: lookup through the system resolver, the resolve family through
// native DNS channels (one per Resolver), and the default result order.
const { codeError, invalidArgType, invalidArgValue, validate } = require("internal/errors");
const { isIP } = require("internal/net_address");
const nativeDns = native.dns;
const kCustomPromisify = Symbol.for("nodejs.util.promisify.custom");

const linux = internal.config.platform === "linux";
const ADDRCONFIG = linux ? 32 : 1024;
const V4MAPPED = linux ? 8 : 2048;
const ALL = linux ? 16 : 256;
const ORDERS = ["ipv4first", "ipv6first", "verbatim"];
let defaultOrder = "verbatim";

const CODES = ["NODATA", "FORMERR", "SERVFAIL", "NOTFOUND", "NOTIMP", "REFUSED", "BADQUERY", "BADNAME", "BADFAMILY", "BADRESP", "CONNREFUSED", "TIMEOUT", "EOF", "FILE", "NOMEM", "DESTRUCTION", "BADSTR", "BADFLAGS", "NONAME", "BADHINTS", "NOTINITIALIZED", "LOADIPHLPAPI", "ADDRGETNETWORKPARAMS", "CANCELLED"];

function dnsError(code, syscall, hostname) {
  return Object.assign(new Error(`${syscall} ${code} ${hostname}`), { errno: undefined, code, syscall, hostname });
}

function lookupOptions(options) {
  let family = 0;
  let hints = 0;
  let all = false;
  let order = defaultOrder;
  if (typeof options === "number" || typeof options === "string") family = options;
  else if (options !== undefined && options !== null) {
    validate.object(options, "options");
    if (options.family !== undefined && options.family !== null) family = options.family;
    if (options.hints !== undefined && options.hints !== null) hints = options.hints;
    if (options.all !== undefined) validate.boolean(options.all, "options.all");
    all = options.all === true;
    if (options.verbatim !== undefined) {
      validate.boolean(options.verbatim, "options.verbatim");
      order = options.verbatim ? "verbatim" : "ipv4first";
    }
    if (options.order !== undefined) {
      if (!ORDERS.includes(options.order)) throw invalidArgValue("options.order", options.order, "must be one of: 'verbatim', 'ipv4first', 'ipv6first'");
      order = options.order;
    }
  }
  if (family === "IPv4") family = 4;
  else if (family === "IPv6") family = 6;
  if (![0, 4, 6].includes(family)) {
    const name = typeof options === "object" && options !== null ? "options.family" : "family";
    throw invalidArgValue(name, family, "must be one of: 0, 4, 6");
  }
  if (typeof hints !== "number" || (hints & ~(ADDRCONFIG | ALL | V4MAPPED)) !== 0) throw invalidArgValue("hints", hints);
  return { family, hints, all, order };
}

function pickAddresses(hostname, list, { family, hints, order }) {
  let found = list.map(([address, kind]) => ({ address, family: kind }));
  if (family === 6 && hints & V4MAPPED) {
    const v6 = found.filter((entry) => entry.family === 6);
    const mapped = found.filter((entry) => entry.family === 4).map((entry) => ({ address: `::ffff:${entry.address}`, family: 6 }));
    found = v6.length && !(hints & ALL) ? v6 : [...v6, ...mapped];
  } else if (family) found = found.filter((entry) => entry.family === family);
  if (order === "ipv4first") found.sort((a, b) => a.family - b.family);
  else if (order === "ipv6first") found.sort((a, b) => b.family - a.family);
  if (!found.length) throw Object.assign(dnsError("ENOTFOUND", "getaddrinfo", hostname), { errno: -3008 });
  return found;
}

function lookupPromise(hostname, options) {
  if (hostname !== undefined && hostname !== null && typeof hostname !== "string") throw invalidArgType("hostname", "of type string", hostname);
  const parsed = lookupOptions(options);
  if (!hostname) {
    return Promise.resolve(parsed.all ? [] : { address: null, family: parsed.family === 6 ? 6 : 4 });
  }
  const literal = isIP(hostname);
  const pending = literal ? Promise.resolve([[hostname, literal]]) : nativeDns.lookup(hostname);
  return pending.then((list) => {
    const found = pickAddresses(hostname, list, parsed);
    return parsed.all ? found : found[0];
  });
}

function lookup(hostname, options, callback) {
  if (typeof options === "function") {
    callback = options;
    options = undefined;
  }
  validate.function(callback, "callback");
  const parsed = lookupOptions(options);
  lookupPromise(hostname, options).then(
    (result) => (parsed.all ? callback(null, result) : callback(null, result.address, result.family)),
    (error) => callback(error),
  );
  return {};
}
Object.defineProperty(lookup, kCustomPromisify, { value: lookupPromise, enumerable: false });

function lookupServicePromise(address, port) {
  if (arguments.length < 2) throw codeError(TypeError, "ERR_MISSING_ARGS", 'The "address" and "port" arguments must be specified');
  validate.string(address, "address");
  if (!isIP(address)) throw invalidArgValue("address", address);
  const number = Number(port);
  if ((typeof port !== "number" && typeof port !== "string") || !Number.isInteger(number) || number < 0 || number > 65535) {
    const shown = typeof port === "string" ? `type string ('${port}')` : `type ${typeof port} (${String(port)})`;
    throw codeError(RangeError, "ERR_SOCKET_BAD_PORT", `Port should be >= 0 and < 65536. Received ${shown}.`);
  }
  return nativeDns.lookupService(address, number).then(([hostname, service]) => ({ hostname, service }));
}

function lookupService(address, port, callback) {
  const promise = lookupServicePromise(address, port);
  validate.function(callback, "callback");
  promise.then(({ hostname, service }) => callback(null, hostname, service), (error) => callback(error));
}
Object.defineProperty(lookupService, kCustomPromisify, { value: lookupServicePromise, enumerable: false });

function parseServer(server, index) {
  validate.string(server, `servers[${index}]`);
  let match = server.match(/^\[([^\]]+)\](?::(\d+))?$/);
  if (!match && isIP(server)) match = [server, server, undefined];
  if (!match) match = server.match(/^([^:]+):(\d+)$/);
  if (!match || !isIP(match[1])) throw codeError(TypeError, "ERR_INVALID_IP_ADDRESS", `Invalid IP address: ${server}`);
  return [match[1], match[2] ? Number(match[2]) : 53];
}

const RECORD_TYPES = { A: "resolve4", AAAA: "resolve6", ANY: "resolveAny", CAA: "resolveCaa", CNAME: "resolveCname", MX: "resolveMx", NAPTR: "resolveNaptr", NS: "resolveNs", PTR: "resolvePtr", SOA: "resolveSoa", SRV: "resolveSrv", TXT: "resolveTxt" };

// The promise-based resolver; `dns.Resolver` wraps it with callbacks.
class PromiseResolver {
  constructor(options) {
    if (options !== undefined) validate.object(options, "options");
    const { timeout = -1, tries = 4 } = options || {};
    validate.integer(timeout, "options.timeout", -1, 2 ** 31 - 1);
    validate.integer(tries, "options.tries", 1, 2 ** 31 - 1);
    Object.assign(this, { _timeout: timeout, _tries: tries, _servers: undefined, _id: 0, _list: [], _pending: new Set() });
  }
  _channel() {
    if (!this._id) {
      const created = nativeDns.createChannel(this._servers, this._timeout, this._tries);
      this._id = created.id;
      this._list = created.servers;
    }
    return this._id;
  }
  getServers() {
    this._channel();
    return this._list.map(([ip, port]) => {
      const host = isIP(ip) === 6 ? `[${ip}]` : ip;
      return port === "53" ? ip : `${host}:${port}`;
    });
  }
  setServers(servers) {
    validate.array(servers, "servers");
    const parsed = servers.map(parseServer);
    if (this._id) nativeDns.closeChannel(this._id);
    this._id = 0;
    this._servers = parsed;
  }
  setLocalAddress(ipv4, ipv6) {
    validate.string(ipv4, "ipv4");
    if (!isIP(ipv4) || (ipv6 !== undefined && !isIP(ipv6))) throw codeError(TypeError, "ERR_INVALID_IP_ADDRESS", `Invalid IP address: ${ipv4}`);
  }
  cancel() {
    for (const entry of this._pending) entry();
    this._pending.clear();
  }
  _run(syscall, name, start) {
    return new Promise((resolve, reject) => {
      const cancel = () => reject(dnsError("ECANCELLED", syscall, name));
      this._pending.add(cancel);
      start().then(
        (value) => (this._pending.delete(cancel), resolve(value)),
        (error) => (this._pending.delete(cancel), reject(error)),
      );
    });
  }
  resolve(hostname, rrtype = "A") {
    validate.string(hostname, "name");
    if (typeof rrtype !== "string") throw invalidArgType("rrtype", "of type string", rrtype);
    const method = RECORD_TYPES[rrtype];
    if (!method) throw invalidArgValue("rrtype", rrtype);
    return this[method](hostname);
  }
  reverse(ip) {
    validate.string(ip, "ip");
    return this._run("getHostByAddr", ip, () => nativeDns.reverse(this._channel(), ip));
  }
}

for (const [type, method] of Object.entries(RECORD_TYPES)) {
  PromiseResolver.prototype[method] = function (hostname, options) {
    validate.string(hostname, "name");
    const ttl = !!(options && options.ttl);
    const syscall = `query${type[0]}${type.slice(1).toLowerCase()}`;
    return this._run(syscall, hostname, () => nativeDns.query(this._channel(), type, hostname, ttl).catch((error) => {
      throw Object.assign(error, { message: `${syscall} ${error.code} ${hostname}`, syscall });
    }));
  };
}

function callbackify(method) {
  return function (...args) {
    const callback = args.pop();
    validate.function(callback, "callback");
    PromiseResolver.prototype[method].apply(this, args).then((value) => callback(null, value), (error) => callback(error));
    return {};
  };
}

class Resolver extends PromiseResolver {}
for (const method of [...Object.values(RECORD_TYPES), "resolve", "reverse"]) Resolver.prototype[method] = callbackify(method);

const defaultResolver = new Resolver();

function setDefaultResultOrder(order) {
  if (!ORDERS.includes(order)) throw invalidArgValue("dnsOrder", order, "must be one of: 'verbatim', 'ipv4first', 'ipv6first'");
  defaultOrder = order;
}

function bindAll(target, resolver, names) {
  for (const name of names) target[name] = resolver[name].bind(resolver);
  return target;
}

const methods = [...Object.values(RECORD_TYPES), "resolve", "reverse", "getServers", "setServers"];
const codes = Object.fromEntries(CODES.map((name) => [name, name === "CANCELLED" ? "ECANCELLED" : `E${name}`]));

const promises = { ...codes, Resolver: PromiseResolver, lookup: lookupPromise, lookupService: lookupServicePromise, setDefaultResultOrder, getDefaultResultOrder: () => defaultOrder };
for (const name of methods) promises[name] = (...args) => PromiseResolver.prototype[name].apply(defaultResolver, args);

module.exports = bindAll({
  ...codes,
  ADDRCONFIG,
  V4MAPPED,
  ALL,
  Resolver,
  lookup,
  lookupService,
  setDefaultResultOrder,
  getDefaultResultOrder: () => defaultOrder,
  promises,
}, defaultResolver, methods);
