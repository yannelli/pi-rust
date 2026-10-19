const { Worker, MessageChannel } = require("node:worker_threads");
const out = {};
const once = (emitter, name) => new Promise((resolve) => emitter.once(name, resolve));
const shared = new SharedArrayBuffer(16);
const view = new Int32Array(shared);
const source = `
const { parentPort, workerData } = require("node:worker_threads");
const view = new Int32Array(workerData.shared);
view[1] = 41;
Atomics.add(view, 1, 1);
Atomics.store(view, 0, 1);
Atomics.notify(view, 0);
parentPort.on("message", ({ again, growable }) => {
  const other = new Int32Array(again);
  const result = Atomics.wait(other, 2, 0, 10000);
  parentPort.postMessage([result !== "timed-out", Atomics.load(other, 2), again === workerData.shared, growable.growable, growable.maxByteLength, growable.byteLength]);
});
`;
const worker = new Worker(source, { eval: true, workerData: { shared } });
// The main thread blocks until the worker stores 1, which only shared memory can show.
const waited = Atomics.wait(view, 0, 0, 10000);
out.mainWait = [waited !== "timed-out", Atomics.load(view, 0), Atomics.load(view, 1)];
const reply = once(worker, "message");
worker.postMessage({ again: shared, growable: new SharedArrayBuffer(4, { maxByteLength: 8 }) });
await new Promise((resolve) => setTimeout(resolve, 20));
Atomics.store(view, 2, 7);
out.notified = Atomics.notify(view, 2) <= 1;
out.worker = await reply;
const clone = structuredClone(shared);
new Int32Array(clone)[3] = 5;
out.clone = [clone !== shared, clone instanceof SharedArrayBuffer, view[3]];
const { port1, port2 } = new MessageChannel();
port1.postMessage({ shared });
const [message] = await once(port2, "message").then((value) => [value]);
new Int32Array(message.shared)[3] = 9;
out.channel = [message.shared !== shared, view[3]];
port1.close();
await worker.terminate();
return out;
