// IP address helpers shared by net, dgram, dns, and tls: isIP, BlockList, and SocketAddress.
const { codeError, invalidArgType, invalidArgValue, validate } = require("internal/errors");

const V4_PART = "(?:25[0-5]|2[0-4][0-9]|1[0-9][0-9]|[1-9][0-9]|[0-9])";
const V4 = `(?:${V4_PART}\\.){3}${V4_PART}`;
const IPV4 = new RegExp(`^${V4}$`);
const H = "[0-9a-fA-F]{1,4}";
const IPV6 = new RegExp(
  "^(?:" +
    `(?:${H}:){7}(?:${H}|:)|(?:${H}:){6}(?:${V4}|:${H}|:)|(?:${H}:){5}(?::${V4}|(?::${H}){1,2}|:)|` +
    `(?:${H}:){4}(?:(?::${H}){0,1}:${V4}|(?::${H}){1,3}|:)|(?:${H}:){3}(?:(?::${H}){0,2}:${V4}|(?::${H}){1,4}|:)|` +
    `(?:${H}:){2}(?:(?::${H}){0,3}:${V4}|(?::${H}){1,5}|:)|(?:${H}:){1}(?:(?::${H}){0,4}:${V4}|(?::${H}){1,6}|:)|` +
    `(?::(?:(?::${H}){0,5}:${V4}|(?::${H}){1,7}|:))` +
    ")(?:%[0-9a-zA-Z-.:]{1,})?$",
);

const isIPv4 = (input) => IPV4.test(String(input));
const isIPv6 = (input) => IPV6.test(String(input));
const isIP = (input) => (isIPv4(input) ? 4 : isIPv6(input) ? 6 : 0);

function v4ToBig(text) {
  return text.split(".").reduce((total, part) => (total << 8n) + BigInt(Number(part)), 0n);
}

// An address as a BigInt; an embedded IPv4 tail counts as two 16-bit words.
function toBig(text, family) {
  text = String(text).replace(/%.*$/, "");
  if (family === "ipv4") return v4ToBig(text);
  const v4 = text.match(/(\d+)\.(\d+)\.(\d+)\.(\d+)$/);
  if (v4) {
    const value = v4ToBig(v4[0]);
    text = text.slice(0, -v4[0].length) + (value >> 16n).toString(16) + ":" + (value & 0xffffn).toString(16);
  }
  const [head, rest] = text.includes("::") ? text.split("::") : [text, undefined];
  const left = head ? head.split(":") : [];
  const right = rest ? rest.split(":") : [];
  const words = rest === undefined ? left : [...left, ...Array(Math.max(0, 8 - left.length - right.length)).fill("0"), ...right];
  return words.reduce((total, word) => (total << 16n) + BigInt(parseInt(word, 16) || 0), 0n);
}

function normalizeFamily(family) {
  const lower = typeof family === "string" ? family.toLowerCase() : family;
  if (lower === "ipv4" || lower === 4) return "ipv4";
  if (lower === "ipv6" || lower === 6) return "ipv6";
  throw codeError(TypeError, "ERR_INVALID_ARG_VALUE", `The argument 'family' must be one of: 'ipv4', 'ipv6'. Received ${require("util").inspect(family)}`);
}

const kInternal = Symbol("kInternal");

class SocketAddress {
  constructor(options = {}) {
    validate.object(options, "options");
    const { family = "ipv4", flowlabel = 0 } = options;
    const normalized = normalizeFamily(family);
    const { address = normalized === "ipv4" ? "127.0.0.1" : "::", port = 0 } = options;
    validate.string(address, "options.address");
    if (typeof port !== "number" || !Number.isInteger(port) || port < 0 || port > 65535) {
      throw codeError(RangeError, "ERR_SOCKET_BAD_PORT", `options.port should be >= 0 and < 65536. Received ${typeof port} (${port}).`);
    }
    if ((normalized === "ipv4" && !isIPv4(address)) || (normalized === "ipv6" && !isIPv6(address))) {
      throw codeError(Error, "ERR_INVALID_ADDRESS", "Invalid socket address");
    }
    this[kInternal] = { address, port, family: normalized, flowlabel };
  }
  get address() { return this[kInternal].address; }
  get port() { return this[kInternal].port; }
  get family() { return this[kInternal].family; }
  get flowlabel() { return this[kInternal].flowlabel; }
  toJSON() { return { ...this[kInternal] }; }
  static isSocketAddress(value) { return value instanceof SocketAddress; }
  static parse(input) {
    validate.string(input, "input");
    const match = input.match(/^\[([^\]]+)\](?::(\d+))?$/) || input.match(/^([^:]+)(?::(\d+))?$/);
    if (!match) return undefined;
    try {
      return new SocketAddress({ address: match[1], port: match[2] ? Number(match[2]) : 0, family: isIPv6(match[1]) ? "ipv6" : "ipv4" });
    } catch {
      return undefined;
    }
  }
}

