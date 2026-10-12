//! Ported from upstream `packages/codemode/test/sandbox.test.ts`: limits, lifetime, escape hatches.

mod common;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::*;
use pi_codemode::{
    CodemodeCallStatus, CodemodeErrorKind, CodemodeOutputItem, CodemodeSandbox, ExecuteOptions, MAX_OUTPUT_CHARS,
    MAX_OUTPUT_ITEMS, SandboxFunction, SandboxOptions, Timeout,
};
use serde_json::json;
use tokio_util::sync::CancellationToken;

fn with_timeout(timeout: Timeout) -> ExecuteOptions {
    ExecuteOptions { timeout: Some(timeout), ..ExecuteOptions::default() }
}

#[tokio::test]
async fn terminates_a_synchronous_infinite_loop_on_timeout() {
    let started = Instant::now();
    let options = with_timeout(Timeout::After(Duration::from_millis(200)));
    let result = sandbox(vec![]).execute("while (true) {}", options).await.unwrap();
    let error = result.error().unwrap();
    assert_eq!((error.kind, error.message.as_str()), (CodemodeErrorKind::Timeout, "Execution timed out after 200 ms"));
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[tokio::test]
async fn terminates_a_microtask_spinning_loop_on_timeout() {
    let options = with_timeout(Timeout::After(Duration::from_millis(200)));
    let result = sandbox(vec![]).execute("while (true) await null", options).await.unwrap();
    assert_eq!(result.error().unwrap().kind, CodemodeErrorKind::Timeout);
}

#[tokio::test]
async fn runs_without_a_deadline() {
    let wait = SandboxFunction::new("wait", |_, _| async {
        tokio::time::sleep(Duration::from_millis(50)).await;
        Ok(Some(json!("late")))
    });
    let result = sandbox(vec![wait]).execute("return await tools.wait()", with_timeout(Timeout::Never)).await.unwrap();
    assert_eq!(result.value(), Some(&json!("late")));
}

#[tokio::test]
async fn fails_a_script_that_waits_on_a_promise_nothing_can_settle() {
    let sandbox = sandbox(vec![echo()]);
    let code = "await tools.echo(1); await new Promise(() => {}); return 'never'";
    let result = sandbox.execute(code, with_timeout(Timeout::Never)).await.unwrap();
    let error = result.error().unwrap();
    assert_eq!(error.kind, CodemodeErrorKind::Script);
    assert!(error.message.contains("can never settle"));
    // Returning while a call is still pending is not a stall.
    let returned = sandbox.execute("tools.echo(2); return 'early'", with_timeout(Timeout::Never)).await.unwrap();
    assert_eq!(returned.value(), Some(&json!("early")));
}

#[tokio::test]
async fn aborts_via_cancellation_and_cancels_in_flight_calls() {
    let tokens = Arc::new(Mutex::new(Vec::<CancellationToken>::new()));
    let (called_tx, mut called) = tokio::sync::mpsc::unbounded_channel();
    let seen = tokens.clone();
    let hang = SandboxFunction::new("hang", move |_, cancel| {
        seen.lock().unwrap().push(cancel);
        let _ = called_tx.send(());
        futures::future::pending()
    });
    let sandbox = sandbox(vec![hang]);

    let cancelled = CancellationToken::new();
    cancelled.cancel();
    let options = ExecuteOptions { cancel: Some(cancelled), ..ExecuteOptions::default() };
    let result = sandbox.execute("await tools.hang()", options).await.unwrap();
    assert_eq!(result.error().unwrap().kind, CodemodeErrorKind::Aborted);

    let cancel = CancellationToken::new();
    let options = ExecuteOptions { cancel: Some(cancel.clone()), ..ExecuteOptions::default() };
    let (aborted, ()) = tokio::join!(sandbox.execute("await tools.hang(); return 'never'", options), async {
        called.recv().await;
        cancel.cancel();
    });
    let aborted = aborted.unwrap();
    assert_eq!(aborted.error().unwrap().kind, CodemodeErrorKind::Aborted);
    assert_eq!(aborted.calls[0].status, CodemodeCallStatus::Cancelled);
    assert!(tokens.lock().unwrap()[0].is_cancelled());

    let (closed, ()) =
        tokio::join!(sandbox.execute("await tools.hang(); return 'never'", ExecuteOptions::default()), async {
            called.recv().await;
            sandbox.close();
        });
    let error = closed.unwrap().error().cloned().unwrap();
    assert_eq!((error.kind, error.message.as_str()), (CodemodeErrorKind::Aborted, "Sandbox closed"));
    assert!(sandbox.execute("return 1", ExecuteOptions::default()).await.unwrap_err().0.contains("closed"));
}

fn output_chars(output: &[CodemodeOutputItem]) -> usize {
    output
        .iter()
        .map(|item| match item {
            CodemodeOutputItem::Text { text, .. } => text.len(),
            CodemodeOutputItem::Image { data, .. } => data.len(),
        })
        .sum()
}

#[tokio::test]
async fn fails_a_script_whose_output_passes_the_limits_even_if_caught() {
    let sandbox = sandbox(vec![]);
    for print in ["text(s)", "console.log(s)", "image(\"data:image/png;base64,\" + p)"] {
        let code = format!(
            "const s = \"x\".repeat(1 << 20);\nconst p = \"iVBORw0KGgoA\" + \"A\".repeat(1 << 20);\nfor (;;) {{ try {{ {print}; }} catch {{}} }}"
        );
        let result = run(&sandbox, &code).await;
        let error = result.error().unwrap();
        assert_eq!((error.kind, error.name.as_deref()), (CodemodeErrorKind::Script, Some("RangeError")));
        assert!(error.message.contains("script output exceeded"));
        let chars = output_chars(&result.output);
        assert!(chars <= MAX_OUTPUT_CHARS && chars > MAX_OUTPUT_CHARS - (2 << 20), "{print}: {chars}");
    }
    let empty = run(&sandbox, "for (;;) text(\"\");").await;
    assert_eq!(empty.error().unwrap().name.as_deref(), Some("RangeError"));
    assert_eq!(empty.output.len(), MAX_OUTPUT_ITEMS);
}

#[tokio::test]
async fn runs_executions_in_parallel_without_sharing_state() {
    let sandbox = sandbox(vec![]);
    let (a, b, c) = tokio::join!(
        attempt(&sandbox, "globalThis.shared = 'a'; await null; return globalThis.shared"),
        attempt(&sandbox, "globalThis.shared = 'b'; await null; return globalThis.shared"),
        attempt(&sandbox, "return typeof globalThis.shared"),
    );
    assert_eq!([a, b, c], [json!("a"), json!("b"), json!("undefined")]);
}

#[tokio::test]
async fn turns_deep_recursion_into_a_catchable_range_error() {
    let code = "let depth = 0;\nfunction dive() { depth++; dive(); }\ntry { dive(); } catch (error) { return [error.name, depth > 1000]; }";
    assert_eq!(attempt(&sandbox(vec![]), code).await, json!(["RangeError", true]));
}

#[tokio::test]
async fn limits_memory_so_runaway_allocations_fail_inside_the_script() {
    let sandbox = CodemodeSandbox::new(SandboxOptions {
        memory_limit_bytes: Some(256 * 1024 * 1024),
        timeout: Timeout::After(Duration::from_secs(30)),
        ..SandboxOptions::default()
    })
    .unwrap();
    let code = "let a = [];\ntry { while (true) a.push(\"x\".repeat(1 << 20) + a.length); } catch (error) { const n = a.length; a = null; return { n, error: String(error) }; }";
    let value = attempt(&sandbox, code).await;
    assert!(value["error"].as_str().unwrap().contains("out of memory"), "{value}");
    // Each entry holds at least 1 MiB, so the limit stops the script well before 4 GiB.
    assert!(value["n"].as_u64().unwrap() < 512);
}

#[tokio::test]
async fn replaces_lone_surrogates_in_text_output() {
    let result = run(&sandbox(vec![]), "text('a\\ud800b'); return '\\udc00';").await;
    assert_eq!(result.output, [CodemodeOutputItem::Text { text: "a\u{fffd}b".into(), console: false }]);
    assert_eq!(result.value(), Some(&json!("\u{fffd}")));
}

#[tokio::test]
async fn has_no_host_globals() {
    let code = "return [typeof process, typeof require, typeof module, typeof setTimeout, typeof fetch, typeof WebAssembly, typeof std, typeof os, typeof globalThis.constructor];";
    let mut expected = vec![json!("undefined"); 8];
    expected.push(json!("function"));
    assert_eq!(attempt(&sandbox(vec![]), code).await, json!(expected));
}

#[tokio::test]
async fn keeps_eval_and_function_inside_the_vm() {
    let code = r#"return [
        eval("typeof process"),
        new Function("return typeof process")(),
        tools.echo.constructor("return typeof require")(),
        (async () => {}).constructor("return typeof setTimeout")() instanceof Promise,
    ];"#;
    assert_eq!(attempt(&sandbox(vec![echo()]), code).await, json!(["undefined", "undefined", "undefined", true]));
}

