// MessagePort, MessageChannel, and BroadcastChannel over native endpoints that any runtime
// thread can post to. Messages use the structured clone format of the v8 serializer. Separate
// runtimes cannot share memory, so a SharedArrayBuffer arrives as a copy.
const EventEmitter = require("events");
const { Serializer } = require("internal/v8_serdes");
const { Deserializer } = require("internal/v8_deserializer");
const { codeError, invalidArgType, missingArgs } = require("internal/errors");
const { DOMException, Event } = require("internal/web");
const nativeWorker = native.worker;

const kId = Symbol("kId");
const kCreate = Symbol("kCreate");
const kClosed = Symbol("kClosed");
const kDetached = Symbol("kDetached");
const kStarted = Symbol("kStarted");
const kUnref = Symbol("kUnref");
const kBusy = Symbol("kBusy");
const kDrain = Symbol("kDrain");
const kPeerClosed = Symbol("kPeerClosed");
const kDetach = Symbol("kDetach");
const kWrapped = Symbol("kWrapped");
const HOST_PORT = 0xfe;
const HOST_SHARED = 0xfd;

const registry = new Map();
const untransferable = new WeakSet();
nativeWorker.setDispatcher((kind, id) => {
  const port = registry.get(id);
  if (port && kind === "close") port[kPeerClosed]();
  else if (port) port[kDrain]();
});

function dataCloneError(message) {
  return new DOMException(message, "DataCloneError");
}

class CloneSerializer extends Serializer {
  constructor(ports) {
    super();
    this._ports = ports;
  }
  _writeObject(value, kind) {
    if (value instanceof MessagePort) {
      const index = this._ports.indexOf(value);
      if (index < 0) throw dataCloneError("Object that needs transfer was found in message but not listed in transferList");
      this._assign(value);
      this._tag("\\");
      this.writeUint32(HOST_PORT);
      return this.writeUint32(index);
    }
    if (kind === "SharedArrayBuffer") {
      const bytes = new Uint8Array(value);
      this._assign(value);
      this._tag("\\");
      this.writeUint32(HOST_SHARED);
      this.writeUint32(bytes.length);
      return this.writeRawBytes(bytes);
    }
    if (value instanceof BroadcastChannel || typeof value === "function") throw dataCloneError(`${String(value)} could not be cloned.`);
    return super._writeObject(value, kind);
  }
  _writeHostObject(value) {
    throw dataCloneError(`${String(value)} could not be cloned.`);
  }
}
CloneSerializer.prototype._getDataCloneError = function DataCloneError(message) {
  return dataCloneError(message);
};

class CloneDeserializer extends Deserializer {
  constructor(buffer, ports) {
    super(buffer);
    this._ports = ports;
  }
  _readHostObject() {
    const tag = this.readUint32();
    if (tag === HOST_PORT) return this._ports[this.readUint32()];
    if (tag !== HOST_SHARED) throw new Error("Unable to deserialize cloned data.");
    const length = this.readUint32();
    const offset = this._readRawBytes(length);
    const shared = new SharedArrayBuffer(length);
    new Uint8Array(shared).set(this._bytes.subarray(offset, offset + length));
    return shared;
  }
}

function transferItems(transferList) {
  if (transferList === undefined || transferList === null) return [];
  if (typeof transferList !== "object") throw invalidArgType("transferList", "an Array or an object", transferList);
  const list = Array.isArray(transferList) ? transferList : transferList.transfer;
  if (list === undefined) return [];
  if (list === null || typeof list[Symbol.iterator] !== "function") {
    throw invalidArgType("options.transfer", "an Array", list);
  }
  return [...list];
}

