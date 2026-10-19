// WebAssembly global cases; webassembly.txt is this script's output under Node 22.
const { ModuleBuilder, op } = require("./builder");
const util = require("util");

function show(label, fn) {
  try {
    console.log(label, "=>", util.inspect(fn()));
  } catch (error) {
    console.log(label, "!!", error.constructor.name, JSON.stringify(error.message));
  }
}

async function showAsync(label, fn) {
  try {
    console.log(label, "=>", util.inspect(await fn()));
  } catch (error) {
    console.log(label, "!!", error.constructor.name, error.code, JSON.stringify(error.message));
  }
}

const math = new ModuleBuilder();
const triple = math.importFunc("env", "triple", ["i32"], ["i32"]);
math.importGlobal("env", "offset", "i32");
math.func(["i32", "i32"], ["i32"], [op.local_get(0), op.local_get(1), op.i32_add], { export: "add" });
math.func(["i64"], ["i64"], [op.local_get(0), op.i64_const(2), op.i64_mul], { export: "double" });
math.func([], [], [op.unreachable], { export: "trap" });
math.func(["i32"], ["i32"], [op.local_get(0), op.call(triple), op.global_get(0), op.i32_add], { export: "callJs" });
math.func(["i32", "i32"], ["i32"], [op.local_get(0), op.local_get(1), op.i32_div_s], { export: "div" });
math.func(["f32", "f64"], ["f64"], [op.local_get(1), op.local_get(1), op.f64_mul], { export: "square" });
math.memory(1, 3);
math.export("memory", "memory", 0);
math.custom("note", [1, 2, 3]);
const mathBytes = math.build();

show("validate", () => [WebAssembly.validate(mathBytes), WebAssembly.validate(new Uint8Array([0, 97, 115, 109, 2, 0, 0, 0]))]);
show("validate type", () => WebAssembly.validate("wasm"));
show("bad magic", () => new WebAssembly.Module(new Uint8Array([1, 2, 3, 4, 1, 0, 0, 0])));
show("bad version", () => new WebAssembly.Module(new Uint8Array([0, 97, 115, 109, 2, 0, 0, 0])));
show("empty", () => new WebAssembly.Module(new ArrayBuffer(0)));
show("not a buffer", () => new WebAssembly.Module(5));
show("without new", () => WebAssembly.Module(mathBytes));
const mathModule = new WebAssembly.Module(mathBytes);
show("imports", () => WebAssembly.Module.imports(mathModule));
show("exports", () => WebAssembly.Module.exports(mathModule));
show("custom", () => WebAssembly.Module.customSections(mathModule, "note").map((buffer) => [...new Uint8Array(buffer)]));
show("no imports", () => new WebAssembly.Instance(mathModule));
show("missing module", () => new WebAssembly.Instance(mathModule, {}));
show("not callable", () => new WebAssembly.Instance(mathModule, { env: { triple: 1, offset: 0 } }));
show("bad global", () => new WebAssembly.Instance(mathModule, { env: { triple() {}, offset: "1" } }));
const { exports: e } = new WebAssembly.Instance(mathModule, { env: { triple: (x) => x * 3, offset: 100 } });
show("exports object", () => [Object.getPrototypeOf(e), Object.isFrozen(e), Object.keys(e).join()]);
show("add", () => [e.add(2, 3), e.add.name, e.add.length, e.add(2 ** 31, 1), e.add("7", {})]);
show("add bigint", () => e.add(1n, 2));
show("i64", () => [e.double(21n), e.double("5"), e.double(true), e.double(2n ** 63n)]);
show("i64 number", () => e.double(21));
show("floats", () => e.square(0, 1.5));
show("unreachable", () => e.trap());
show("divide by zero", () => e.div(1, 0));
show("overflow", () => e.div(-2147483648, -1));
show("call js", () => e.callJs(4));
show("identity", () => [e.add === e.add, e.memory === e.memory]);
show("tags", () => [String(mathModule), String(e.memory), Object.prototype.toString.call(WebAssembly)]);
show("errors", () => [new WebAssembly.RuntimeError("x").name, String(new WebAssembly.CompileError("y")), new WebAssembly.LinkError() instanceof Error]);

