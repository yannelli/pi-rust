// node:v8: the serializer API and heap statistics. QuickJS has no V8 heap, so the numbers come
// from QuickJS's memory accounting and the snapshots hold no nodes.
const { Buffer } = require("buffer");
const { codeError, validate } = require("internal/errors");
const { Serializer: BaseSerializer } = require("internal/v8_serdes");
const { Deserializer: BaseDeserializer } = require("internal/v8_deserializer");
const nativeContext = native.asyncContext;

class Serializer extends BaseSerializer {}
Serializer.prototype._getDataCloneError = Error;
Serializer.prototype._writeHostObject = function writeHostObject(object) {
  throw new this._getDataCloneError(`Unserializable host object: ${require("util").inspect(object)}`);
};
class Deserializer extends BaseDeserializer {}

// Node's host object type indexes: index 10 is Buffer.
const BUFFER_INDEX = 10;
const viewTypes = [Int8Array, Uint8Array, Uint8ClampedArray, Int16Array, Uint16Array, Int32Array, Uint32Array,
  Float32Array, Float64Array, DataView, Buffer, BigInt64Array, BigUint64Array];
if (typeof Float16Array === "function") viewTypes.push(Float16Array);
const viewIndex = new Map(viewTypes.filter((Ctor) => Ctor !== Buffer)
  .map((Ctor) => [Object.prototype.toString.call(new Ctor(new ArrayBuffer(0))), viewTypes.indexOf(Ctor)]));

class DefaultSerializer extends Serializer {
  constructor() {
    super();
    this._setTreatArrayBufferViewsAsHostObjects(true);
  }
  _writeHostObject(view) {
    let index = BUFFER_INDEX;
    if (!Buffer.isBuffer(view)) {
      index = viewIndex.get(Object.prototype.toString.call(view));
      if (index === undefined) throw new this._getDataCloneError(`Unserializable host object: ${require("util").inspect(view)}`);
    }
    this.writeUint32(index);
    this.writeUint32(view.byteLength);
    this.writeRawBytes(new Uint8Array(view.buffer, view.byteOffset, view.byteLength));
  }
}

class DefaultDeserializer extends Deserializer {
  _readHostObject() {
    const index = this.readUint32();
    const length = this.readUint32();
    const offset = this._readRawBytes(length);
    const copy = new Uint8Array(this.buffer.buffer, this.buffer.byteOffset + offset, length).slice();
    if (index === BUFFER_INDEX) return Buffer.from(copy.buffer, 0, length);
    const Ctor = viewTypes[index];
    if (!Ctor) throw new Error("Unable to deserialize cloned data.");
    return new Ctor(copy.buffer, 0, Ctor === DataView ? length : length / Ctor.BYTES_PER_ELEMENT);
  }
}

function serialize(value) {
  const serializer = new DefaultSerializer();
  serializer.writeHeader();
  serializer.writeValue(value);
  return serializer.releaseBuffer();
}

function deserialize(buffer) {
  const deserializer = new DefaultDeserializer(buffer);
  deserializer.readHeader();
  return deserializer.readValue();
}

const DEFAULT_LIMIT = 4345298944;
let peakMalloc = 0;
function memory() {
  const usage = nativeContext.memory();
  peakMalloc = Math.max(peakMalloc, usage.mallocSize);
  const limit = usage.mallocLimit > 0 && usage.mallocLimit < Number.MAX_SAFE_INTEGER ? usage.mallocLimit : DEFAULT_LIMIT;
  return { ...usage, limit };
}

function getHeapStatistics() {
  const usage = memory();
  return {
    total_heap_size: usage.mallocSize,
    total_heap_size_executable: usage.funcCodeSize,
    total_physical_size: usage.mallocSize,
    total_available_size: Math.max(0, usage.limit - usage.mallocSize),
    used_heap_size: usage.memoryUsed,
    heap_size_limit: usage.limit,
    malloced_memory: usage.mallocSize,
    peak_malloced_memory: peakMalloc,
    does_zap_garbage: 0,
    number_of_native_contexts: 1,
    number_of_detached_contexts: 0,
    total_global_handles_size: 0,
    used_global_handles_size: 0,
    external_memory: usage.binarySize,
  };
}

// QuickJS has one heap; it is reported as old_space (objects) plus code_space and
// large_object_space (array buffers). The other V8 spaces are empty.
function getHeapSpaceStatistics() {
  const usage = memory();
  const objects = usage.objSize + usage.propSize + usage.shapeSize + usage.strSize + usage.atomSize;
  const sizes = { old_space: objects, code_space: usage.funcCodeSize, large_object_space: usage.binarySize };
  const names = ["read_only_space", "new_space", "old_space", "code_space", "shared_space", "trusted_space",
    "shared_trusted_space", "new_large_object_space", "large_object_space", "code_large_object_space",
    "shared_large_object_space", "shared_trusted_large_object_space", "trusted_large_object_space"];
  return names.map((space_name) => {
    const size = sizes[space_name] || 0;
    return { space_name, space_size: size, space_used_size: size, space_available_size: 0, physical_space_size: size };
  });
}

