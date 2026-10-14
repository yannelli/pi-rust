// node:cluster. Workers are child_process.fork children. Servers that workers listen on are
// owned by the primary: with SCHED_RR it accepts and sends each connection to a worker over
// IPC; with SCHED_NONE it sends the listening socket to every worker.
const EventEmitter = require("events");
const { validate } = require("internal/errors");

const SCHED_NONE = 1;
const SCHED_RR = 2;
const workerId = process.env.NODE_UNIQUE_ID ?? internal.clusterWorkerId;
const isWorker = workerId !== undefined;
const cluster = new EventEmitter();
const policyName = process.env.NODE_CLUSTER_SCHED_POLICY;
const message = (act, fields) => ({ cmd: "NODE_CLUSTER", act, ...fields });

class Worker extends EventEmitter {
  constructor(options = {}) {
    super();
    this.exitedAfterDisconnect = undefined;
    this.state = options.state || "none";
    this.id = options.id | 0;
    this.process = options.process;
    this.process.on("error", (error) => this.emit("error", error));
    this.on("newListener", (name) => {
      if (name !== "message" || this._forwarding) return;
      this._forwarding = true;
      this.process.on("message", (value, handle) => this.emit("message", value, handle));
    });
  }
  send(...args) {
    return this.process.send(...args);
  }
  isDead() {
    return this.process.exitCode != null || this.process.signalCode != null;
  }
  isConnected() {
    return !!this.process.connected;
  }
}

Object.assign(cluster, {
  isWorker,
  isMaster: !isWorker,
  isPrimary: !isWorker,
  Worker,
  workers: isWorker ? undefined : {},
  settings: {},
  SCHED_NONE,
  SCHED_RR,
  schedulingPolicy: policyName === "none" ? SCHED_NONE : SCHED_RR,
});

// ---- Primary ----
const handles = new Map();
let ids = 0;
let initialized = false;

cluster.setupPrimary = function setupPrimary(options) {
  if (options !== undefined) validate.object(options, "options");
  const settings = { args: process.argv.slice(2), exec: process.argv[1], execArgv: process.execArgv, silent: false,
    ...cluster.settings, ...options };
  cluster.settings = settings;
  if (initialized) return void process.nextTick(() => cluster.emit("setup", settings));
  initialized = true;
  if (options && options.schedulingPolicy !== undefined) cluster.schedulingPolicy = options.schedulingPolicy;
  process.nextTick(() => cluster.emit("setup", settings));
};
cluster.setupMaster = cluster.setupPrimary;

// Accepts on a native listener and hands each connection to the next worker in turn.
class RoundRobinHandle {
  constructor(key, request) {
    this.key = key;
    this.workers = [];
    this.next = 0;
    this.pending = [];
    try {
      const result = native.net.listen(request, undefined, (event, value, id) => {
        if (event === "connection") this.distribute(id);
      });
      this.id = result.id;
      this.address = result.address;
    } catch (error) {
      this.error = error;
    }
  }
  add(worker) {
    this.workers.push(worker);
    for (const id of this.pending.splice(0)) this.distribute(id);
  }
  remove(worker) {
    this.workers = this.workers.filter((item) => item !== worker);
    if (this.workers.length) return false;
    if (this.id !== undefined) native.net.close(this.id);
    for (const id of this.pending.splice(0)) native.net.close(id);
    handles.delete(this.key);
    return true;
  }
  distribute(id) {
    const live = this.workers.filter((worker) => worker.isConnected());
    if (!live.length) return void this.pending.push(id);
    const worker = live[this.next++ % live.length];
    const { Socket } = require("net");
    const socket = new Socket();
    socket._handle = id;
    worker.process.send(message("newconn", { key: this.key }), socket, (error) => error && this.pending.push(id));
  }
}

