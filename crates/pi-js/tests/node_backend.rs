//! The Node backend: calls, values, host functions, and process lifecycle.

mod common;
mod node_common;

use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::Duration;

use common::*;
use node_common::*;
use pi_js::{HostFunction, HostValue, JsError, UncaughtKind};
use serde_json::json;

#[tokio::test]
async fn evaluates_scripts_and_awaits_promises() {
    let (_dir, root) = tempdir();
    let Some((runtime, _capture)) = node_runtime(&root) else { return };
    assert!(runtime.is_node());
    let value = runtime.eval("1 + 2").await.unwrap();
    assert_eq!(value.as_json(), Some(&json!(3)));
    let value = runtime.eval("Promise.resolve({ version: process.versions.node.split('.')[0] >= 22 })").await.unwrap();
    assert_eq!(value.into_json(), json!({ "version": true }));
    assert!(runtime.eval("undefined").await.unwrap().is_undefined());
    let error = runtime.eval("throw new TypeError('nope')").await.unwrap_err();
    assert_eq!((error.name.as_str(), error.message.as_str()), ("TypeError", "nope"));
    assert!(runtime.with(|_| Ok(())).await.unwrap_err().code.as_deref() == Some("ERR_PI_NODE_BACKEND"));
}

#[tokio::test]
async fn sync_and_async_host_functions() {
    let (_dir, root) = tempdir();
    let Some((runtime, _capture)) = node_runtime(&root) else { return };
    runtime.register_host_function(
        "add",
        HostFunction::sync(|args| {
            let number = |index: usize| args.get(index).and_then(HostValue::as_json).and_then(|v| v.as_f64());
            Ok(json!(number(0).unwrap_or(0.0) + number(1).unwrap_or(0.0)).into())
        }),
    );
    runtime.register_host_function(
        "later",
        HostFunction::asynchronous(|args| async move {
            tokio::time::sleep(Duration::from_millis(5)).await;
            Ok(args.into_iter().next().unwrap_or_default())
        }),
    );
    runtime.register_host_function(
        "fail",
        HostFunction::sync(|_| Err(JsError::type_error("host said no").with_code("E_HOST").with_data("retry", false))),
    );
    let value = runtime
        .eval(
            r#"(async () => {
  const host = (await import("pi:host")).default;
  const out = { sum: host.call("add", 2, 3), later: await host.call("later", "x"), has: host.has("add") };
  try { host.call("fail"); } catch (e) { out.error = [e.name, e.message, e.code, e.retry]; }
  try { host.call("missing"); } catch (e) { out.missing = e.code; }
  out.names = host.names();
  return out;
})()"#,
        )
        .await
        .unwrap();
    assert_eq!(
        value.into_json(),
        json!({
            "sum": 5, "later": "x", "has": true,
            "error": ["TypeError", "host said no", "E_HOST", false],
            "missing": "ERR_PI_HOST_FUNCTION",
            "names": ["add", "fail", "later"],
        })
    );
}

#[tokio::test]
async fn references_call_methods_and_release() {
    let (_dir, root) = tempdir();
    let Some((runtime, _capture)) = node_runtime(&root) else { return };
    runtime
        .eval_module(
            "counter.mjs",
            "export default { count: 1, add(n) { this.count += n; return this.count; }, async later() { return 'done'; } };",
        )
        .await
        .unwrap();
    let counter = runtime.import_default("./counter.mjs").await.unwrap();
    assert_eq!(counter.call_method("add", vec![json!(2).into()]).await.unwrap().into_json(), json!(3));
    assert_eq!(counter.blocking_get("count").unwrap().into_json(), json!(3));
    counter.blocking_set("count", json!(10).into()).unwrap();
    assert_eq!(counter.blocking_call_method("add", vec![json!(1).into()]).unwrap().into_json(), json!(11));
    assert_eq!(counter.call_method("later", Vec::new()).await.unwrap().into_json(), json!("done"));
    assert!(counter.blocking_call_method_opt_ref("missing", Vec::new()).is_err());
    let add = counter.get("add").await.unwrap();
    assert!(add.as_js_ref().is_some());
    assert_eq!(counter.to_json().await.unwrap(), json!({ "count": 11 }));
    let count = r#"globalThis[Symbol.for("pi.node.refCount")]()"#;
    let before = runtime.eval(count).await.unwrap().into_json();
    drop(counter);
    drop(add);
    for _ in 0..200 {
        if runtime.eval(count).await.unwrap().into_json().as_u64() < before.as_u64() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("dropped references were not released");
}

#[tokio::test]
async fn bytes_round_trip() {
    let (_dir, root) = tempdir();
    let Some((runtime, _capture)) = node_runtime(&root) else { return };
    runtime.register_host_function("echo", HostFunction::sync(|args| Ok(args.into_iter().next().unwrap_or_default())));
    let bytes = runtime
        .eval(
            r#"(async () => {
  const host = (await import("pi:host")).default;
  const back = host.call("echo", new Uint8Array([0, 1, 254, 255]));
  return Uint8Array.from([...back, back instanceof Uint8Array ? 1 : 0]);
})()"#,
        )
        .await
        .unwrap();
    assert!(matches!(bytes, HostValue::Bytes(ref data) if data == &[0, 1, 254, 255, 1]));
}

