//! JS ownership and control-flow proof that a handle send failing while it prepares socket copies releases them all:
//! shipped ipc.js on fake natives, with a mock "win32" platform. Not a kernel, WinSock or Node-parity test.

mod common;

use common::*;
use serde_json::{Value, json};

const IPC: &str = include_str!("../src/js/ipc.js");
const FACTORY: &str = include_str!("fixtures/ipc/sender_factory.js");

/// A case that would throw out of `send` or out of a fed NACK reports the thrown code with the registry state at
/// once, so a regression fails on that state instead of waiting.
const HARNESS: &str = r#"
const SOURCE = IPC_SOURCE;
FACTORY
const NACK = { cmd: "NODE_HANDLE_NACK" };
const codeOf = (error) => error.code || error.name;

// The first handle send fails while copying or duplicating; afterwards a plain message still goes out.
async function failedPreparation(plan, message = "A") {
  const c = channel(plan);
  let thrown = null;
  try {
    c.target.send(message, c.socket, () => {});
  } catch (error) {
    thrown = codeOf(error);
  }
  const mid = recorder();
  c.target.send("mid", mid.callback);
  await mid.at(1);
  return { thrown, mid: mid.calls, ...c.summary() };
}

// The first send works; preparing the resend after a NACK fails, which must reach A's callback.
async function failedResend(plan) {
  const c = channel(plan);
  const a = recorder();
  const mid = recorder();
  c.target.send("A", c.socket, a.callback);
  c.target.send("mid", mid.callback);
  await a.at(1);
  try {
    c.feed(NACK);
  } catch (error) {
    return { thrown: codeOf(error), a: a.calls, mid: mid.calls, ...c.summary() };
  }
  await Promise.all([a.at(2), mid.at(1)]);
  return { thrown: null, a: a.calls, mid: mid.calls, ...c.summary() };
}

// toJSON runs once per write, before any copy of that write exists, and may drop the message.
async function serializationOrder() {
  const c = channel({ platform: "win32" });
  const order = [];
  const message = { toJSON: () => (order.push([c.calls.exports, c.calls.clones]), undefined) };
  const a = recorder();
  c.target.send(message, c.socket, a.callback);
  await a.at(1);
  c.feed(NACK);
  await a.at(2);
  c.feed({ cmd: "NODE_HANDLE_ACK" });
  return { order, a: a.calls, ...c.summary() };
}

const circular = {};
circular.self = circular;
return {
  secondExport: await failedPreparation({ exports: ["ok", "EMFILE"] }),
  windowsDuplicate: await failedPreparation({ platform: "win32", dups: ["WSAEINVAL"] }),
  bigint: await failedPreparation({}, { big: 1n }),
  circular: await failedPreparation({}, circular),
  resendClone: await failedResend({ clones: ["EMFILE"] }),
  resendWindowsDuplicate: await failedResend({ platform: "win32", dups: ["ok", "WSAEINVAL"] }),
  serialization: await serializationOrder(),
};
"#;

/// The state every case must end in: channel open, nothing pending or queued, no sender id left open, and every
/// export unmarked, because IPC marks the socket itself.
fn settled(extra: Value) -> Value {
    let mut state = json!({
        "pipeCloses": 0,
        "connected": true,
        "pending": false,
        "queued": null,
        "open": [],
        "unmarkedExports": 0
    });
    state.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
    state
}

/// A send that failed before its socket was shared: no mark, the socket intact, and "mid" sent alone.
fn unshared(thrown: &str, exports: u32) -> Value {
    settled(json!({
        "thrown": thrown, "mid": [null], "lines": ["mid"], "destroyed": 0, "exports": exports, "clones": 0, "marks": 0
    }))
}

/// A resend that failed after a good first write: A's callback gets the error and "mid" follows.
fn resent(thrown: &str, first: Value) -> Value {
    settled(json!({
        "thrown": null, "a": [null, thrown], "mid": [null], "lines": [first, "mid"],
        "destroyed": 1, "exports": 2, "clones": 1, "marks": 1
    }))
}

#[tokio::test(flavor = "multi_thread")]
async fn failed_copy_preparation_releases_every_sender_copy_and_keeps_messages_moving() {
    let (_dir, root) = tempdir();
    let (runtime, capture) = runtime(&root);
    let body = HARNESS.replace("FACTORY", FACTORY).replace("IPC_SOURCE", &serde_json::to_string(IPC).unwrap());
    let actual = run_cjs_within(&runtime, &root, &body, 30).await;
    let expected = json!({
        "secondExport": unshared("EMFILE", 2),
        "windowsDuplicate": unshared("WSAEINVAL", 2),
        "bigint": unshared("TypeError", 0),
        "circular": unshared("TypeError", 0),
        "resendClone": resent("EMFILE", json!({ "handle": "A" })),
        "resendWindowsDuplicate": resent("WSAEINVAL", json!({ "handle": "A", "wsa": "wsa-100" })),
        "serialization": settled(json!({
            "order": [[0, 0], [2, 0]],
            "a": [null, null],
            "lines": [{ "wsa": "wsa-100" }, { "wsa": "wsa-102" }],
            "destroyed": 1, "exports": 2, "clones": 1, "marks": 1
        })),
    });
    for (name, case) in expected.as_object().unwrap() {
        assert_same_keys(&actual[name], case);
    }
    assert!(capture.uncaught.lock().unwrap().is_empty(), "uncaught errors; stderr: {}", capture.stderr());
}