// Listens in the primary and sends the listening socket to each worker.
class SharedHandle {
  constructor(key, request) {
    this.key = key;
    this.workers = [];
    this.server = require("net").createServer();
    try {
      const result = native.net.listen(request, undefined, () => {});
      native.net.stopAccepting(result.id);
      native.net.ref(result.id, false);
      this.server._handle = result.id;
      this.address = result.address;
    } catch (error) {
      this.error = error;
    }
  }
  add(worker) {
    this.workers.push(worker);
  }
  remove(worker) {
    this.workers = this.workers.filter((item) => item !== worker);
    if (this.workers.length) return false;
    if (this.server._handle !== null) native.net.close(this.server._handle);
    handles.delete(this.key);
    return true;
  }
}

function queryServer(worker, query) {
  const request = { host: query.address ?? undefined, port: query.port, backlog: query.backlog, ipv6Only: !!query.ipv6Only };
  let handle = handles.get(query.key);
  if (!handle) {
    const Handle = cluster.schedulingPolicy === SCHED_NONE || query.addressType === "udp4" ? SharedHandle : RoundRobinHandle;
    handle = new Handle(query.key, request);
    if (!handle.error) handles.set(query.key, handle);
  }
  if (handle.error) {
    return void worker.process.send(message("queryReply", { seq: query.seq, error: { message: handle.error.message, code: handle.error.code, errno: handle.error.errno, syscall: handle.error.syscall } }));
  }
  handle.add(worker);
  const reply = message("queryReply", { seq: query.seq, sockname: handle.address, shared: handle instanceof SharedHandle });
  if (handle instanceof SharedHandle) worker.process.send(reply, handle.server);
  else worker.process.send(reply);
}

function removeFromHandles(worker) {
  for (const handle of [...handles.values()]) handle.remove(worker);
}

function onPrimaryMessage(worker, value) {
  if (!value || value.cmd !== "NODE_CLUSTER") return;
  if (value.act === "online") {
    worker.state = "online";
    worker.emit("online");
    cluster.emit("online", worker);
  } else if (value.act === "queryServer") queryServer(worker, value);
  else if (value.act === "listening") {
    const address = { addressType: value.addressType, address: value.address, port: value.port, fd: undefined };
    worker.state = "listening";
    worker.emit("listening", address);
    cluster.emit("listening", worker, address);
  } else if (value.act === "exitedAfterDisconnect") worker.exitedAfterDisconnect = true;
  else if (value.act === "close") {
    const handle = handles.get(value.key);
    if (handle) handle.remove(worker);
  }
}

cluster.fork = function fork(env) {
  cluster.setupPrimary();
  const id = ++ids;
  const settings = cluster.settings;
  const childProcess = require("child_process").fork(settings.exec, settings.args, {
    cwd: settings.cwd,
    env: { ...process.env, ...env, NODE_UNIQUE_ID: `${id}` },
    execArgv: settings.execArgv,
    silent: settings.silent,
    serialization: settings.serialization,
    stdio: settings.stdio,
    windowsHide: settings.windowsHide,
  });
  const worker = new Worker({ id, process: childProcess });
  worker.disconnect = function disconnect() {
    this.exitedAfterDisconnect = true;
    if (this.isConnected()) this.process.send(message("disconnect"));
    return this;
  };
  worker.kill = worker.destroy = function kill(signal = "SIGTERM") {
    this.exitedAfterDisconnect = true;
    removeFromHandles(this);
    this.process.kill(signal);
  };
  childProcess.on("internalMessage", (value) => onPrimaryMessage(worker, value));
  childProcess.once("disconnect", () => {
    worker.state = "disconnected";
    removeFromHandles(worker);
    worker.emit("disconnect");
    cluster.emit("disconnect", worker);
  });
  childProcess.once("exit", (code, signal) => {
    worker.state = "dead";
    removeFromHandles(worker);
    if (worker.exitedAfterDisconnect === undefined) worker.exitedAfterDisconnect = false;
    delete cluster.workers[id];
    worker.emit("exit", code, signal);
    cluster.emit("exit", worker, code, signal);
  });
  cluster.on("newListener", function forward(name) {
    if (name !== "message") return;
    cluster.removeListener("newListener", forward);
    worker.on("message", (value, handle) => cluster.emit("message", worker, value, handle));
  });
  if (cluster.listenerCount("message") > 0) worker.on("message", (value, handle) => cluster.emit("message", worker, value, handle));
  cluster.workers[id] = worker;
  process.nextTick(() => cluster.emit("fork", worker));
  return worker;
};

