//! JS ownership and control-flow proof for failed IPC handle writes while the channel stays open: the shipped
//! ipc.js runs on fake natives that only record which socket ids are open. Not a kernel or Node-parity test.
#![cfg(unix)]

mod common;

use common::*;
use serde_json::json;

const IPC: &str = include_str!("../src/js/ipc.js");
const FACTORY: &str = include_str!("fixtures/ipc/sender_factory.js");

/// Each case loads ipc.js through the shared fake-native factory; `writeHandle` follows each case's script.
const HARNESS: &str = r#"
const SOURCE = IPC_SOURCE;
FACTORY
async function failedFirstWrite(step) {
  const c = channel([step]);
  const a = recorder();
  const mid = recorder();
  c.target.send("A", c.socket, a.callback);
  c.target.send("mid", mid.callback);
  await Promise.all([a.at(1), mid.at(1)]);
  return { a: a.calls, mid: mid.calls, ...c.state() };
}

async function failedResend() {
  const c = channel(["ok", "before"]);
  const a = recorder();
  const mid = recorder();
  c.target.send("A", c.socket, a.callback);
  c.target.send("mid", mid.callback);
  await a.at(1);
  c.feed({ cmd: "NODE_HANDLE_NACK" });
  await Promise.all([a.at(2), mid.at(1)]);
  return { a: a.calls, mid: mid.calls, ...c.state() };
}

async function retryLimit() {
  const warnings = [];
  const onWarning = (warning) => warnings.push(warning.name);
  process.on("warning", onWarning);
  try {
    const c = channel([]);
    const a = recorder();
    const mid = recorder();
    c.target.send("A", c.socket, a.callback);
    c.target.send("mid", mid.callback);
    for (let delivered = 1; delivered <= 4; delivered++) {
      await a.at(delivered);
      c.feed({ cmd: "NODE_HANDLE_NACK" });
    }
    await mid.at(1);
    return { a: a.calls, mid: mid.calls, warnings: [...warnings], ...c.state() };
  } finally {
    process.removeListener("warning", onWarning);
  }
}

return {
  before: await failedFirstWrite("before"),
  after: await failedFirstWrite("after"),
  resend: await failedResend(),
  limit: await retryLimit(),
};
"#;

/// What every case leaves: the channel open, nothing pending or queued, no sender id open, the socket destroyed once.
fn settled(extra: serde_json::Value) -> serde_json::Value {
    let mut state = json!({
        "pipeCloses": 0,
        "destroyed": 1,
        "connected": true,
        "pending": null,
        "queue": null,
        "open": []
    });
    state.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
    state
}

#[tokio::test(flavor = "multi_thread")]
async fn failed_handle_writes_release_every_sender_copy_while_the_channel_stays_open() {
    let (_dir, root) = tempdir();
    let (runtime, capture) = runtime(&root);
    let body = HARNESS.replace("FACTORY", FACTORY).replace("IPC_SOURCE", &serde_json::to_string(IPC).unwrap());
    let actual = run_cjs_within(&runtime, &root, &body, 30).await;
    let handle = json!({ "handle": "A" });
    let expected = json!({
        "before": settled(json!({ "a": ["EINJECTED"], "mid": [null], "lines": ["mid"] })),
        "after": settled(json!({ "a": ["EINJECTED"], "mid": [null], "lines": [handle, "mid"] })),
        "resend": settled(json!({ "a": [null, "EINJECTED"], "mid": [null], "lines": [handle, "mid"] })),
        "limit": settled(json!({
            "a": [null, null, null, null],
            "mid": [null],
            "warnings": ["SentHandleNotReceivedWarning"],
            "lines": [handle, handle, handle, handle, "mid"]
        })),
    });
    for (name, case) in expected.as_object().unwrap() {
        assert_same_keys(&actual[name], case);
    }
    assert!(capture.uncaught.lock().unwrap().is_empty(), "uncaught errors; stderr: {}", capture.stderr());
}
