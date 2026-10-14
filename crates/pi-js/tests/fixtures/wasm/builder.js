// Builds small WebAssembly binaries for the tests, since no text format tools are available.
// Functions are declared after imports; bodies are byte arrays without the final `end`.
"use strict";

const TYPES = { i32: 0x7f, i64: 0x7e, f32: 0x7d, f64: 0x7c, v128: 0x7b, funcref: 0x70, externref: 0x6f };
const KINDS = { func: 0, table: 1, memory: 2, global: 3 };

function uleb(value) {
  const out = [];
  do {
    let byte = value & 0x7f;
    value = Math.floor(value / 128);
    if (value) byte |= 0x80;
    out.push(byte);
  } while (value);
  return out;
}

function sleb(value) {
  let big = BigInt(value);
  const out = [];
  for (;;) {
    const byte = Number(big & 0x7fn);
    big >>= 7n;
    if ((big === 0n && !(byte & 0x40)) || (big === -1n && byte & 0x40)) return out.concat(byte);
    out.push(byte | 0x80);
  }
}

function text(value) {
  const bytes = [...Buffer.from(value, "utf8")];
  return [...uleb(bytes.length), ...bytes];
}

function vec(items) {
  return [...uleb(items.length), ...items.flat()];
}

function limits(min, max) {
  return max === undefined ? [0, ...uleb(min)] : [1, ...uleb(min), ...uleb(max)];
}

const op = {
  unreachable: 0x00,
  nop: 0x01,
  block: (type = 0x40) => [0x02, type],
  loop: (type = 0x40) => [0x03, type],
  if: (type = 0x40) => [0x04, type],
  else: 0x05,
  end: 0x0b,
  br: (depth) => [0x0c, ...uleb(depth)],
  br_if: (depth) => [0x0d, ...uleb(depth)],
  return: 0x0f,
  call: (index) => [0x10, ...uleb(index)],
  call_indirect: (type, table = 0) => [0x11, ...uleb(type), ...uleb(table)],
  drop: 0x1a,
  local_get: (index) => [0x20, ...uleb(index)],
  local_set: (index) => [0x21, ...uleb(index)],
  local_tee: (index) => [0x22, ...uleb(index)],
  global_get: (index) => [0x23, ...uleb(index)],
  global_set: (index) => [0x24, ...uleb(index)],
  table_get: (index = 0) => [0x25, ...uleb(index)],
  table_set: (index = 0) => [0x26, ...uleb(index)],
  i32_load: (offset = 0) => [0x28, 2, ...uleb(offset)],
  i64_load: (offset = 0) => [0x29, 3, ...uleb(offset)],
  i32_load8_u: (offset = 0) => [0x2d, 0, ...uleb(offset)],
  i32_store: (offset = 0) => [0x36, 2, ...uleb(offset)],
  i64_store: (offset = 0) => [0x37, 3, ...uleb(offset)],
  i32_store8: (offset = 0) => [0x3a, 0, ...uleb(offset)],
  memory_size: [0x3f, 0],
  memory_grow: [0x40, 0],
  i32_const: (value) => [0x41, ...sleb(value)],
  i64_const: (value) => [0x42, ...sleb(value)],
  f32_const: (value) => [0x43, ...new Uint8Array(new Float32Array([value]).buffer)],
  f64_const: (value) => [0x44, ...new Uint8Array(new Float64Array([value]).buffer)],
  i32_eqz: 0x45,
  i32_eq: 0x46,
  i32_ne: 0x47,
  i32_lt_s: 0x48,
  i32_gt_s: 0x4a,
  i32_add: 0x6a,
  i32_sub: 0x6b,
  i32_mul: 0x6c,
  i32_div_s: 0x6d,
  i64_add: 0x7c,
  i64_mul: 0x7e,
  f32_add: 0x92,
  f64_add: 0xa0,
  f64_mul: 0xa2,
  i32_trunc_f64_s: 0xaa,
  ref_null: (type) => [0xd0, TYPES[type]],
  ref_is_null: 0xd1,
  ref_func: (index) => [0xd2, ...uleb(index)],
};

