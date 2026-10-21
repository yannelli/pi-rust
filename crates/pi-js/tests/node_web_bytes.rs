//! Readable byte streams with BYOB readers, FileHandle.readableWebStream, and Duplex.from for
//! promises, async functions, Blobs, and web streams. Expected values come from Node 22.

mod common;

use common::*;

const BYOB_SCRIPT: &str = r##"
const web = require("stream/web");
const out = {};
  out.globals = ["ReadableStreamBYOBReader", "ReadableByteStreamController", "ReadableStreamBYOBRequest"].map((n) => typeof globalThis[n] === "function" && globalThis[n] === web[n]);
  let counter = 0;
  let seenRequest;
  const pulled = new ReadableStream({
    type: "bytes",
    start(c) { out.controller = [c.constructor.name, c instanceof web.ReadableByteStreamController, c.byobRequest, c.desiredSize]; },
    pull(c) {
      const req = c.byobRequest;
      seenRequest = seenRequest || [req.constructor.name, req.view.constructor.name, req.view.byteLength, req.view.byteOffset];
      if (counter >= 3) { c.close(); req.respond(0); return; }
      const v = req.view;
      for (let i = 0; i < Math.min(v.byteLength, 4); i++) v[i] = 65 + counter;
      counter++;
      req.respond(Math.min(v.byteLength, 4));
    },
  });
  const reader = pulled.getReader({ mode: "byob" });
  out.reader = [reader.constructor.name, reader instanceof web.ReadableStreamBYOBReader, pulled.locked];
  const buf = new Uint8Array(10);
  const r1 = await reader.read(buf);
  out.r1 = [r1.done, r1.value.constructor.name, Array.from(r1.value), r1.value.buffer.byteLength, buf.byteLength];
  out.seenRequest = seenRequest;
  const r2 = await reader.read(new Uint16Array(4));
  out.r2 = [r2.done, r2.value.constructor.name, r2.value.length, r2.value.byteLength];
  const r3 = await reader.read(new Uint8Array(2));
  out.r3 = [r3.done, Array.from(r3.value)];
  const r4 = await reader.read(new Uint8Array(8));
  out.r4 = [r4.done, Array.from(r4.value)];
  const r5 = await reader.read(new Uint8Array(8));
  out.r5 = [r5.done, r5.value.constructor.name, r5.value.byteLength, r5.value.buffer.byteLength];
  const r6 = await reader.read(new Uint8Array(3));
  out.r6 = [r6.done, r6.value.byteLength];
  await reader.closed;
  out.closed = true;

  // enqueue path: chunks queued before the read, then a min read.
  let ctrl;
  const pushed = new ReadableStream({ type: "bytes", start(c) { ctrl = c; } });
  const br = pushed.getReader({ mode: "byob" });
  ctrl.enqueue(new Uint8Array([1, 2, 3]));
  const chunk = new Uint8Array([4, 5, 6, 7, 8]);
  ctrl.enqueue(chunk);
  out.enqueueDetached = chunk.byteLength;
  out.desired = ctrl.desiredSize;
  const q1 = await br.read(new Uint8Array(4));
  out.q1 = Array.from(q1.value);
  const pending = br.read(new Uint8Array(6), { min: 6 });
  ctrl.enqueue(new Uint8Array([9, 10]));
  ctrl.enqueue(new Uint8Array([11]));
  const q2 = await pending;
  out.q2 = Array.from(q2.value);
  ctrl.enqueue(new Uint8Array([12, 13]));
  ctrl.close();
  out.q3 = Array.from((await br.read(new Uint8Array(4))).value);
  const q4 = await br.read(new Uint8Array(4));
  out.q4 = [q4.done, q4.value.byteLength];

  // default reader on a byte stream, with autoAllocateChunkSize
  let n = 0;
  const auto = new ReadableStream({
    type: "bytes",
    autoAllocateChunkSize: 5,
    pull(c) {
      if (n++ === 2) { c.close(); c.byobRequest.respond(0); return; }
      const v = c.byobRequest.view;
      v[0] = 100 + n; v[1] = 200;
      c.byobRequest.respond(2);
    },
  });
  const chunks = [];
  for await (const c of auto) chunks.push([c.constructor.name, Array.from(c), c.buffer.byteLength]);
  out.auto = chunks;

  // default reader with enqueue on a byte stream
  const plain = new ReadableStream({ type: "bytes", start(c) { c.enqueue(new Uint8Array([7, 8])); c.close(); } });
  const pr = plain.getReader();
  const p1 = await pr.read();
  out.plainDefault = [p1.value.constructor.name, Array.from(p1.value), (await pr.read()).done];

  // errors
  try { new ReadableStream().getReader({ mode: "byob" }); } catch (e) { out.notBytes = [e.name, e.code, e.message]; }
  try { new ReadableStream({ type: "bytes" }).getReader({ mode: "nope" }); } catch (e) { out.badMode = [e.name, e.code, e.message]; }
  const er = new ReadableStream({ type: "bytes" }).getReader({ mode: "byob" });
  await er.read(new Uint8Array(0)).catch((e) => (out.zero = [e.name, e.code, e.message]));
  await er.read("x").catch((e) => (out.notView = [e.name, e.code, e.message]));
  await er.read(new Uint8Array(4), { min: 5 }).catch((e) => (out.minRange = [e.name, e.code, e.message]));
  await er.read(new Uint8Array(4), { min: 0 }).catch((e) => (out.minZero = [e.name, e.code, e.message]));
  const pendingRead = er.read(new Uint8Array(4));
  er.releaseLock();
  await pendingRead.catch((e) => (out.released = [e.name, e.code, e.message]));
  await er.closed.catch((e) => (out.releasedClosed = [e.name, e.code]));
  let c2;
  const cancelled = new ReadableStream({ type: "bytes", start(c) { c2 = c; }, cancel(reason) { out.cancelReason = reason; } });
  const cr = cancelled.getReader({ mode: "byob" });
  const cp = cr.read(new Uint8Array(4));
  await cr.cancel("why");
  const cv = await cp;
  out.cancelled = [cv.done, cv.value];
  try { c2.enqueue(new Uint8Array(1)); } catch (e) { out.enqueueClosed = [e.name, e.code, e.message]; }
  let c3;
  const big = new ReadableStream({ type: "bytes", start(c) { c3 = c; } });
  const bigReader = big.getReader({ mode: "byob" });
  const bp = bigReader.read(new Uint8Array(2));
  try { c3.byobRequest.respond(3); } catch (e) { out.respondRange = [e.name, e.code, e.message]; }
  try { c3.enqueue(new Uint8Array(0)); } catch (e) { out.enqueueEmpty = [e.name, e.code, e.message]; }
  try { c3.enqueue("x"); } catch (e) { out.enqueueString = [e.name, e.code, e.message]; }
  c3.byobRequest.respondWithNewView(new Uint8Array(c3.byobRequest.view.buffer, 0, 1));
  out.newView = Array.from((await bp).value);
  c3.error(new Error("boom"));
  await bigReader.read(new Uint8Array(1)).catch((e) => (out.errored = e.message));
  out.toStringTags = [Object.prototype.toString.call(new ReadableStream({ type: "bytes" }).getReader({ mode: "byob" }))];
