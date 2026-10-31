#![cfg(unix)]

mod common;

use common::*;

const SCRIPT: &str = r#"
const cp = require("node:child_process");
const fs = require("node:fs");
const child = cp.spawn("sh", ["-c", "exit 0"], { stdio: "ignore" });
fs.appendFileSync("stage", "wait for OS exit without dispatching the JS exit event\n");
const waiter = cp.spawnSync("sh", ["-c", 'while kill -0 "$1" 2>/dev/null; do sleep 0.01; done', "wait", String(child.pid)], { timeout: 5000 });
if (waiter.status !== 0) throw new Error("child was not reaped before the waiter timed out");
const before = child.exitCode;
let invalid;
try { child.kill("SIGBOGUS"); } catch (error) { invalid = [error.name, error.code]; }
const result = child.kill();
const killed = child.killed;
fs.appendFileSync("stage", "dispatch queued child exit\n");
await new Promise((resolve) => child.on("close", resolve));
let processError;
try { process.kill(child.pid, 0); } catch (error) { processError = error.code; }
return { before, invalid, result, killed, code: child.exitCode, processError };
"#;

#[tokio::test]
async fn killing_a_reaped_child_before_its_exit_event_returns_false() {
    let (_dir, root) = tempdir();
    let (runtime, capture) = runtime(&root);
    let actual = run_cjs_within(&runtime, &root, SCRIPT, 15).await;
    assert_eq!(
        actual,
        serde_json::json!({
            "before": null,
            "invalid": ["TypeError", "ERR_UNKNOWN_SIGNAL"],
            "result": false,
            "killed": false,
            "code": 0,
            "processError": "ESRCH"
        })
    );
    assert!(capture.uncaught.lock().unwrap().is_empty());
}