function addressOf(value, family, name) {
  if (value instanceof SocketAddress) return [value.address, value.family];
  validate.string(value, name);
  return [value, normalizeFamily(family)];
}

// Value of an address in the family a rule uses, or undefined when it cannot match.
function valueIn(address, family, ruleFamily) {
  if (family === ruleFamily) return toBig(address, family);
  if (family === "ipv4" && ruleFamily === "ipv6") return 0xffff00000000n + toBig(address, "ipv4");
  const big = toBig(address, "ipv6");
  if (family === "ipv6" && ruleFamily === "ipv4" && big >> 32n === 0xffffn) return big & 0xffffffffn;
  return undefined;
}

class BlockList {
  constructor() { this[kInternal] = []; }
  static isBlockList(value) { return value instanceof BlockList; }
  addAddress(address, family = "ipv4") {
    const [text, fam] = addressOf(address, family, "address");
    this[kInternal].unshift({ type: "address", family: fam, text, start: toBig(text, fam) });
  }
  addRange(start, end, family = "ipv4") {
    const [startText, fam] = addressOf(start, family, "start");
    const [endText] = addressOf(end, family, "end");
    const low = toBig(startText, fam);
    const high = toBig(endText, fam);
    if (low > high) throw invalidArgValue("start", startText, "must come before end");
    this[kInternal].unshift({ type: "range", family: fam, text: `${startText}-${endText}`, start: low, end: high });
  }
  addSubnet(network, prefix, family = "ipv4") {
    const [text, fam] = addressOf(network, family, "network");
    validate.integer(prefix, "prefix", 0, fam === "ipv4" ? 32 : 128);
    const bits = BigInt(fam === "ipv4" ? 32 : 128);
    const mask = ((1n << bits) - 1n) ^ ((1n << (bits - BigInt(prefix))) - 1n);
    this[kInternal].unshift({ type: "subnet", family: fam, text: `${text}/${prefix}`, start: toBig(text, fam) & mask, mask });
  }
  check(address, family = "ipv4") {
    let text;
    let fam;
    try {
      [text, fam] = addressOf(address, family, "address");
    } catch (error) {
      if (error.code === "ERR_INVALID_ARG_TYPE") throw error;
      return false;
    }
    if ((fam === "ipv4" && !isIPv4(text)) || (fam === "ipv6" && !isIPv6(text))) return false;
    return this[kInternal].some((rule) => {
      const value = valueIn(text, fam, rule.family);
      if (value === undefined) return false;
      if (rule.type === "address") return value === rule.start;
      if (rule.type === "range") return value >= rule.start && value <= rule.end;
      return (value & rule.mask) === rule.start;
    });
  }
  get rules() {
    const label = { address: "Address", range: "Range", subnet: "Subnet" };
    return this[kInternal].map((rule) => `${label[rule.type]}: ${rule.family === "ipv4" ? "IPv4" : "IPv6"} ${rule.text}`);
  }
  toJSON() { return this.rules; }
  fromJSON(data) {
    const list = typeof data === "string" ? JSON.parse(data) : data;
    if (!Array.isArray(list)) throw invalidArgType("data", "an instance of Array", data);
    for (const entry of [...list].reverse()) {
      const match = String(entry).match(/^(Address|Range|Subnet): (IPv4|IPv6) (.+)$/);
      if (!match) continue;
      const fam = match[2].toLowerCase();
      if (match[1] === "Address") this.addAddress(match[3], fam);
      else if (match[1] === "Range") this.addRange(...match[3].split("-"), fam);
      else this.addSubnet(match[3].split("/")[0], Number(match[3].split("/")[1]), fam);
    }
  }
}

module.exports = { isIP, isIPv4, isIPv6, BlockList, SocketAddress };
