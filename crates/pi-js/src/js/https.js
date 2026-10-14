// node:https: the node:http server and client over node:tls sockets.
const tls = require("tls");
const http = require("http");
const { setupServer, connectionListener } = require("internal/http_server");
const { ClientRequest } = require("internal/http_client");

function Server(options, requestListener) {
  if (!(this instanceof Server)) return new Server(options, requestListener);
  if (typeof options === "function") [requestListener, options] = [options, {}];
  options = { ...options };
  if (!options.ALPNProtocols) options.ALPNProtocols = ["http/1.1"];
  tls.Server.call(this, { noDelay: true, ...options }, connectionListener);
  setupServer(this, options, requestListener);
  this.on("tlsClientError", (error, socket) => {
    if (!this.emit("clientError", error, socket) && socket) socket.destroy(error);
  });
}
Object.setPrototypeOf(Server.prototype, tls.Server.prototype);
Object.setPrototypeOf(Server, tls.Server);
for (const name of ["setTimeout", "close", "closeAllConnections", "closeIdleConnections"]) Server.prototype[name] = http.Server.prototype[name];
Server.prototype[Symbol.asyncDispose] = http.Server.prototype[Symbol.asyncDispose];

function Agent(options) {
  if (!(this instanceof Agent)) return new Agent(options);
  http.Agent.call(this, options);
  this.defaultPort = 443;
  this.protocol = "https:";
  this.maxCachedSessions = this.options.maxCachedSessions ?? 100;
  this._sessionCache = { map: {}, list: [] };
}
Object.setPrototypeOf(Agent.prototype, http.Agent.prototype);
Object.setPrototypeOf(Agent, http.Agent);

Agent.prototype.createConnection = function (port, host, options) {
  if (port !== null && typeof port === "object") options = { ...port };
  else if (host !== null && typeof host === "object") options = { ...host, port };
  else options = { ...(options !== null && typeof options === "object" ? options : {}), port, host };
  if (typeof port === "number") options.port = port;
  if (typeof host === "string") options.host = host;
  return tls.connect(options);
};

const NAME_PARTS = ["ca", "cert", "clientCertEngine", "ciphers", "key", "pfx", "rejectUnauthorized", "servername", "minVersion", "maxVersion", "secureProtocol", "crl", "honorCipherOrder", "ecdhCurve", "dhparam", "secureOptions", "sessionIdContext", "sigalgs", "privateKeyIdentifier", "privateKeyEngine"];
const DEFINED = new Set(["rejectUnauthorized", "honorCipherOrder", "secureOptions"]);
Agent.prototype.getName = function (options = {}) {
  let name = http.Agent.prototype.getName.call(this, options);
  for (const part of NAME_PARTS) {
    name += ":";
    const value = options[part];
    if (DEFINED.has(part) ? value !== undefined : part === "servername" ? value && value !== options.host : value) name += value;
  }
  return name;
};

const globalAgent = new Agent({ keepAlive: true, scheduling: "lifo", timeout: 5000 });

function request(...args) {
  let options = {};
  if (typeof args[0] === "string") options = require("url").urlToHttpOptions(new URL(args.shift()));
  else if (args[0] instanceof URL) options = require("url").urlToHttpOptions(args.shift());
  if (args[0] && typeof args[0] !== "function") options = { ...options, ...args.shift() };
  options._defaultAgent = module.exports.globalAgent;
  return new ClientRequest(options, ...args);
}

function get(...args) {
  const req = request(...args);
  req.end();
  return req;
}

module.exports = {
  Agent,
  globalAgent,
  Server,
  createServer: (options, requestListener) => new Server(options, requestListener),
  get,
  request,
};