#[tokio::test]
async fn rejects_dynamic_import() {
    let code =
        "try { await import(\"node:fs\"); return \"imported\"; } catch (error) { return error.constructor.name; }";
    let result = run(&sandbox(vec![]), code).await;
    assert_ne!(result.value(), Some(&json!("imported")));
}

#[tokio::test]
async fn ignores_patches_to_built_ins() {
    let code = r#"
        Array.prototype.toJSON = () => null;
        Object.prototype.toJSON = () => 5;
        Promise.prototype.then = () => {};
        Map.prototype.get = () => undefined;
        globalThis.JSON = { stringify: () => "x", parse: () => "x" };
        store("k", [1]);
        return [await tools.echo([2]), JSON.stringify({ a: 1 })];
    "#;
    let result = run(&sandbox(vec![echo()]), code).await;
    let success = result.outcome.unwrap();
    assert_eq!(success.value, Some(json!([[2], "{\"a\":1}"])));
    assert_eq!(success.store_writes.set.get("k"), Some(&json!([1])));
}

#[tokio::test]
async fn freezes_intrinsics_reachable_only_from_instances() {
    let code = r#"return [
        Object.getPrototypeOf(function* () {}).prototype,
        Object.getPrototypeOf(async function () {}),
        Object.getPrototypeOf(Int8Array).prototype,
        Object.getPrototypeOf([][Symbol.iterator]()),
        Object.getPrototypeOf(Object.getPrototypeOf([][Symbol.iterator]())),
        Object.getPrototypeOf(new Map()[Symbol.iterator]()),
        Object.getPrototypeOf(/a/[Symbol.matchAll]("")),
    ].every((object) => Object.isFrozen(object));"#;
    assert_eq!(attempt(&sandbox(vec![]), code).await, json!(true));
}

