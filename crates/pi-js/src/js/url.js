// node:url: WHATWG URL and URLSearchParams on the Rust url parser, plus the legacy API.
const encoding = native.encoding;
const querystring = require("querystring");
const customInspect = Symbol.for("nodejs.util.inspect.custom");
const FIELDS = ["href", "origin", "protocol", "username", "password", "host", "hostname", "port", "pathname", "search", "hash"];
const SLASHED = new Set(["http:", "https:", "ftp:", "gopher:", "file:", "ws:", "wss:"]);

function invalidUrl(input) {
  const error = new TypeError("Invalid URL");
  error.code = "ERR_INVALID_URL";
  error.input = input;
  return error;
}

function compareCodeUnits(a, b) {
  if (a < b) return -1;
  if (a > b) return 1;
  return 0;
}

class URLSearchParams {
  #pairs = [];
  #onChange = null;
  constructor(init = "") {
    if (init instanceof URLSearchParams) this.#pairs = init.#pairs.map((pair) => pair.slice());
    else if (init !== null && typeof init === "object" && typeof init[Symbol.iterator] === "function") {
      for (const pair of init) {
        const items = Array.from(pair);
        if (items.length !== 2) throw Object.assign(new TypeError("Each query pair must be an iterable [name, value] tuple"), { code: "ERR_INVALID_TUPLE" });
        this.#pairs.push([String(items[0]), String(items[1])]);
      }
    } else if (init !== null && typeof init === "object") {
      for (const key of Object.keys(init)) this.#pairs.push([key, String(init[key])]);
    } else {
      this.#parse(String(init));
    }
  }
  #parse(text) {
    this.#pairs = [];
    if (text.startsWith("?")) text = text.slice(1);
    for (const part of text.split("&")) {
      if (part === "") continue;
      const index = part.indexOf("=");
      const name = index >= 0 ? part.slice(0, index) : part;
      const value = index >= 0 ? part.slice(index + 1) : "";
      this.#pairs.push([internal.form.decode(name), internal.form.decode(value)]);
    }
  }
  #update() {
    if (this.#onChange) this.#onChange(this.toString());
  }
  static link(params, text, onChange) {
    params.#parse(text);
    params.#onChange = onChange;
  }
  static reparse(params, text) {
    params.#parse(text);
  }
  get size() {
    return this.#pairs.length;
  }
  append(name, value) {
    this.#pairs.push([String(name), String(value)]);
    this.#update();
  }
  delete(name, value) {
    name = String(name);
    this.#pairs = this.#pairs.filter(([key, item]) => key !== name || (value !== undefined && item !== String(value)));
    this.#update();
  }
  get(name) {
    const pair = this.#pairs.find(([key]) => key === String(name));
    return pair ? pair[1] : null;
  }
  getAll(name) {
    return this.#pairs.filter(([key]) => key === String(name)).map(([, value]) => value);
  }
  has(name, value) {
    return this.#pairs.some(([key, item]) => key === String(name) && (value === undefined || item === String(value)));
  }
  set(name, value) {
    name = String(name);
    const index = this.#pairs.findIndex(([key]) => key === name);
    if (index < 0) this.#pairs.push([name, String(value)]);
    else {
      this.#pairs[index][1] = String(value);
      this.#pairs = this.#pairs.filter(([key], position) => key !== name || position === index);
    }
    this.#update();
  }
  sort() {
    this.#pairs.sort(([a], [b]) => compareCodeUnits(a, b));
    this.#update();
  }
  forEach(callback, thisArg) {
    for (const [key, value] of this.#pairs) callback.call(thisArg, value, key, this);
  }
  *entries() {
    for (const [key, value] of this.#pairs) yield [key, value];
  }
  *keys() {
    for (const [key] of this.#pairs) yield key;
  }
  *values() {
    for (const [, value] of this.#pairs) yield value;
  }
  [Symbol.iterator]() {
    return this.entries();
  }
  toString() {
    return this.#pairs.map(([key, value]) => `${internal.form.encode(key)}=${internal.form.encode(value)}`).join("&");
  }
  get [Symbol.toStringTag]() {
    return "URLSearchParams";
  }
  [customInspect](depth, options, inspect) {
    const body = this.#pairs.map(([key, value]) => `${inspect(key)} => ${inspect(value)}`).join(", ");
    return `URLSearchParams {${body ? ` ${body} ` : ""}}`;
  }
}

class URL {
  #parts;
  #params = null;
  constructor(input, base) {
    if (arguments.length === 0) throw Object.assign(new TypeError('The "url" argument must be specified'), { code: "ERR_MISSING_ARGS" });
    const text = String(input);
    const parts = encoding.urlParse(text, base === undefined ? undefined : String(base));
    if (!parts) throw invalidUrl(text);
    this.#parts = parts;
  }
  static canParse(input, base) {
    return encoding.urlParse(String(input), base === undefined ? undefined : String(base)) !== undefined;
  }
  static parse(input, base) {
    return URL.canParse(input, base) ? new URL(input, base) : null;
  }
  static createObjectURL(obj) {
    require("buffer");
    return internal.createObjectURL(obj);
  }
  static revokeObjectURL(...args) {
    require("buffer");
    return internal.revokeObjectURL(...args);
  }
  #set(field, value) {
    const parts = encoding.urlSet(this.#parts[0], field, String(value));
    if (parts) this.#parts = parts;
    if (this.#params) URLSearchParams.reparse(this.#params, this.#parts[9]);
  }
  get searchParams() {
    if (!this.#params) {
      this.#params = new URLSearchParams();
      URLSearchParams.link(this.#params, this.#parts[9], (query) => {
        const parts = encoding.urlSet(this.#parts[0], "search", query);
        if (parts) this.#parts = parts;
      });
    }
    return this.#params;
  }
  get href() {
    return this.#parts[0];
  }
  set href(value) {
    const parts = encoding.urlParse(String(value));
    if (!parts) throw invalidUrl(String(value));
    this.#parts = parts;
    if (this.#params) URLSearchParams.reparse(this.#params, this.#parts[9]);
  }
  get origin() {
    return this.#parts[1];
  }
  toString() {
    return this.#parts[0];
  }
  toJSON() {
    return this.#parts[0];
  }
  get [Symbol.toStringTag]() {
    return "URL";
  }
  static {
    FIELDS.forEach((field, index) => {
      if (field === "href" || field === "origin") return;
      Object.defineProperty(this.prototype, field, {
        enumerable: true,
        configurable: true,
        get() {
          return this.#parts[index];
        },
        set(value) {
          this.#set(field, value);
        },
      });
    });
  }
  [customInspect](depth, options, inspect) {
    const object = {};
    FIELDS.forEach((field, index) => {
      object[field] = this.#parts[index];
    });
    object.searchParams = this.searchParams;
    return `URL ${inspect(object, options)}`;
  }
}

function fileURLToPath(value) {
  const href = typeof value === "string" ? value : value instanceof URL ? value.href : null;
  if (href === null) throw Object.assign(new TypeError('The "path" argument must be of type string or an instance of URL'), { code: "ERR_INVALID_ARG_TYPE" });
  return encoding.fileUrlToPath(href);
}

function pathToFileURL(path) {
  return new URL(encoding.pathToFileUrl(String(path)));
}

function urlToHttpOptions(url) {
  const options = {
    protocol: url.protocol,
    hostname: url.hostname.startsWith("[") ? url.hostname.slice(1, -1) : url.hostname,
    hash: url.hash,
    search: url.search,
    pathname: url.pathname,
    path: `${url.pathname || ""}${url.search || ""}`,
    href: url.href,
  };
  if (url.port !== "") options.port = Number(url.port);
  if (url.username || url.password) options.auth = `${decodeURIComponent(url.username)}:${decodeURIComponent(url.password)}`;
  return options;
}

// Legacy API.
function Url() {
  for (const key of ["protocol", "slashes", "auth", "host", "port", "hostname", "hash", "search", "query", "pathname", "path", "href"]) {
    this[key] = null;
  }
}

function urlParse(input, parseQueryString = false, slashesDenoteHost = false) {
  if (input instanceof Url) return input;
  if (typeof input !== "string") throw Object.assign(new TypeError('The "url" argument must be of type string'), { code: "ERR_INVALID_ARG_TYPE" });
  const result = new Url();
  let rest = input.trim().replace(/\\/g, "/");
  const hashAt = rest.indexOf("#");
  if (hashAt >= 0) {
    result.hash = rest.slice(hashAt);
    rest = rest.slice(0, hashAt);
  }
  const queryAt = rest.indexOf("?");
  if (queryAt >= 0) {
    result.search = rest.slice(queryAt);
    result.query = rest.slice(queryAt + 1);
    rest = rest.slice(0, queryAt);
  } else if (parseQueryString) {
    result.search = "";
    result.query = {};
  }
  const protocol = /^[a-z0-9.+-]+:/i.exec(rest);
  if (protocol) {
    result.protocol = protocol[0].toLowerCase();
    rest = rest.slice(protocol[0].length);
  }
  const hasSlashes = rest.startsWith("//") && result.protocol !== "javascript:";
  if (hasSlashes && (protocol || slashesDenoteHost)) {
    result.slashes = true;
    rest = rest.slice(2);
    const end = rest.indexOf("/") >= 0 ? rest.indexOf("/") : rest.length;
    let host = rest.slice(0, end);
    rest = rest.slice(end);
    const at = host.lastIndexOf("@");
    if (at >= 0) {
      result.auth = decodeURIComponent(host.slice(0, at));
      host = host.slice(at + 1);
    }
    const port = /:(\d*)$/.exec(host);
    if (port) {
      if (port[1]) result.port = port[1];
      host = host.slice(0, port.index);
    }
    result.hostname = host.toLowerCase().replace(/^\[(.*)\]$/, "$1");
    result.host = (host.startsWith("[") ? `[${result.hostname}]` : result.hostname) + (result.port ? `:${result.port}` : "");
  }
  result.pathname = rest || (result.hostname !== null && SLASHED.has(result.protocol) ? "/" : null);
  if (parseQueryString && typeof result.query === "string") result.query = querystring.parse(result.query);
  result.path = result.pathname === null && result.search === null ? null : (result.pathname || "") + (result.search || "");
  result.href = urlFormat(result);
  return result;
}

function urlFormat(object, options) {
  if (typeof object === "string") object = urlParse(object);
  if (object instanceof URL) return formatWhatwg(object, options || {});
  let protocol = object.protocol || "";
  if (protocol && !protocol.endsWith(":")) protocol += ":";
  const auth = object.auth ? `${encodeURIComponent(object.auth).replace(/%3A/i, ":")}@` : "";
  let host = "";
  if (object.host) host = auth + object.host;
  else if (object.hostname) {
    const name = object.hostname.includes(":") && !object.hostname.startsWith("[") ? `[${object.hostname}]` : object.hostname;
    host = auth + name + (object.port ? `:${object.port}` : "");
  }
  let pathname = object.pathname || "";
  let search = object.search || "";
  if (!search && object.query && typeof object.query === "object") {
    const query = querystring.stringify(object.query);
    if (query) search = `?${query}`;
  }
  let hash = object.hash || "";
  if (object.slashes || ((!protocol || SLASHED.has(protocol)) && host)) {
    host = `//${host}`;
    if (pathname && !pathname.startsWith("/")) pathname = `/${pathname}`;
  }
  if (search && !search.startsWith("?")) search = `?${search}`;
  if (hash && !hash.startsWith("#")) hash = `#${hash}`;
  pathname = pathname.replace(/[?#]/g, encodeURIComponent);
  return protocol + host + pathname + search.replace("#", "%23") + hash;
}

function formatWhatwg(url, { auth = true, fragment = true, search = true, unicode = false }) {
  let out = url.protocol;
  if (url.host || url.protocol === "file:") {
    out += "//";
    if (auth && (url.username || url.password)) out += url.username + (url.password ? `:${url.password}` : "") + "@";
    out += unicode ? encoding.domainToUnicode(url.hostname) + (url.port ? `:${url.port}` : "") : url.host;
  }
  out += url.pathname;
  if (search) out += url.search;
  if (fragment) out += url.hash;
  return out;
}

function urlResolve(source, relative) {
  const resolved = new URL(relative, new URL(source, "resolve://"));
  if (resolved.protocol === "resolve:") return resolved.pathname + resolved.search + resolved.hash;
  return resolved.toString();
}

Url.prototype.format = function format() {
  return urlFormat(this);
};
Url.prototype.resolve = function resolve(relative) {
  return urlResolve(urlFormat(this), relative);
};

module.exports = {
  URL,
  URLSearchParams,
  Url,
  parse: urlParse,
  format: urlFormat,
  resolve: urlResolve,
  resolveObject: (source, relative) => urlParse(urlResolve(source, relative)),
  fileURLToPath,
  pathToFileURL,
  urlToHttpOptions,
  domainToASCII: (domain) => encoding.domainToASCII(String(domain)),
  domainToUnicode: (domain) => encoding.domainToUnicode(String(domain)),
};