cluster.disconnect = function disconnect(callback) {
  const workers = Object.values(cluster.workers);
  let pending = workers.length;
  const done = () => --pending <= 0 && typeof callback === "function" && callback();
  if (!pending) return void (typeof callback === "function" && process.nextTick(callback));
  for (const worker of workers) {
    if (worker.isConnected()) {
      worker.once("disconnect", done);
      worker.disconnect();
    } else done();
  }
};

// ---- Worker ----
const servers = new Map();
const queries = new Map();
let seq = 0;

function updateChannelRef() {
  if (!process.channel) return;
  if (servers.size > 0 || queries.size > 0) process.channel.ref();
  else process.channel.unref();
}

function onWorkerMessage(value, handle) {
  if (!value || value.cmd !== "NODE_CLUSTER") return;
  if (value.act === "queryReply") {
    const query = queries.get(value.seq);
    queries.delete(value.seq);
    if (query) query(value, handle);
    updateChannelRef();
  } else if (value.act === "newconn") {
    const server = servers.get(value.key);
    if (server && handle) server._addConnection(handle);
    else if (handle) handle.destroy();
  } else if (value.act === "disconnect") {
    cluster.worker.exitedAfterDisconnect = true;
    closeServersAndDisconnect();
  }
}

function closeServersAndDisconnect() {
  for (const server of [...servers.values()]) server.close();
  if (process.connected) process.disconnect();
}

// Called by net.Server.listen in a worker: ask the primary for the listening socket.
cluster._getServer = function getServer(server, request) {
  const address = request.host ?? null;
  const addressType = address === null ? 4 : require("net").isIP(address) || 4;
  const key = `${address}:${request.port ?? 0}:${addressType}`;
  const id = ++seq;
  server._clusterKey = key;
  queries.set(id, (reply, handle) => {
    if (reply.error) {
      const error = Object.assign(new Error(reply.error.message), reply.error);
      return server.emit("error", error);
    }
    if (reply.shared && handle) {
      handle._addConnection = (socket) => server._addConnection(socket);
      server._handle = handle._handle;
      server._shared = handle;
    } else server._handle = 0;
    server._address = reply.sockname;
    servers.set(key, server);
    updateChannelRef();
    process.nextTick(() => server.emit("listening"));
    const sockname = reply.sockname || {};
    process.send(message("listening", { address: sockname.address, port: sockname.port, addressType }));
  });
  updateChannelRef();
  process.send(message("queryServer", { key, address, port: request.port ?? 0, addressType, backlog: request.backlog, ipv6Only: request.ipv6Only, seq: id }));
};

// Called by net.Server.close in a worker.
cluster._closeServer = function closeServer(server) {
  const key = server._clusterKey;
  server._clusterKey = undefined;
  if (servers.get(key) !== server) return;
  servers.delete(key);
  updateChannelRef();
  if (process.connected) process.send(message("close", { key }));
};

if (isWorker) {
  internal.clusterWorkerId = workerId;
  delete process.env.NODE_UNIQUE_ID;
  const worker = new Worker({ id: Number(workerId), process, state: "online" });
  worker.disconnect = function disconnect() {
    this.exitedAfterDisconnect = true;
    if (process.connected) process.send(message("exitedAfterDisconnect"));
    closeServersAndDisconnect();
    return this;
  };
  worker.kill = worker.destroy = function kill() {
    this.exitedAfterDisconnect = true;
    closeServersAndDisconnect();
    process.exit(0);
  };
  cluster.worker = worker;
  process.on("internalMessage", onWorkerMessage);
  if (process.connected) process.send(message("online"));
}

module.exports = cluster;