function getHeapCodeStatistics() {
  const usage = memory();
  return {
    code_and_metadata_size: usage.funcSize,
    bytecode_and_metadata_size: usage.funcCodeSize,
    external_script_source_size: 0,
    cpu_profiler_metadata_size: 0,
  };
}

function snapshotText() {
  const meta = {
    node_fields: ["type", "name", "id", "self_size", "edge_count", "detachedness"],
    node_types: [["hidden", "array", "string", "object", "code", "closure", "regexp", "number", "native", "synthetic"]],
    edge_fields: ["type", "name_or_index", "to_node"],
    edge_types: [["context", "element", "property", "internal", "hidden", "shortcut", "weak"]],
  };
  return JSON.stringify({ snapshot: { meta, node_count: 0, edge_count: 0, trace_function_count: 0 }, nodes: [], edges: [], strings: [] });
}

internal.heapSnapshotText = snapshotText;

let snapshotSeq = 0;
function writeHeapSnapshot(filename, options) {
  if (filename !== undefined) validate.string(filename, "filename");
  if (options !== undefined) validate.object(options, "options");
  if (filename === undefined) {
    const now = new Date();
    const pad = (n) => String(n).padStart(2, "0");
    const date = `${now.getFullYear()}${pad(now.getMonth() + 1)}${pad(now.getDate())}`;
    const time = `${pad(now.getHours())}${pad(now.getMinutes())}${pad(now.getSeconds())}`;
    filename = `Heap.${date}.${time}.${process.pid}.0.${String(++snapshotSeq).padStart(3, "0")}.heapsnapshot`;
  }
  require("fs").writeFileSync(filename, snapshotText());
  return filename;
}

function getHeapSnapshot(options) {
  if (options !== undefined) validate.object(options, "options");
  return require("stream").Readable.from([Buffer.from(snapshotText())], { objectMode: false });
}

let cachedTag;
function cachedDataVersionTag() {
  if (cachedTag === undefined) {
    let hash = 0x811c9dc5;
    for (const char of `quickjs-${process.versions.node}`) hash = Math.imul(hash ^ char.charCodeAt(0), 0x01000193) >>> 0;
    cachedTag = hash;
  }
  return cachedTag;
}

function isStringOneByteRepresentation(content) {
  validate.string(content, "content");
  for (let i = 0; i < content.length; i++) if (content.charCodeAt(i) > 255) return false;
  return true;
}

// V8 flags have no QuickJS equivalent; the call validates its argument and does nothing.
function setFlagsFromString(flags) {
  validate.string(flags, "flags");
}

function notBuilding() {
  throw codeError(Error, "ERR_NOT_BUILDING_SNAPSHOT", "Operation cannot be invoked when not building startup snapshot");
}

class GCProfiler {
  #start;
  start() {
    this.#start = Date.now();
  }
  stop() {
    if (this.#start === undefined) return undefined;
    const result = { version: 1, startTime: this.#start, endTime: Date.now(), statistics: [] };
    this.#start = undefined;
    return result;
  }
}

module.exports = {
  cachedDataVersionTag,
  getHeapSnapshot,
  getHeapStatistics,
  getHeapSpaceStatistics,
  getHeapCodeStatistics,
  setFlagsFromString,
  Serializer,
  Deserializer,
  DefaultSerializer,
  DefaultDeserializer,
  deserialize,
  takeCoverage() {},
  stopCoverage() {},
  serialize,
  writeHeapSnapshot,
  setHeapSnapshotNearHeapLimit(limit) {
    validate.integer(limit, "limit", 1);
  },
  startupSnapshot: {
    addSerializeCallback: notBuilding,
    addDeserializeCallback: notBuilding,
    setDeserializeMainFunction: notBuilding,
    isBuildingSnapshot: () => false,
  },
  GCProfiler,
  isStringOneByteRepresentation,
  queryObjects(ctor) {
    validate.function(ctor, "constructor");
    throw internal.unsupported("v8.queryObjects");
  },
  // Promise hooks are accepted and never called: QuickJS gives no before or after events.
  promiseHooks: {
    onInit: () => () => {},
    onSettled: () => () => {},
    onBefore: () => () => {},
    onAfter: () => () => {},
    createHook: () => () => {},
  },
};