// Serialize `value`, then detach what the transfer list moves.
function serialize(value, transferList, source) {
  const ports = [];
  const buffers = [];
  for (const item of transferItems(transferList)) {
    if (item !== null && typeof item === "object" && untransferable.has(item)) {
      throw dataCloneError("An ArrayBuffer is marked as untransferable");
    }
    if (item instanceof MessagePort) {
      if (item === source) throw dataCloneError("Transfer list contains source port");
      if (item[kDetached] || item[kClosed]) throw dataCloneError("MessagePort in transfer list is already detached");
      if (ports.includes(item)) throw dataCloneError("Transfer list contains duplicate MessagePort");
      ports.push(item);
    } else if (item instanceof ArrayBuffer) {
      if (buffers.includes(item)) throw dataCloneError("Transfer list contains duplicate ArrayBuffer");
      buffers.push(item);
    } else {
      throw codeError(TypeError, "ERR_INVALID_TRANSFER_OBJECT", "Found invalid object in transferList");
    }
  }
  const serializer = new CloneSerializer(ports);
  buffers.forEach((buffer, index) => serializer.transferArrayBuffer(index, buffer));
  serializer.writeHeader();
  serializer.writeValue(value);
  const data = serializer.releaseBuffer();
  const moved = buffers.map((buffer) => {
    const copy = new Uint8Array(buffer.slice(0));
    if (typeof buffer.transfer === "function") buffer.transfer();
    return copy;
  });
  for (const port of ports) port[kDetach]();
  return { data, buffers: moved, ports: ports.map((port) => port[kId]) };
}

function deserialize(envelope) {
  const ports = envelope.ports.map((id) => new MessagePort(kCreate, id));
  const deserializer = new CloneDeserializer(envelope.data, ports);
  envelope.buffers.forEach((bytes, index) => deserializer.transferArrayBuffer(index, bytes.buffer));
  deserializer.readHeader();
  return { value: deserializer.readValue(), ports };
}

function structuredClone(value, options) {
  if (arguments.length === 0) throw new TypeError("The \"value\" argument must be specified");
  if (options !== undefined && (options === null || typeof options !== "object")) {
    throw invalidArgType("options", "of type object", options);
  }
  return deserialize(serialize(value, options)).value;
}

class MessageEvent extends Event {
  constructor(type, init = {}) {
    super(type, init);
    this.data = init.data;
    this.origin = init.origin ?? "";
    this.lastEventId = init.lastEventId ?? "";
    this.source = init.source ?? null;
    this.ports = Object.freeze([...(init.ports || [])]);
  }
}

// Node's MessagePort is a NodeEventTarget: `on('message', value)` and
// `addEventListener('message', event)` both work.
class PortBase extends EventEmitter {
  constructor(id) {
    super();
    this[kId] = id;
    this[kClosed] = false;
    this[kStarted] = false;
    this[kUnref] = false;
    this[kBusy] = false;
    registry.set(id, this);
    this.on("newListener", (name) => {
      if (name === "message") process.nextTick(() => (this.start(), this._updateRef()));
    });
    this.on("removeListener", (name) => name === "message" && this._updateRef());
  }
  _updateRef() {
    const busy = !this[kClosed] && !this[kUnref] && this.listenerCount("message") > 0;
    if (busy !== this[kBusy]) nativeWorker.busy((this[kBusy] = busy));
  }
  start() {
    if (this[kStarted] || this[kClosed]) return;
    this[kStarted] = true;
    process.nextTick(() => this[kDrain]());
  }
  [kDrain]() {
    while (this[kStarted] && !this[kClosed]) {
      const envelope = nativeWorker.receive(this[kId]);
      if (!envelope) return;
      let message;
      try {
        message = deserialize(envelope);
      } catch (error) {
        this.emit("messageerror", error);
        continue;
      }
      try {
        this.emit("message", message.value, message.ports);
      } catch (error) {
        queueMicrotask(() => { throw error; });
      }
    }
  }
  [kPeerClosed]() {
    this[kDrain]();
    this.close();
  }
  close() {
    if (this[kClosed]) return;
    this[kClosed] = true;
    registry.delete(this[kId]);
    nativeWorker.close(this[kId]);
    this._updateRef();
    process.nextTick(() => this.emit("close"));
  }
  ref() {
    this[kUnref] = false;
    this._updateRef();
    return this;
  }
  unref() {
    this[kUnref] = true;
    this._updateRef();
    return this;
  }
  hasRef() {
    return !this[kClosed] && !this[kUnref] && this.listenerCount("message") > 0;
  }
  addEventListener(type, listener, options) {
    if (typeof listener !== "function" && (listener === null || typeof listener !== "object")) return;
    const once = typeof options === "object" && options !== null && !!options.once;
    const call = typeof listener === "function" ? listener : (event) => listener.handleEvent(event);
    const wrapped = (data, ports) => call.call(this, type === "message" || type === "messageerror"
      ? new MessageEvent(type, { data, ports }) : Object.assign(new Event(type), { data }));
    wrapped[kWrapped] = listener;
    if (once) this.once(type, wrapped);
    else this.on(type, wrapped);
  }
  removeEventListener(type, listener) {
    const found = this.rawListeners(type).find((item) => (item.listener || item)[kWrapped] === listener);
    if (found) this.removeListener(type, found.listener || found);
  }
  dispatchEvent(event) {
    this.emit(event.type, event.data);
    return true;
  }
}

