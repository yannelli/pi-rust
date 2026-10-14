//! node:async_hooks and node:domain: AsyncLocalStorage across awaits, timers, ticks, and
//! callbacks; AsyncResource; EventEmitterAsyncResource; and domain error routing.

mod common;

use common::*;
use serde_json::json;

async fn run(body: &str) -> serde_json::Value {
    let (_dir, root) = tempdir();
    let (runtime, _capture) = runtime(&root);
    run_cjs(&runtime, &root, body).await
}

#[tokio::test]
async fn async_local_storage_follows_async_work() {
    let actual = run(r#"
const { AsyncLocalStorage, AsyncResource, executionAsyncId, triggerAsyncId, createHook } = require("async_hooks");
const als = new AsyncLocalStorage();
const log = [];
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
async function work(tag) {
  log.push(`${tag} start ${als.getStore()}`);
  await sleep(2);
  log.push(`${tag} sleep ${als.getStore()}`);
  await null;
  log.push(`${tag} null ${als.getStore()}`);
  try { await Promise.reject(new Error("x")); } catch { log.push(`${tag} catch ${als.getStore()}`); }
  await Promise.all([sleep(1), sleep(2)]);
  return als.getStore();
}
const done = [];
als.run("A", () => {
  done.push(new Promise((resolve) => setTimeout(() => resolve(`timeout ${als.getStore()}`), 1)));
  done.push(new Promise((resolve) => setImmediate(() => resolve(`immediate ${als.getStore()}`))));
  done.push(new Promise((resolve) => process.nextTick(() => resolve(`tick ${als.getStore()}`))));
  done.push(new Promise((resolve) => queueMicrotask(() => resolve(`microtask ${als.getStore()}`))));
  done.push(Promise.resolve().then(() => `then ${als.getStore()}`));
  done.push(new Promise((resolve) => require("fs").stat(__filename, () => resolve(`fs ${als.getStore()}`))));
});
const results = await Promise.all([als.run("B", () => work("B")), als.run("C", () => work("C"))]);
const outside = als.getStore();
const callbacks = await Promise.all(done);
const snapshot = als.run("S", () => AsyncLocalStorage.snapshot());
const resource = als.run("R", () => new AsyncResource("X"));
const bound = als.run("bound", () => AsyncLocalStorage.bind(() => als.getStore()));
const events = [];
const hook = createHook({ init: (id, type) => events.push(["init", type]), before: () => events.push("before"), after: () => events.push("after") }).enable();
const hooked = new AsyncResource("H");
const ids = hooked.runInAsyncScope(() => [executionAsyncId() === hooked.asyncId(), triggerAsyncId() === hooked.triggerAsyncId()]);
hook.disable();
const other = new AsyncLocalStorage({ defaultValue: "dflt", name: "named" });
als.enterWith("entered");
const entered = als.getStore();
als.disable();
return {
  log: log.sort(), results, outside, callbacks,
  snapshot: snapshot(() => als.getStore()),
  resource: resource.runInAsyncScope(() => als.getStore()),
  bound: bound(),
  exit: als.run("Q", () => als.exit(() => als.getStore())),
  nested: als.run("outer", () => [als.run("inner", () => als.getStore()), als.getStore()]),
  events, ids, defaults: [other.getStore(), other.name], entered, disabled: als.getStore(),
};
"#)
    .await;
    let mut expected = Vec::new();
    for tag in ["B", "C"] {
        for step in ["catch", "null", "sleep", "start"] {
            expected.push(format!("{tag} {step} {tag}"));
        }
    }
    assert_eq!(actual["log"], json!(expected));
    assert_eq!(actual["results"], json!(["B", "C"]));
    assert_eq!(actual["outside"], json!(null));
    assert_eq!(actual["callbacks"], json!(["timeout A", "immediate A", "tick A", "microtask A", "then A", "fs A"]));
    assert_eq!(actual["snapshot"], "S");
    assert_eq!(actual["resource"], "R");
    assert_eq!(actual["bound"], "bound");
    assert_eq!(actual["exit"], json!(null));
    assert_eq!(actual["nested"], json!(["inner", "outer"]));
    assert_eq!(actual["events"], json!([["init", "H"], "before", "after"]));
    assert_eq!(actual["ids"], json!([true, true]));
    assert_eq!(actual["defaults"], json!(["dflt", "named"]));
    assert_eq!(actual["entered"], "entered");
    assert_eq!(actual["disabled"], json!(null));
}

#[tokio::test]
async fn emitter_resources_channels_and_domains_use_async_context() {
    let actual = run(r#"
const { AsyncLocalStorage } = require("async_hooks");
const { EventEmitterAsyncResource, EventEmitter } = require("events");
const dc = require("diagnostics_channel");
const domain = require("domain");
const als = new AsyncLocalStorage();
const emitter = als.run(42, () => new EventEmitterAsyncResource({ name: "R" }));
let seen;
emitter.on("x", () => { seen = als.getStore(); });
emitter.emit("x");
const channel = dc.channel("pi.test");
channel.bindStore(als, (data) => data.id);
const stored = await channel.runStores({ id: 7 }, async () => { await null; return als.getStore(); });
const d = domain.create();
const caught = await new Promise((resolve) => {
  d.on("error", (e) => resolve([e.message, e.domainThrown, e.domain === d]));
  d.run(() => setTimeout(() => { throw new Error("boom"); }, 1));
});
const emitted = await new Promise((resolve) => {
  const d2 = domain.create();
  d2.on("error", (e) => resolve([e.message, e.domainThrown, e.domainEmitter instanceof EventEmitter]));
  d2.run(() => { const ee = new EventEmitter(); setTimeout(() => ee.emit("error", new Error("ee")), 1); });
});
const intercepted = [];
const d3 = domain.create();
d3.on("error", (e) => intercepted.push(e.message));
d3.intercept((v) => intercepted.push(v))(null, 5);
d3.intercept(() => {})(new Error("cb error"));
const active = d3.run(() => [process.domain === d3, domain.active === d3]);
const tsFile = require("path").join(__dirname, "context.ts");
require("fs").writeFileSync(tsFile, "export async function f(s: any): Promise<string> { await new Promise((r) => setTimeout(r, 1)); return s.getStore() as string; }");
const ts = await als.run("ts", () => require(tsFile).f(als));
return { ts, seen, stored, caught, emitted, intercepted, active, after: process.domain === undefined && domain.active === undefined };
"#)
    .await;
    assert_eq!(actual["ts"], "ts");
    assert_eq!(actual["seen"], 42);
    assert_eq!(actual["stored"], 7);
    assert_eq!(actual["caught"], json!(["boom", true, true]));
    assert_eq!(actual["emitted"], json!(["ee", false, true]));
    assert_eq!(actual["intercepted"], json!([5, "cb error"]));
    assert_eq!(actual["active"], json!([true, true]));
    assert_eq!(actual["after"], true);
}
