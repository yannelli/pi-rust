// node:dns and dns/promises: lookup through the system resolver, the resolve family through
// native DNS channels (one per Resolver), and the default result order.
const { codeError, errnoOf, invalidArgType, invalidArgValue, validate } = require("internal/errors");
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
  return [match[1], (Number(match[2]) & 0xffff) || 53];
}

const RECORD_TYPES = { A: "resolve4", AAAA: "resolve6", ANY: "resolveAny", CAA: "resolveCaa", CNAME: "resolveCname", MX: "resolveMx", NAPTR: "resolveNaptr", NS: "resolveNs", PTR: "resolvePtr", SOA: "resolveSoa", SRV: "resolveSrv", TLSA: "resolveTlsa", TXT: "resolveTxt" };

function validateName(value, name) {
  validate.string(value, name);
  if (value.includes("\0")) throw invalidArgValue(name, value, "must be a string without null bytes");
}

function methodFor(rrtype) {
  if (typeof rrtype !== "string") throw invalidArgType("rrtype", "of type string", rrtype);
  if (!Object.hasOwn(RECORD_TYPES, rrtype)) throw invalidArgValue("rrtype", rrtype);
  return RECORD_TYPES[rrtype];
}

class PromiseResolver {
  constructor(options) {
    const { timeout = -1, tries = 4, maxTimeout = 0 } = options || {};
    validate.integer(timeout, "options.timeout", -1, 2 ** 31 - 1);
    validate.integer(tries, "options.tries", 1, 2 ** 31 - 1);
    validate.integer(maxTimeout, "options.maxTimeout", 0, 2 ** 32 - 1);
    this._timeout = timeout;
    this._tries = tries;
    this._maxTimeout = maxTimeout;
    this._handle = null;
    this._pending = new Set();
  }
  _channel(servers) {
    this._handle ??= nativeDns.createChannel(servers, this._timeout, this._tries, this._maxTimeout);
    return this._handle;
  }
  getServers() {
    return this._channel().getServers();
  }
  setServers(servers) {
    validate.array(servers, "servers");
    const parsed = servers.map(parseServer);
    if (this._pending.size) {
      throw codeError(Error, "ERR_DNS_SET_SERVERS_FAILED",
        `c-ares failed to set servers: "There are pending queries." [${require("util").inspect(servers)}]`);
    }
    if (this._handle) this._handle.setServers(parsed);
    else this._channel(parsed);
  }
  setLocalAddress(ipv4, ipv6) {
    validateName(ipv4, "ipv4");
    if (ipv6 !== undefined) validateName(ipv6, "ipv6");
    this._channel().setLocalAddress(ipv4, ipv6);
  }
  cancel() {
    this._handle?.cancel();
    for (const cancel of this._pending) cancel();
    this._pending.clear();
  }
  _query(type, name, ttl) {
    const syscall = type === "REVERSE" ? "getHostByAddr" : `query${type[0]}${type.slice(1).toLowerCase()}`;
    let request;
    try {
      request = this._channel().query(type, name, ttl);
    } catch (error) {
      if (error.code === "EINVAL") throw Object.assign(dnsError("EINVAL", syscall, name), { errno: errnoOf("EINVAL") });
      throw error;
    }
    if (Array.isArray(request)) return Promise.resolve(request);
    if (typeof request === "string") return Promise.reject(dnsError(request, syscall, name));
    return new Promise((resolve, reject) => {
      const cancel = () => reject(dnsError("ECANCELLED", syscall, name));
      this._pending.add(cancel);
      request.then((value) => {
        if (!this._pending.delete(cancel)) return;
        if (type === "TLSA" || type === "ANY") {
          for (let i = 0; i < value.length; i++) {
            const record = value[i];
            if (record === null) delete value[i];
            else if (Array.isArray(record.data)) record.data = Uint8Array.from(record.data).buffer;
          }
        }
        resolve(value);
      }, (error) => {
        if (this._pending.delete(cancel)) reject(dnsError(error.code, syscall, name));
      });
    });
  }
  resolve(hostname, rrtype = "A") {
    return PromiseResolver.prototype[methodFor(rrtype)].call(this, hostname);
  }
}

class Resolver extends PromiseResolver {
  resolve(hostname, rrtype, callback) {
    if (typeof rrtype === "function") return this.resolve4(hostname, rrtype);
    return Resolver.prototype[methodFor(rrtype === undefined ? "A" : rrtype)].call(this, hostname, callback);
  }
}

for (const [type, method] of Object.entries({ ...RECORD_TYPES, REVERSE: "reverse" })) {
  PromiseResolver.prototype[method] = function (name, options) {
    validateName(name, "name");
    try {
      return this._query(type, name, !!options?.ttl);
    } catch (error) {
      return Promise.reject(error);
    }
  };
  Resolver.prototype[method] = function (name, options, callback) {
    validateName(name, "name");
    if (typeof options === "function") {
      callback = options;
      options = undefined;
    }
    validate.function(callback, "callback");
    this._query(type, name, !!options?.ttl).then((value) => callback(null, value), (error) => callback(error));
    return {};
  };
}

function setDefaultResultOrder(order) {
  if (!ORDERS.includes(order)) throw invalidArgValue("dnsOrder", order, "must be one of: 'verbatim', 'ipv4first', 'ipv6first'");
  defaultOrder = order;
}

const methods = [...Object.values(RECORD_TYPES), "resolve", "reverse", "getServers"];
function bindQueries(target, prototype, resolver) {
  for (const method of methods) target[method] = prototype[method].bind(resolver);
}

const codes = Object.fromEntries(CODES.map((name) => [name, name === "CANCELLED" ? "ECANCELLED" : `E${name}`]));
const order = { setDefaultResultOrder, getDefaultResultOrder: () => defaultOrder };
const promises = { ...codes, ...order, Resolver: PromiseResolver, lookup: lookupPromise, lookupService: lookupServicePromise };
const dns = { ...codes, ...order, ADDRCONFIG, V4MAPPED, ALL, Resolver, lookup, lookupService, promises };
const defaultResolver = new Resolver();
bindQueries(dns, Resolver.prototype, defaultResolver);
bindQueries(promises, PromiseResolver.prototype, defaultResolver);
dns.setServers = (servers) => {
  const resolver = new Resolver();
  resolver.setServers(servers);
  bindQueries(dns, Resolver.prototype, resolver);
  bindQueries(promises, PromiseResolver.prototype, resolver);
};
promises.setServers = (servers) => {
  const resolver = new PromiseResolver();
  resolver.setServers(servers);
  bindQueries(promises, PromiseResolver.prototype, resolver);
};

module.exports = dns;