return out;
"##;

const BYOB_EXPECTED: &str = r##"{"globals":[true,true,true],"controller":["ReadableByteStreamController",true,null,0],"reader":["ReadableStreamBYOBReader",true,true],"r1":[false,"Uint8Array",[65,65,65,65],10,0],"seenRequest":["ReadableStreamBYOBRequest","Uint8Array",10,0],"r2":[false,"Uint16Array",2,4],"r3":[false,[67,67]],"r4":[true,[]],"r5":[true,"Uint8Array",0,8],"r6":[true,0],"closed":true,"enqueueDetached":0,"desired":-8,"q1":[1,2,3,4],"q2":[5,6,7,8,9,10],"q3":[11,12,13],"q4":[true,0],"auto":[["Uint8Array",[101,200],5],["Uint8Array",[102,200],5]],"plainDefault":["Uint8Array",[7,8],true],"notBytes":["TypeError","ERR_INVALID_ARG_VALUE","The argument 'stream' must be a byte stream. Received ReadableStream { locked: false, state: 'readable', supportsBYOB: false }"],"badMode":["TypeError","ERR_INVALID_ARG_VALUE","The property 'options.mode' is invalid. Received 'nope'"],"zero":["TypeError","ERR_INVALID_STATE","Invalid state: View or Viewed ArrayBuffer is zero-length or detached"],"notView":["TypeError","ERR_INVALID_ARG_TYPE","The \"view\" argument must be an instance of Buffer, TypedArray, or DataView. Received type string ('x')"],"minRange":["RangeError","ERR_OUT_OF_RANGE","The value of \"options.min\" is out of range. It must be <= view.length. Received 5"],"minZero":["TypeError","ERR_INVALID_ARG_VALUE","The property 'options.min' must be greater than 0. Received 0"],"released":["TypeError","ERR_INVALID_STATE","Invalid state: Releasing reader"],"releasedClosed":["TypeError","ERR_INVALID_STATE"],"cancelReason":"why","cancelled":[true,null],"enqueueClosed":["TypeError","ERR_INVALID_STATE","Invalid state: ReadableStream is already closed"],"respondRange":["RangeError","ERR_INVALID_ARG_VALUE","The argument 'bytesWritten' is invalid. Received 3"],"enqueueEmpty":["TypeError","ERR_INVALID_STATE","Invalid state: chunk ArrayBuffer is zero-length or detached"],"enqueueString":["TypeError","ERR_INVALID_ARG_TYPE","The \"buffer\" argument must be an instance of Buffer, TypedArray, or DataView. Received type string ('x')"],"newView":[0],"errored":"boom","toStringTags":["[object ReadableStreamBYOBReader]"]}"##;

#[tokio::test]
async fn byte_streams_and_byob_readers_match_node() {
    let (_dir, root) = tempdir();
    let (runtime, _capture) = runtime(&root);
    let actual = run_cjs_within(&runtime, &root, BYOB_SCRIPT, 30).await;
    assert_same_keys(&actual, &serde_json::from_str(BYOB_EXPECTED).unwrap());
}