for (const type of ["message", "messageerror"]) {
  Object.defineProperty(PortBase.prototype, `on${type}`, {
    configurable: true,
    get() {
      return this[`_on${type}`] ?? null;
    },
    set(handler) {
      if (this[`_on${type}`]) this.removeEventListener(type, this[`_on${type}`]);
      this[`_on${type}`] = typeof handler === "function" ? handler : null;
      if (this[`_on${type}`]) this.addEventListener(type, handler);
    },
  });
}

class MessagePort extends PortBase {
  constructor(token, id) {
    if (token !== kCreate) throw codeError(TypeError, "ERR_CONSTRUCT_CALL_INVALID", "Constructor cannot be called");
    super(id);
    nativeWorker.attach(id);
  }
  postMessage(value, transferList) {
    if (arguments.length === 0) throw missingArgs("message");
    if (this[kClosed]) return;
    const envelope = serialize(value, transferList, this);
    nativeWorker.post(this[kId], envelope.data, envelope.buffers, envelope.ports);
  }
  [kDetach]() {
    this[kDetached] = true;
    this[kClosed] = true;
    registry.delete(this[kId]);
    nativeWorker.detach(this[kId]);
    this._updateRef();
  }
  get [Symbol.toStringTag]() {
    return "MessagePort";
  }
}

class MessageChannel {
  constructor() {
    const [first, second] = nativeWorker.channel();
    this.port1 = new MessagePort(kCreate, first);
    this.port2 = new MessagePort(kCreate, second);
  }
}

class BroadcastChannel extends PortBase {
  constructor(name) {
    if (arguments.length === 0) throw missingArgs("name");
    const id = nativeWorker.join(`${name}`);
    super(id);
    this.name = `${name}`;
    nativeWorker.attach(id);
  }
  postMessage(message) {
    if (arguments.length === 0) throw missingArgs("message");
    if (this[kClosed]) throw new DOMException("BroadcastChannel is closed.", "InvalidStateError");
    nativeWorker.broadcast(this[kId], serialize(message).data);
  }
  get [Symbol.toStringTag]() {
    return "BroadcastChannel";
  }
}

function receiveMessageOnPort(port) {
  if (!(port instanceof PortBase)) throw invalidArgType("port", "an instance of MessagePort", port);
  if (port[kClosed]) return undefined;
  const envelope = nativeWorker.receive(port[kId]);
  return envelope ? { message: deserialize(envelope).value } : undefined;
}

function markAsUntransferable(object) {
  if ((typeof object === "object" && object !== null) || typeof object === "function") untransferable.add(object);
}

function isMarkedAsUntransferable(object) {
  return (typeof object === "object" && object !== null) || typeof object === "function" ? untransferable.has(object) : false;
}

function moveMessagePortToContext(port, contextifiedSandbox) {
  if (!(port instanceof MessagePort)) throw invalidArgType("port", "an instance of MessagePort", port);
  if (contextifiedSandbox === null || typeof contextifiedSandbox !== "object" || !require("vm").isContext(contextifiedSandbox)) {
    throw invalidArgType("contextifiedSandbox", "an vm.Context", contextifiedSandbox);
  }
  return port;
}

module.exports = {
  MessagePort,
  MessageChannel,
  BroadcastChannel,
  MessageEvent,
  structuredClone,
  receiveMessageOnPort,
  markAsUntransferable,
  isMarkedAsUntransferable,
  moveMessagePortToContext,
  createPort: (id) => new MessagePort(kCreate, id),
};
