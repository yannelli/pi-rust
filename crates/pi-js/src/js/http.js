// node:http: HTTP/1.1 servers and clients in JavaScript over node:net sockets.
const { validate } = require("internal/errors");
const common = require("internal/http_common");
const { OutgoingMessage } = require("internal/http_outgoing");
const { Server, ServerResponse } = require("internal/http_server");
const { Agent, globalAgent } = require("internal/http_agent");
const { ClientRequest } = require("internal/http_client");

function request(url, options, callback) {
  return new ClientRequest(url, options, callback);
}

function get(url, options, callback) {
  const req = request(url, options, callback);
  req.end();
  return req;
}

const http = {
  METHODS: common.METHODS.slice(),
  STATUS_CODES: common.STATUS_CODES,
  Agent,
  ClientRequest,
  IncomingMessage: common.IncomingMessage,
  OutgoingMessage,
  Server,
  ServerResponse,
  createServer: (options, requestListener) => new Server(options, requestListener),
  validateHeaderName: (name, label) => common.validateHeaderName(name, label),
  validateHeaderValue: common.validateHeaderValue,
  get,
  request,
  setMaxIdleHTTPParsers(max) {
    validate.integer(max, "max", 1);
  },
  _connectionListener: require("internal/http_server").connectionListener,
};
Object.defineProperty(http, "maxHeaderSize", { enumerable: true, configurable: true, get: () => common.settings.maxHeaderSize });
let agent = globalAgent;
Object.defineProperty(http, "globalAgent", { enumerable: true, configurable: true, get: () => agent, set: (value) => (agent = value) });
module.exports = http;
