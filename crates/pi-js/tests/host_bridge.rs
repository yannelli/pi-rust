mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::*;
use pi_js::{HostFunction, HostValue, JsError, JsRef, JsRuntime, UncaughtKind};
use serde_json::json;

fn number(args: &[HostValue], index: usize) -> f64 {
    args.get(index).and_then(HostValue::as_json).and_then(serde_json::Value::as_f64).unwrap_or(0.0)
}

#[tokio::test]
async fn host_functions_return_values_promises_and_errors() {
    let (_dir, root) = tempdir();
    let (runtime, _capture) = runtime(&root);
    runtime.register_host_function(
        "add",
        HostFunction::sync(|args| Ok(json!(number(&args, 0) + number(&args, 1)).into())),
    );
    runtime.register_host_function(
        "double_later",
        HostFunction::asynchronous(|args| async move {
            tokio::time::sleep(Duration::from_millis(5)).await;
            Ok(json!(number(&args, 0) * 2.0).into())
        }),
    );
    runtime.register_host_function(
        "fail",
        HostFunction::asynchronous(|_| async {
            Err(JsError::type_error("host said no").with_code("E_HOST").with_data("detail", json!({ "retry": false })))
        }),
    );
    runtime.register_host_function(
        "inspect",
        HostFunction::sync(|args| {
            let kinds: Vec<&str> = args
                .iter()
                .map(|arg| match arg {
                    HostValue::Undefined => "undefined",
                    HostValue::Json(_) => "json",
                    HostValue::Bytes(_) => "bytes",
                    HostValue::Ref(_) => "ref",
                })
                .collect();
            Ok(json!(kinds).into())
        }),
    );
    runtime.register_host_function("bytes", HostFunction::sync(|_| Ok(HostValue::Bytes(vec![1, 2, 255]))));
    let result = run_cjs(
        &runtime,
        &root,
        r#"
const host = require("pi:host");
const out = {};
out.sum = host.call("add", 2, 3);
out.doubled = await Promise.all([1, 2, 3].map((n) => host.call("double_later", n)));
try { await host.call("fail"); } catch (e) { out.error = [e.name, e.message, e.code, e.detail]; }
try { host.call("missing"); } catch (e) { out.missing = e.code; }
out.kinds = host.call("inspect", undefined, { a: 1 }, new Uint8Array([1]), () => 1, "s");
const bytes = host.call("bytes");
out.bytes = [bytes instanceof Uint8Array, Array.from(bytes)];
out.names = host.names().includes("add") && host.has("fail");
return out;
"#,
    )
    .await;
    assert_eq!(
        result,
        json!({
            "sum": 5,
            "doubled": [2, 4, 6],
            "error": ["TypeError", "host said no", "E_HOST", { "retry": false }],
            "missing": "ERR_PI_HOST_FUNCTION",
            "kinds": ["undefined", "json", "bytes", "ref", "json"],
            "bytes": [true, [1, 2, 255]],
            "names": true,
        })
    );
}

#[tokio::test]
async fn javascript_callbacks_are_callable_from_rust_and_other_threads() {
    let (_dir, root) = tempdir();
    let (runtime, _capture) = runtime(&root);
    let stored: Arc<Mutex<Vec<JsRef>>> = Arc::default();
    let sink = stored.clone();
    runtime.register_host_function(
        "register",
        HostFunction::sync(move |mut args| {
            let callback = args.remove(0);
            sink.lock()
                .unwrap()
                .push(callback.as_js_ref().cloned().ok_or_else(|| JsError::type_error("not a function"))?);
            Ok(HostValue::Undefined)
        }),
    );
    run_cjs(
        &runtime,
        &root,
        r#"
const host = require("pi:host");
host.call("register", (name, count) => `${name}:${count * 2}`);
host.call("register", async (value) => { await new Promise((r) => setTimeout(r, 2)); return { echoed: value }; });
host.call("register", () => { const e = new RangeError("callback broke"); e.code = "E_CB"; throw e; });
host.call("register", function render(width) { return [`width=${width}`, "line 2"]; });
"#,
    )
    .await;
    let callbacks = stored.lock().unwrap().clone();
    assert_eq!(callbacks[0].call(vec!["tool".into(), json!(21).into()]).await.unwrap().into_json(), json!("tool:42"));
    let echoed = callbacks[1].call(vec![HostValue::Json(json!({ "nested": [1, 2] }))]).await.unwrap();
    assert_eq!(echoed.into_json(), json!({ "echoed": { "nested": [1, 2] } }));
    let error = callbacks[2].call(vec![]).await.unwrap_err();
    assert_eq!(
        (error.name.as_str(), error.message.as_str(), error.code.as_deref()),
        ("RangeError", "callback broke", Some("E_CB"))
    );
    assert!(error.stack.unwrap_or_default().contains("case-"), "stack names the module");
    let render = callbacks[3].clone();
    let lines =
        std::thread::spawn(move || render.blocking_call(vec![json!(80).into()]).unwrap().into_json()).join().unwrap();
    assert_eq!(lines, json!(["width=80", "line 2"]));
}