#[tokio::test]
async fn file_handle_web_stream_reads_into_byob_views() {
    let (_dir, root) = tempdir();
    let (runtime, _capture) = runtime(&root);
    let script = r#"
const fs = require("fs");
fs.writeFileSync("f.txt", "hello world");
const handle = await fs.promises.open("f.txt");
const reader = handle.readableWebStream().getReader({ mode: "byob" });
const first = await reader.read(new Uint8Array(4));
const second = await reader.read(new Uint8Array(100));
const third = await reader.read(new Uint8Array(4));
await handle.close();
return [reader.constructor.name, Buffer.from(first.value).toString(), Buffer.from(second.value).toString(), third.done, third.value.byteLength];
"#;
    let actual = run_cjs_within(&runtime, &root, script, 30).await;
    assert_eq!(actual, serde_json::json!(["ReadableStreamBYOBReader", "hell", "o world", true, 0]));
}

const DUPLEX_SCRIPT: &str = r##"
const { Duplex, Readable } = require("stream");
const out = {};
const collect = (stream) => new Promise((resolve, reject) => {
  const chunks = [];
  stream.on("data", (c) => chunks.push(typeof c === "string" ? c : Buffer.isBuffer(c) ? ["buf", c.toString()] : c));
  stream.on("end", () => resolve(chunks));
  stream.on("error", reject);
});
  const fromPromise = Duplex.from(Promise.resolve("value"));
  out.promise = [fromPromise instanceof Duplex, fromPromise.readable, fromPromise.writable, fromPromise.readableObjectMode, await collect(fromPromise)];
  out.promiseNull = await collect(Duplex.from(Promise.resolve(null)));
  const rejected = Duplex.from(Promise.reject(new Error("nope")));
  out.promiseRejected = await new Promise((resolve) => rejected.on("error", (e) => resolve([e.message, rejected.destroyed])));

  const seen = [];
  const sink = Duplex.from(async function (source) {
    for await (const chunk of source) seen.push(String(chunk));
  });
  out.asyncFn = [sink.readable, sink.writable, sink.writableObjectMode];
  await new Promise((resolve, reject) => { sink.on("finish", resolve); sink.on("error", reject); sink.write("a"); sink.write("b"); sink.end("c"); });
  out.asyncFnSeen = seen;
  const bad = Duplex.from(async function (source) { for await (const c of source); return 5; });
  out.asyncFnReturn = await new Promise((resolve) => { bad.on("error", (e) => resolve([e.name, e.code, e.message])); bad.end("x"); });

  const webReadable = new ReadableStream({ start(c) { c.enqueue("w1"); c.enqueue("w2"); c.close(); } });
  const fromWeb = Duplex.from(webReadable);
  out.web = [fromWeb.readable, fromWeb.writable, await collect(fromWeb)];
  const written = [];
  const webWritable = new WritableStream({ write(chunk) { written.push(String(chunk)); } });
  const toWeb = Duplex.from(webWritable);
  out.webWritable = [toWeb.readable, toWeb.writable];
  await new Promise((resolve, reject) => { toWeb.on("finish", resolve); toWeb.on("error", reject); toWeb.write("x"); toWeb.end("y"); });
  out.webWritten = written;
  const transform = new TransformStream({ transform(chunk, c) { c.enqueue(String(chunk).toUpperCase()); } });
  const pair = Duplex.from(transform);
  const pairOut = collect(pair);
  pair.write("hi");
  pair.end("there");
  out.webPair = [pair.readable, await pairOut];
  const blob = Duplex.from(new Blob(["blob data"]));
  const blobChunks = await collect(blob);
  out.blob = blobChunks.map((c) => [c.constructor.name, Buffer.from(c).toString()]);
  try { Duplex.from(5); } catch (e) { out.invalid = [e.code, e.message]; }
return out;
"##;

const DUPLEX_EXPECTED: &str = r##"{"promise":[true,true,false,true,["value"]],"promiseNull":[],"promiseRejected":["nope",true],"asyncFn":[false,true,true],"asyncFnSeen":["a","b","c"],"asyncFnReturn":["TypeError","ERR_INVALID_RETURN_VALUE","Expected nully to be returned from the \"body\" function but got type number (5)."],"web":[true,false,[["buf","w1"],["buf","w2"]]],"webWritable":[false,true],"webWritten":["x","y"],"webPair":[true,[["buf","HI"],["buf","THERE"]]],"blob":[["ArrayBuffer","blob data"]],"invalid":["ERR_INVALID_ARG_TYPE","The \"body\" argument must be of type function or an instance of Blob, ReadableStream, WritableStream, Stream, Iterable, AsyncIterable, or Promise or { readable, writable } pair. Received type number (5)"]}"##;

#[tokio::test]
async fn duplex_from_matches_node() {
    let (_dir, root) = tempdir();
    let (runtime, _capture) = runtime(&root);
    let actual = run_cjs_within(&runtime, &root, DUPLEX_SCRIPT, 30).await;
    assert_same_keys(&actual, &serde_json::from_str(DUPLEX_EXPECTED).unwrap());
}