class ModuleBuilder {
  constructor() {
    this.types = [];
    this.imports = [];
    this.importedFuncs = 0;
    this.funcs = [];
    this.tables = [];
    this.memories = [];
    this.globals = [];
    this.exports = [];
    this.elements = [];
    this.datas = [];
    this.customs = [];
    this.startIndex = undefined;
  }

  type(params, results) {
    const bytes = [0x60, ...vec(params.map((t) => [TYPES[t]])), ...vec(results.map((t) => [TYPES[t]]))];
    const key = bytes.join(",");
    let index = this.types.findIndex((type) => type.join(",") === key);
    if (index < 0) index = this.types.push(bytes) - 1;
    return index;
  }

  importFunc(module, name, params = [], results = []) {
    this.imports.push([...text(module), ...text(name), 0, ...uleb(this.type(params, results))]);
    return this.importedFuncs++;
  }

  importMemory(module, name, min, max) {
    this.imports.push([...text(module), ...text(name), 2, ...limits(min, max)]);
  }

  importTable(module, name, element, min, max) {
    this.imports.push([...text(module), ...text(name), 1, TYPES[element], ...limits(min, max)]);
  }

  importGlobal(module, name, type, mutable = false) {
    this.imports.push([...text(module), ...text(name), 3, TYPES[type], mutable ? 1 : 0]);
  }

  func(params, results, body, options = {}) {
    const locals = (options.locals || []).map((t) => [1, TYPES[t]]);
    this.funcs.push({ type: this.type(params, results), code: [...vec(locals), ...body.flat(), 0x0b] });
    const index = this.importedFuncs + this.funcs.length - 1;
    if (options.export) this.export(options.export, "func", index);
    return index;
  }

  table(element, min, max) {
    this.tables.push([TYPES[element], ...limits(min, max)]);
    return this.tables.length - 1;
  }

  memory(min, max) {
    this.memories.push(limits(min, max));
    return this.memories.length - 1;
  }

  global(type, mutable, init) {
    this.globals.push([TYPES[type], mutable ? 1 : 0, ...init.flat(), 0x0b]);
    return this.globals.length - 1;
  }

  export(name, kind, index) {
    this.exports.push([...text(name), KINDS[kind], ...uleb(index)]);
  }

  start(index) {
    this.startIndex = index;
  }

  elem(offset, funcs) {
    this.elements.push([0, 0x41, ...sleb(offset), 0x0b, ...vec(funcs.map((f) => uleb(f)))]);
  }

  data(offset, bytes) {
    const data = typeof bytes === "string" ? [...Buffer.from(bytes, "utf8")] : [...bytes];
    this.datas.push([0, 0x41, ...sleb(offset), 0x0b, ...uleb(data.length), ...data]);
  }

  custom(name, bytes) {
    this.customs.push([...text(name), ...bytes]);
  }

  build() {
    const out = [0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00];
    const section = (id, items, raw) => {
      if (!raw && items.length === 0) return;
      const body = raw ? items : vec(items);
      out.push(id, ...uleb(body.length), ...body);
    };
    section(1, this.types);
    section(2, this.imports);
    section(3, this.funcs.map((f) => uleb(f.type)));
    section(4, this.tables);
    section(5, this.memories);
    section(6, this.globals);
    section(7, this.exports);
    if (this.startIndex !== undefined) section(8, uleb(this.startIndex), true);
    section(9, this.elements);
    section(10, this.funcs.map((f) => [...uleb(f.code.length), ...f.code]));
    section(11, this.datas);
    for (const custom of this.customs) section(0, custom, true);
    return new Uint8Array(out);
  }
}

module.exports = { ModuleBuilder, op };