#[tokio::test]
async fn lets_instances_override_properties_of_frozen_prototypes() {
    let code = r#"
        const object = {};
        object.toString = () => "custom";
        function Legacy() {}
        Legacy.prototype = Object.create(Error.prototype);
        Legacy.prototype.constructor = Legacy;
        const bare = new Error();
        bare.message = "set later";
        class MyError extends Error {
            constructor(message) {
                super(message);
                this.name = "MyError";
            }
        }
        let patched = "silent";
        try { Error.prototype.name = "Patched"; } catch (error) { patched = error.constructor.name; }
        return [String(object), new Legacy().constructor === Legacy, bare.message, new MyError("x").name, Error.prototype.name, patched];
    "#;
    assert_eq!(
        attempt(&sandbox(vec![]), code).await,
        json!(["custom", true, "set later", "MyError", "Error", "TypeError"])
    );
}

#[tokio::test]
async fn keeps_tools_and_console_frozen() {
    let code = r#"
        try { tools.echo = () => 'nope'; } catch {}
        try { tools.extra = () => 'nope'; } catch {}
        try { globalThis.tools = null; } catch {}
        return ["extra" in tools, await tools.echo('still')];
    "#;
    assert_eq!(attempt(&sandbox(vec![echo()]), code).await, json!([false, "still"]));
}