const buffer = e.memory.buffer;
show("memory", () => [buffer.byteLength, e.memory.grow(1), buffer.byteLength, e.memory.buffer.byteLength]);
show("grow past maximum", () => e.memory.grow(5));
show("grow negative", () => e.memory.grow(-1));
show("memory descriptor", () => new WebAssembly.Memory({}));
show("memory bounds", () => new WebAssembly.Memory({ initial: 70000 }));
show("memory maximum", () => new WebAssembly.Memory({ initial: 3, maximum: 1 }));
show("transfer memory buffer", () => e.memory.buffer.transfer());
show("transfer memory buffer in a clone", () => structuredClone(e.memory.buffer, { transfer: [e.memory.buffer] }));

const mem = new ModuleBuilder();
const peek = mem.importFunc("env", "peek", [], []);
mem.importMemory("env", "memory", 1);
mem.data(16, "hello");
mem.func(["i32"], ["i32"], [op.local_get(0), op.i32_load8_u()], { export: "load8" });
mem.func(["i32", "i32"], [], [op.local_get(0), op.local_get(1), op.i32_store8()], { export: "store8" });
mem.func(["i32"], ["i32"], [op.local_get(0), op.memory_grow], { export: "grow" });
mem.func([], [], [op.i32_const(1), op.memory_grow, op.drop, op.call(peek)], { export: "growThenPeek" });
mem.func([], ["i32"], [op.i32_const(200000), op.i32_load()], { export: "outOfBounds" });
const memory = new WebAssembly.Memory({ initial: 1, maximum: 4 });
let before = memory.buffer;
const m = new WebAssembly.Instance(new WebAssembly.Module(mem.build()), {
  env: { memory, peek: () => console.log("peek", before.byteLength, memory.buffer.byteLength) },
}).exports;
show("data segment", () => Buffer.from(memory.buffer, 16, 5).toString());
show("shared bytes", () => {
  new Uint8Array(memory.buffer)[100] = 42;
  m.store8(101, 7);
  return [m.load8(100), new Uint8Array(memory.buffer)[101], m.memory];
});
show("wasm grow", () => [m.grow(1), before.byteLength, memory.buffer.byteLength, m.grow(10)]);
before = memory.buffer;
m.growThenPeek();
show("out of bounds", () => m.outOfBounds());

const refs = new ModuleBuilder();
const echo = refs.importFunc("env", "echo", ["externref"], ["externref"]);
const pair = refs.importFunc("env", "pair", [], ["i32", "f64"]);
refs.func(["i32", "i64"], ["i64", "i32"], [op.local_get(1), op.local_get(0)], { export: "swap" });
refs.func(["externref"], ["externref"], [op.local_get(0), op.call(echo)], { export: "echo" });
refs.func(["externref"], ["i32"], [op.local_get(0), op.ref_is_null], { export: "isNull" });
refs.func([], ["i32", "f64"], [op.call(pair)], { export: "pair" });
const three = refs.func([], ["i32"], [op.i32_const(3)], { export: "three" });
refs.func([], ["funcref"], [op.ref_func(three)], { export: "getThree" });
refs.table("funcref", 2);
refs.export("table", "table", 0);
refs.elem(0, [three]);
refs.global("i32", true, [op.i32_const(5)]);
refs.export("counter", "global", 0);
refs.global("i64", false, [op.i64_const(-7)]);
refs.export("constant", "global", 1);
const refsBytes = refs.build();

const objects = new ModuleBuilder();
objects.importFunc("env", "three", [], ["i32"]);
objects.importTable("env", "table", "funcref", 1);
objects.importGlobal("env", "counter", "i32", true);
objects.func([], ["i32"], [op.global_get(0), op.call(0), op.i32_add], { export: "sum" });
objects.func([], [], [op.global_get(0), op.i32_const(1), op.i32_add, op.global_set(0)], { export: "bump" });
const recurse = new ModuleBuilder();
const callback = recurse.importFunc("env", "callback", ["i32"], ["i32"]);
const body = [op.local_get(0), op.i32_eqz, op.if(0x7f), op.i32_const(0), op.else];
recurse.func(["i32"], ["i32"], [...body, op.local_get(0), op.call(callback), op.local_get(0), op.i32_add, op.end], { export: "sum" });
recurse.func([], [], [op.call(2)], { export: "forever" });
const start = new ModuleBuilder();
const log = start.importFunc("env", "log", ["i32"], []);
start.start(start.func([], [], [op.i32_const(99), op.call(log)]));