#[tokio::test]
async fn handles_work_from_other_threads_and_scopes() {
    let (_dir, root) = tempdir();
    let (runtime, _capture) = runtime(&root);
    let handle = runtime.handle();
    let from_task = tokio::spawn(async move { handle.eval("6 * 7").await.unwrap().into_json() }).await.unwrap();
    assert_eq!(from_task, json!(42));
    let handle = runtime.handle();
    let blocking = std::thread::spawn(move || {
        handle.blocking_with(|scope| {
            scope.to_json(scope.ctx().eval::<pi_js::rquickjs::Value, _>("({ a: [1, 'x'] })").unwrap())
        })
    })
    .join()
    .unwrap()
    .unwrap();
    assert_eq!(blocking, json!({ "a": [1, "x"] }));
    runtime
        .with(|scope| {
            let function = scope.host_function(HostFunction::sync(|args| {
                Ok(json!(format!("hi {}", args[0].as_str().unwrap_or(""))).into())
            }))?;
            scope.ctx().globals().set("greet", function).map_err(|error| scope.caught(error))
        })
        .await
        .unwrap();
    assert_eq!(runtime.eval("greet('there')").await.unwrap().into_json(), json!("hi there"));
    runtime
        .with(|scope| {
            let work = async {
                tokio::time::sleep(Duration::from_millis(2)).await;
                Ok(7)
            };
            let promise =
                scope.spawn(work, |ctx, value: i32| Ok(pi_js::rquickjs::Value::new_int(ctx.clone(), value * 3)))?;
            scope.ctx().globals().set("spawned", promise).map_err(|error| scope.caught(error))
        })
        .await
        .unwrap();
    assert_eq!(runtime.eval("spawned").await.unwrap().into_json(), json!(21));
    assert_eq!(runtime.eval("Promise.resolve(5).then((n) => n + 1)").await.unwrap().into_json(), json!(6));
}

#[tokio::test]
async fn uncaught_errors_exit_idle_and_interrupt() {
    let (_dir, root) = tempdir();
    let capture = Arc::new(Capture::default());
    let exits: Arc<Mutex<Vec<i32>>> = Arc::default();
    let mut options = options(&root, &capture);
    let exit_sink = exits.clone();
    options.on_exit = Some(Arc::new(move |code| exit_sink.lock().unwrap().push(code)));
    let runtime = JsRuntime::new(options).unwrap();
    runtime.eval("setTimeout(() => { throw new Error('late failure'); }, 1); Promise.reject(new Error('nobody listens')); undefined").await.unwrap();
    runtime.idle().await.unwrap();
    {
        let uncaught = capture.uncaught.lock().unwrap();
        let kinds: Vec<_> = uncaught.iter().map(|(kind, error)| (*kind, error.message.clone())).collect();
        assert!(kinds.contains(&(UncaughtKind::Exception, "late failure".into())), "{kinds:?}");
        assert!(kinds.contains(&(UncaughtKind::Rejection, "nobody listens".into())), "{kinds:?}");
    }
    let handled = runtime
        .eval("new Promise((resolve) => { process.once('uncaughtException', (e) => resolve(e.message)); setTimeout(() => { throw new Error('caught by process'); }, 1); })")
        .await
        .unwrap();
    assert_eq!(handled.into_json(), json!("caught by process"));
    let handle = runtime.handle();
    let stopper = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        handle.interrupt();
    });
    let interrupted = runtime.eval("for (;;) {}").await.unwrap_err();
    assert!(interrupted.message.contains("interrupted"), "{interrupted:?}");
    stopper.await.unwrap();
    assert_eq!(runtime.eval("1 + 1").await.unwrap().into_json(), json!(2));
    let _ = runtime.eval("process.on('exit', (code) => console.log('exiting', code)); process.exit(3)").await;
    assert_eq!(*exits.lock().unwrap(), vec![3]);
    assert!(capture.stdout().contains("exiting 3"));
}

#[tokio::test]
async fn async_host_calls_start_in_the_order_javascript_made_them() {
    let (_dir, root) = tempdir();
    let (runtime, _capture) = runtime(&root);
    let started: Arc<Mutex<Vec<u64>>> = Arc::default();
    let sink = started.clone();
    runtime.register_host_function(
        "start",
        HostFunction::asynchronous(move |args| {
            let sink = sink.clone();
            async move {
                // Synchronous work before the op records its start widens the race window.
                std::thread::sleep(Duration::from_micros(200));
                sink.lock().unwrap().push(number(&args, 0) as u64);
                tokio::task::yield_now().await;
                Ok(HostValue::Undefined)
            }
        }),
    );
    run_cjs(
        &runtime,
        &root,
        r#"
const host = require("pi:host");
for (let round = 0; round < 20; round++) {
	await Promise.all(Array.from({ length: 5 }, (_, n) => host.call("start", round * 5 + n)));
}
"#,
    )
    .await;
    let started = started.lock().unwrap().clone();
    assert_eq!(started, (0..100).collect::<Vec<u64>>());
}