#[tokio::test]
async fn nested_calls_during_a_sync_host_call_complete() {
    let (_dir, root) = tempdir();
    let Some((runtime, _capture)) = node_runtime(&root) else { return };
    let handle = runtime.handle();
    runtime.register_host_function(
        "poke",
        HostFunction::sync(move |args| {
            let callback = args.into_iter().next().and_then(|value| value.as_js_ref().cloned());
            let callback = callback.ok_or_else(|| JsError::new("no callback"))?;
            assert!(handle.on_js_thread());
            assert!(callback.blocking_call(Vec::new()).is_err(), "blocking on the bridge thread must fail");
            std::thread::spawn(move || callback.blocking_call(vec![json!("from rust").into()]));
            Ok(json!("sync done").into())
        }),
    );
    let value = runtime
        .eval(
            r#"(async () => {
  const host = (await import("pi:host")).default;
  let seen;
  const arrived = new Promise((resolve) => { seen = resolve; });
  const result = host.call("poke", (value) => seen(value));
  return [result, await arrived];
})()"#,
        )
        .await
        .unwrap();
    assert_eq!(value.into_json(), json!(["sync done", "from rust"]));
}

#[tokio::test]
async fn output_uncaught_errors_and_exit_reach_callbacks() {
    let (_dir, root) = tempdir();
    let capture = Arc::new(Capture::default());
    let exit_code = Arc::new(AtomicI32::new(-1));
    let mut options = options(&root, &capture);
    let code = exit_code.clone();
    options.on_exit = Some(Arc::new(move |value| code.store(value, Ordering::SeqCst)));
    let Some(runtime) = node_runtime_with(options) else { return };
    runtime.eval("console.log('hello', 1); process.stderr.write('warn\\n'); void 0").await.unwrap();
    runtime.eval("setTimeout(() => { throw new Error('late boom'); }, 1); void 0").await.unwrap();
    runtime.eval("Promise.reject(new RangeError('lost')); void 0").await.unwrap();
    wait_for(|| capture.uncaught.lock().unwrap().len() >= 2).await;
    wait_for(|| capture.stdout().contains("hello 1") && capture.stderr().contains("warn")).await;
    let uncaught = capture.uncaught.lock().unwrap().clone();
    assert!(uncaught.iter().any(|(kind, error)| *kind == UncaughtKind::Exception && error.message == "late boom"));
    assert!(uncaught.iter().any(|(kind, error)| *kind == UncaughtKind::Rejection && error.name == "RangeError"));
    let _ = runtime.eval("process.exit(7)").await;
    assert_eq!(exit_code.load(Ordering::SeqCst), 7);
    wait_for(|| !runtime.is_alive()).await;
    let error = runtime.eval("1").await.unwrap_err();
    assert!(error.message.contains("process.exit(7)"), "{error:?}");
}

#[tokio::test]
async fn idle_waits_for_timers() {
    let (_dir, root) = tempdir();
    let Some((runtime, _capture)) = node_runtime(&root) else { return };
    runtime.eval("setTimeout(() => { globalThis.done = 'yes'; }, 30); void 0").await.unwrap();
    runtime.idle().await.unwrap();
    assert_eq!(runtime.eval("globalThis.done").await.unwrap().into_json(), json!("yes"));
}

#[tokio::test]
async fn a_child_crash_fails_pending_calls() {
    let (_dir, root) = tempdir();
    let Some((runtime, _capture)) = node_runtime(&root) else { return };
    let pending = runtime.eval("new Promise(() => {})");
    let killer = runtime.eval("setTimeout(() => process.kill(process.pid, 'SIGKILL'), 20); void 0");
    let (pending, _) = tokio::join!(pending, killer);
    let error = pending.unwrap_err();
    assert_eq!(error.code.as_deref(), Some("ERR_PI_NODE_EXITED"), "{error:?}");
    assert!(runtime.eval("1").await.is_err());
}

#[tokio::test]
async fn interrupt_ends_a_busy_worker() {
    let (_dir, root) = tempdir();
    let Some((runtime, _capture)) = node_runtime(&root) else { return };
    let busy = runtime.eval("while (true) {}");
    let interrupt = async {
        tokio::time::sleep(Duration::from_millis(50)).await;
        runtime.interrupt();
    };
    let (busy, ()) = tokio::join!(busy, interrupt);
    assert!(busy.unwrap_err().message.contains("interrupted"));
}