(async () => {
  const echoed = (value) => value;
  const { module: refsModule, instance } = await WebAssembly.instantiate(refsBytes, { env: { echo: echoed, pair: () => [7, 1.5] } });
  const r = instance.exports;
  const object = {};
  show("multi value", () => [r.swap(1, 2n), r.pair()]);
  show("externref", () => [r.echo(object) === object, r.echo(undefined), r.echo(null), r.echo(5), r.isNull(null), r.isNull(undefined)]);
  show("funcref", () => [r.getThree() === r.three, r.getThree()(), r.three.name]);
  show("table", () => [r.table.length, r.table.get(0) === r.three, r.table.get(1), r.table.grow(1, r.three), r.table.length]);
  show("table index", () => r.table.get(9));
  show("table value", () => r.table.set(0, () => 1));
  show("globals", () => [r.counter.value, (r.counter.value = 9), r.counter.value, r.constant.value, r.constant.valueOf()]);
  show("immutable", () => {
    r.constant.value = 1n;
  });
  const linked = new WebAssembly.Instance(new WebAssembly.Module(objects.build()), { env: r }).exports;
  show("linked", () => [linked.sum(), (linked.bump(), r.counter.value)]);
  show("wrong type", () => new WebAssembly.Instance(new WebAssembly.Module(objects.build()), { env: { ...r, three: r.swap } }));
  show("mutable value", () => new WebAssembly.Instance(new WebAssembly.Module(objects.build()), { env: { ...r, counter: 1 } }));
  const multiBad = new WebAssembly.Instance(refsModule, { env: { echo: echoed, pair: () => [1] } }).exports;
  show("multi mismatch", () => multiBad.pair());
  let recursive;
  recursive = new WebAssembly.Instance(new WebAssembly.Module(recurse.build()), { env: { callback: (n) => recursive.exports.sum(n - 1) } });
  show("reentrant", () => recursive.exports.sum(50));
  show("stack overflow", () => recursive.exports.forever());
  show("js throws", () => new WebAssembly.Instance(new WebAssembly.Module(recurse.build()), { env: { callback: () => { throw new Error("boom"); } } }).exports.sum(1));
  show("start", () => new WebAssembly.Instance(new WebAssembly.Module(start.build()), { env: { log: (x) => console.log("start ran", x) } }) instanceof WebAssembly.Instance);
  const table = new WebAssembly.Table({ element: "externref", initial: 2 }, "x");
  show("new table", () => [table.length, table.get(0), table.grow(1), table.get(2), String(table)]);
  show("table element", () => new WebAssembly.Table({ element: "i32", initial: 1 }));
  show("new globals", () => [new WebAssembly.Global({ value: "f32", mutable: true }, 0.1).value, new WebAssembly.Global({ value: "i64" }).value, new WebAssembly.Global({ value: "externref" }).value]);
  show("global type", () => new WebAssembly.Global({ value: "x" }));
  show("v128 global", () => new WebAssembly.Global({ value: "v128" }));
  show("descriptors", () => Object.keys(Object.getOwnPropertyDescriptors(WebAssembly.Table.prototype)));
  await showAsync("instantiate module", async () => (await WebAssembly.instantiate(mathModule, { env: { triple() {}, offset: 1 } })) instanceof WebAssembly.Instance);
  await showAsync("instantiate type", () => WebAssembly.instantiate(5));
  await showAsync("instantiate imports", () => WebAssembly.instantiate(mathBytes));
  await showAsync("compile", async () => String(await WebAssembly.compile(mathBytes)));
  await showAsync("compile bad", () => WebAssembly.compile(new Uint8Array([0, 97, 115, 109, 2, 0, 0, 0])));
  const response = (init) => new Response(refsBytes, init);
  const wasmType = { headers: { "content-type": "application/wasm" } };
  await showAsync("streaming", async () => Object.keys(await WebAssembly.instantiateStreaming(response(wasmType), { env: { echo: echoed, pair() {} } })));
  await showAsync("streaming type", () => WebAssembly.compileStreaming(response()));
  await showAsync("streaming status", () => WebAssembly.compileStreaming(Promise.resolve(response({ status: 404, ...wasmType }))));
  await showAsync("streaming source", () => WebAssembly.compileStreaming(5));
  console.log(util.inspect(WebAssembly));
  console.log(util.inspect(r.counter), util.inspect(instance), util.inspect(WebAssembly.Module));
})();
