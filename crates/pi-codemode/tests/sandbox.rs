//! Ported from upstream `packages/codemode/test/sandbox.test.ts`: execution, tools, store, globals.

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::*;
use pi_codemode::{CodemodeCallStatus, CodemodeErrorKind, CodemodeOutputItem, ExecuteOptions, SandboxFunction};
use serde_json::{Value, json};

fn text(text: &str) -> CodemodeOutputItem {
    CodemodeOutputItem::Text { text: text.into(), console: false }
}

fn image(data: &str, mime_type: &str) -> CodemodeOutputItem {
    CodemodeOutputItem::Image { data: data.into(), mime_type: mime_type.into() }
}

#[tokio::test]
async fn returns_the_value_after_a_json_round_trip() {
    let sandbox = sandbox(vec![]);
    let result = run(&sandbox, "return { a: 1, b: [true, 'x'] }").await;
    assert_eq!(result.value(), Some(&json!({ "a": 1, "b": [true, "x"] })));
    assert!(result.output.is_empty() && result.calls.is_empty());
    assert_eq!(run(&sandbox, "return 'plain'").await.value(), Some(&json!("plain")));
    let empty = run(&sandbox, "").await;
    assert!(empty.is_ok());
    assert_eq!(empty.value(), None);
}

#[tokio::test]
async fn supports_top_level_await() {
    let sandbox = sandbox(vec![]);
    let result = run(&sandbox, "const x = await Promise.resolve(41); return x + 1").await;
    assert_eq!(result.value(), Some(&json!(42)));
}

#[tokio::test]
async fn collects_text_image_and_console_output_in_order() {
    let sandbox = sandbox(vec![]);
    let code = format!(
        r#"
        console.log("hello", 1, {{ a: 1 }});
        text({{ json: true }});
        text(undefined);
        text(7);
        image("data:image/png;base64,{PNG}");
        image({{ image_url: "data:image/jpeg;base64,{JPEG}" }});
        image({{ type: "image", data: "{GIF}", mimeType: "image/gif" }});
        image("data:image/png;base64,{WEBP}");
        image({{ type: "image", data: "{PNG}" }});
        console.error(new Error("bad"));
        return null;
        "#
    );
    let result = run(&sandbox, &code).await;
    assert!(result.is_ok());
    let (last, items) = result.output.split_last().unwrap();
    assert_eq!(
        items,
        [
            CodemodeOutputItem::Text { text: r#"hello 1 {"a":1}"#.into(), console: true },
            text(r#"{"json":true}"#),
            text("undefined"),
            text("7"),
            image(PNG, "image/png"),
            image(JPEG, "image/jpeg"),
            image(GIF, "image/gif"),
            // The MIME type comes from the data, not from the declared type.
            image(WEBP, "image/webp"),
            image(PNG, "image/png"),
        ]
    );
    assert!(matches!(last, CodemodeOutputItem::Text { text, console: true } if text.starts_with("Error: bad")));
}

#[tokio::test]
async fn rejects_invalid_text_and_image_arguments() {
    let sandbox = sandbox(vec![]);
    let result = run(
        &sandbox,
        r#"
        const errors = [];
        const circular = {};
        circular.self = circular;
        for (const run of [
            () => text(circular),
            () => image(""),
            () => image("https://example.com/a.png"),
            () => image("data:image/png,raw"),
            () => image({ type: "text", text: "x" }),
            () => image({ type: "image", data: "" }),
            () => image(42),
            () => image("data:image/png;base64,AAAA!"),
            () => image("data:image/png;base64,AAAAA"),
            () => image("data:image/png;base64,AA=A"),
            () => image("data:image/png;base64,"),
            () => image("data:image/png;base64,AAAA\n[Output truncated]"),
            () => image({ type: "image", data: "AAAA!", mimeType: "image/png" }),
            () => image("data:image/png;base64,AAAA"),
            () => image("data:image/png;base64,QUJD"),
            () => image("data:image/jpeg;base64,/9j/9w=="),
        ]) {
            try { run(); errors.push("no error"); } catch (error) { errors.push(error.name + ": " + error.message); }
        }
        return errors;
        "#,
    )
    .await;
    assert!(result.output.is_empty());
    let errors: Vec<String> = serde_json::from_value(result.value().cloned().unwrap()).unwrap();
    assert!(errors[0].starts_with("TypeError: ") && errors[0].contains("circular"), "{}", errors[0]);
    let mut expected = vec![
        r#"TypeError: image expects a non-empty image URL string, an object with image_url, or an image block { type: "image", data, mimeType }"#,
        "TypeError: remote image URLs are not supported in tool outputs. Pass a base64 data URI instead",
        "TypeError: invalid image output. Pass a base64 data URI instead",
        "TypeError: image only accepts image blocks, got \"text\"",
        "TypeError: image expected image block data",
        r#"TypeError: image expects a non-empty image URL string, an object with image_url, or an image block { type: "image", data, mimeType }"#,
    ];
    expected
        .extend(["TypeError: invalid image output. The image data is not valid base64 (truncated or corrupted?)"; 6]);
    expected.extend(["TypeError: invalid image output. The image data is not a PNG, JPEG, GIF, or WebP image"; 3]);
    assert_eq!(errors[1..], expected);
}

#[tokio::test]
async fn accepts_wrapped_and_large_base64_image_data() {
    let sandbox = sandbox(vec![]);
    let large = format!("iVBORw0KGgoA{}", "QUJD".repeat(256 * 1024));
    let code =
        format!("image(\"data:image/png;base64,iVBORw0K\\r\\nGgo=\\n\");\nimage(\"data:image/png;base64,{large}\");");
    let result = run(&sandbox, &code).await;
    assert!(result.is_ok());
    assert_eq!(result.output, [image(PNG, "image/png"), image(&large, "image/png")]);
}

#[tokio::test]
async fn ends_the_script_on_exit_keeping_output_and_store_writes() {
    let sandbox = sandbox(vec![echo()]);
    let code = r#"
        text("before");
        store("k", 1);
        await tools.echo(1);
        try { exit(); } catch {}
        text("after");
        return "unreachable";
    "#;
    let result = run(&sandbox, code).await;
    let success = result.outcome.as_ref().unwrap();
    assert_eq!(success.value, None);
    assert_eq!(result.output, [text("before")]);
    assert_eq!(Value::Object(success.store_writes.set.clone()), json!({ "k": 1 }));
    assert!(success.store_writes.delete.is_empty());
}

#[tokio::test]
async fn keeps_output_produced_before_a_failure() {
    let result = run(&sandbox(vec![]), "text(\"partial\");\nthrow new Error(\"boom\")").await;
    assert!(!result.is_ok());
    assert_eq!(result.output, [text("partial")]);
}

#[tokio::test]
async fn reports_syntax_and_thrown_errors_with_line_numbers() {
    let sandbox = sandbox(vec![]);
    let syntax = run(&sandbox, "const a = 1;\nconst b = ;\nreturn a").await;
    let error = syntax.error().unwrap();
    assert_eq!((error.kind, error.name.as_deref()), (CodemodeErrorKind::Script, Some("SyntaxError")));
    assert!(error.stack.as_deref().unwrap().contains("codemode.js:2"), "{error:?}");

    let thrown = run(&sandbox, "const a = 1;\nthrow new TypeError('boom ' + a)").await;
    let error = thrown.error().unwrap();
    assert_eq!((error.name.as_deref(), error.message.as_str()), (Some("TypeError"), "boom 1"));
    assert!(error.stack.as_deref().unwrap().contains("codemode.js:2"));
}

#[tokio::test]
async fn formats_stacks_like_v8_without_prelude_frames() {
    let result = run(&sandbox(vec![]), "console.log(new Error('inner'));\nthrow new RangeError('outer')").await;
    let stack = result.error().unwrap().stack.clone().unwrap();
    assert!(stack.starts_with("RangeError: outer\n    at "), "{stack}");
    assert!(stack.lines().nth(1).unwrap().contains("codemode.js:2"));
    assert!(!stack.contains("codemode-prelude.js"));
    let CodemodeOutputItem::Text { text, .. } = &result.output[0] else { panic!("text expected") };
    assert!(text.starts_with("Error: inner\n    at ") && text.lines().nth(1).unwrap().contains("codemode.js:1"));
    assert!(!format!("{:?}", result.output).contains("codemode-prelude.js"));
}

#[tokio::test]
async fn reports_non_error_throws_and_unserializable_returns() {
    let sandbox = sandbox(vec![]);
    let thrown = run(&sandbox, "throw { code: 7 }").await;
    assert_eq!(thrown.error().unwrap().message, r#"{"code":7}"#);
    let bigint = run(&sandbox, "return 10n").await;
    assert_eq!(bigint.error().unwrap().name.as_deref(), Some("TypeError"));
    let renamed = run(&sandbox, "const error = new Error('x'); error.message = 42; throw error;").await;
    let error = renamed.error().unwrap();
    assert_eq!(
        (error.kind, error.name.as_deref(), error.message.as_str()),
        (CodemodeErrorKind::Script, Some("Error"), "42")
    );
}

#[tokio::test]
async fn exposes_tools_as_async_functions_and_records_calls() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    let add = SandboxFunction::new("add", move |args, _| {
        let log = log.clone();
        async move {
            let args = args.unwrap_or_default();
            log.lock().unwrap().push(args.clone());
            Ok(Some(json!({ "sum": args["a"].as_i64().unwrap() + args["b"].as_i64().unwrap() })))
        }
    });
    let code = "const first = await tools.add({ a: 1, b: 2 });\nconst second = await tools.add({ a: first.sum, b: 10 });\nreturn second.sum;";
    let result = run(&sandbox(vec![add]), code).await;
    assert_eq!(result.value(), Some(&json!(13)));
    assert_eq!(*seen.lock().unwrap(), [json!({ "a": 1, "b": 2 }), json!({ "a": 3, "b": 10 })]);
    let statuses: Vec<_> = result.calls.iter().map(|call| (call.name.as_str(), call.status)).collect();
    assert_eq!(statuses, [("add", CodemodeCallStatus::Ok), ("add", CodemodeCallStatus::Ok)]);
    assert!(result.calls.iter().all(|call| call.duration_ms >= 0.0));
}

#[tokio::test]
async fn runs_concurrent_calls_and_lists_tool_names() {
    let delay = SandboxFunction::new("delay", |args, _| async move {
        tokio::time::sleep(Duration::from_millis(20)).await;
        Ok(args)
    });
    let code = "const [a, b, c] = await Promise.all([tools.delay(1), tools.delay(2), tools.echo(3)]);\nreturn { values: [a, b, c], names: Object.keys(tools) };";
    let result = run(&sandbox(vec![echo(), delay]), code).await;
    assert_eq!(result.value(), Some(&json!({ "values": [1, 2, 3], "names": ["echo", "delay"] })));
}

#[tokio::test]
async fn exposes_normalized_identifiers_and_lists_them_in_all_tools() {
    let sandbox = sandbox(vec![
        constant("my-tool", json!("dash")).with_description("Dashes"),
        constant("my_tool", json!("underscore")).with_description("Shadowed"),
        constant("mcp__docs__search", json!("mcp")),
    ]);
    let code = "try { ALL_TOOLS.push({}); } catch {}\nreturn { all: ALL_TOOLS, calls: [await tools.my_tool(), await tools[\"my-tool\"](), await tools.mcp__docs__search()] };";
    let value = attempt(&sandbox, code).await;
    assert_eq!(
        value,
        json!({
            "all": [{ "name": "my_tool", "description": "Dashes" }, { "name": "mcp__docs__search", "description": "" }],
            "calls": ["dash", "dash", "mcp"],
        })
    );
}

#[tokio::test]
async fn passes_undefined_arguments_and_results_through() {
    let noop = SandboxFunction::new("noop", |args, _| async move { Ok(args) });
    assert_eq!(
        attempt(&sandbox(vec![noop]), "return [await tools.noop(), await tools.noop(null)]").await,
        json!([null, null])
    );
}

#[tokio::test]
async fn turns_tool_errors_into_catchable_errors() {
    let fail = SandboxFunction::new("fail", |_, _| async { Err("tool exploded".to_string()) });
    let code = "try { await tools.fail(); return 'no error'; } catch (error) { return { isError: error instanceof Error, message: error.message }; }";
    let result = run(&sandbox(vec![fail]), code).await;
    assert_eq!(result.value(), Some(&json!({ "isError": true, "message": "tool exploded" })));
    assert_eq!(result.calls[0].status, CodemodeCallStatus::Error);
}

#[tokio::test]
async fn rejects_calls_to_unknown_tools() {
    let result = run(&sandbox(vec![]), "return await tools.missing()").await;
    let error = result.error().unwrap();
    assert_eq!((error.kind, error.name.as_deref()), (CodemodeErrorKind::Script, Some("TypeError")));
}

#[tokio::test]
async fn cancels_unawaited_calls_when_the_script_returns() {
    let seen = Arc::new(Mutex::new(None));
    let token_slot = seen.clone();
    let slow = SandboxFunction::new("slow", move |_, cancel| {
        *token_slot.lock().unwrap() = Some(cancel.clone());
        async move {
            cancel.cancelled().await;
            Err("aborted".to_string())
        }
    });
    let result = run(&sandbox(vec![slow]), "tools.slow(); return 'early'").await;
    assert_eq!(result.value(), Some(&json!("early")));
    assert_eq!(result.calls[0].status, CodemodeCallStatus::Cancelled);
    assert!(seen.lock().unwrap().as_ref().unwrap().is_cancelled());
}

#[tokio::test]
async fn names_close_matches_for_missing_tools() {
    let sandbox = sandbox(vec![echo(), constant("web-search", json!(""))]);
    assert_eq!(
        attempt(&sandbox, "return tools.Echo;").await,
        json!(
            "tools.Echo does not exist. Did you mean tools.echo? ALL_TOOLS lists every tool; searchTools(query) finds tools by topic. Check for a member with \"Echo\" in tools."
        )
    );
    assert!(
        attempt(&sandbox, "return tools.websearch;").await.as_str().unwrap().contains("Did you mean tools.web_search?")
    );
    assert!(
        attempt(&sandbox, "return tools.nothing;").await.as_str().unwrap().contains("Available: echo, web_search.")
    );
    let checks = "return ['echo' in tools, 'nothing' in tools, String(tools.toString), JSON.stringify(tools)];";
    assert_eq!(attempt(&sandbox, checks).await, json!([true, false, "undefined", "{}"]));
}

#[tokio::test]
async fn registers_and_unregisters_between_executions() {
    let mut sandbox = sandbox(vec![]);
    sandbox.register_tool(echo()).unwrap();
    assert!(sandbox.register_tool(echo()).unwrap_err().0.contains("already registered"));
    assert_eq!(sandbox.tools().iter().map(|tool| tool.name()).collect::<Vec<_>>(), ["echo"]);
    assert_eq!(attempt(&sandbox, "return await tools.echo('a')").await, json!("a"));
    assert!(sandbox.unregister_tool("echo"));
    assert_eq!(attempt(&sandbox, "return 'echo' in tools").await, json!(false));
}

fn store_options(store: Value) -> ExecuteOptions {
    ExecuteOptions { store: store.as_object().cloned().unwrap(), ..ExecuteOptions::default() }
}

#[tokio::test]
async fn reads_the_store_snapshot_and_reports_writes() {
    let code = r#"
        const seen = load("counter");
        store("counter", seen + 1);
        store("list", [1, { a: null }]);
        store("old", undefined);
        return [seen, load("counter"), load("missing"), load("old")];
    "#;
    let result = sandbox(vec![]).execute(code, store_options(json!({ "counter": 41, "old": "x" }))).await.unwrap();
    let success = result.outcome.unwrap();
    // undefined array elements become null in the JSON round trip of the return value.
    assert_eq!(success.value, Some(json!([41, 42, null, null])));
    assert_eq!(Value::Object(success.store_writes.set), json!({ "counter": 42, "list": [1, { "a": null }] }));
    assert_eq!(success.store_writes.delete, ["old"]);
}

#[tokio::test]
async fn returns_copies_from_load() {
    let code = "const value = load(\"obj\"); value.a = 2; const kept = { b: 1 }; store(\"kept\", kept); kept.b = 2;\nreturn [load(\"obj\").a, load(\"kept\").b];";
    let result = sandbox(vec![]).execute(code, store_options(json!({ "obj": { "a": 1 } }))).await.unwrap();
    let success = result.outcome.unwrap();
    assert_eq!(success.value, Some(json!([1, 1])));
    assert_eq!(Value::Object(success.store_writes.set), json!({ "kept": { "b": 1 } }));
}

#[tokio::test]
async fn rejects_invalid_keys_values_and_oversized_writes() {
    let code = r#"
        const attempt = (fn) => { try { fn(); return "ok"; } catch (error) { return error.name; } };
        return [
            attempt(() => store(1, "x")),
            attempt(() => load({})),
            attempt(() => store("fn", () => 1)),
            attempt(() => store("big", "x".repeat(300 * 1024))),
            attempt(() => { for (let i = 0; i < 8; i++) store("k" + i, "x".repeat(200 * 1024)); }),
        ];
    "#;
    assert_eq!(
        attempt(&sandbox(vec![]), code).await,
        json!(["TypeError", "TypeError", "TypeError", "RangeError", "RangeError"])
    );
    let oversized = run(&sandbox(vec![]), "store(\"img\", \"x\".repeat(300 * 1024));").await;
    let message = &oversized.error().unwrap().message;
    assert!(message.contains("store(\"img\") value has 307202 characters of JSON"), "{message}");
    assert!(message.contains("Show images with image()"));
}

#[tokio::test]
async fn exposes_globals_without_recording_them_as_calls() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    let attach = SandboxFunction::new("attach", move |args, _| {
        log.lock().unwrap().push(args.unwrap_or_default());
        async { Ok(None) }
    });
    let sandbox = with_globals(vec![echo()], vec![attach]);
    let code = "await attach({ ref: 1 });\nattach(\"not awaited\");\nreturn [typeof attach, typeof globalThis.attach, await tools.echo(2)];";
    let result = run(&sandbox, code).await;
    assert_eq!(result.value(), Some(&json!(["function", "function", 2])));
    assert_eq!(result.calls.iter().map(|call| call.name.as_str()).collect::<Vec<_>>(), ["echo"]);
    // Messages are handled in order, so an unawaited global still runs before the script settles.
    assert_eq!(*seen.lock().unwrap(), [json!({ "ref": 1 }), json!("not awaited")]);
}

#[tokio::test]
async fn groups_namespaced_globals_and_spreads_arguments() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    let list = SandboxFunction::new("models.list", move |args, _| {
        log.lock().unwrap().push(args.unwrap_or_default());
        async { Ok(None) }
    })
    .with_spread();
    let first = SandboxFunction::new("models.first", |args, _| async move { Ok(args) });
    let sandbox = with_globals(vec![], vec![list, first]);
    let code = "await models.list(\"classifier\", undefined, 3);\nawait models.list();\ntry { models.extra = 1; } catch {}\nreturn [Object.keys(models), await models.first(\"a\", \"ignored\"), \"extra\" in models];";
    assert_eq!(attempt(&sandbox, code).await, json!([["list", "first"], "a", false]));
    // undefined array elements become null in the JSON round trip.
    assert_eq!(*seen.lock().unwrap(), [json!(["classifier", null, 3]), json!([])]);
}

#[tokio::test]
async fn names_namespace_members_that_do_not_exist() {
    let noop = |name: &str| SandboxFunction::new(name, |_, _| async { Ok(None) });
    let sandbox = with_globals(vec![], vec![noop("models.classify"), noop("models.generateImages")]);
    assert_eq!(
        attempt(&sandbox, "await models.generateImage();").await,
        json!(
            "models.generateImage does not exist. Did you mean models.generateImages? Check for a member with \"generateImage\" in models."
        )
    );
}

#[test]
fn rejects_invalid_and_reserved_global_names() {
    use pi_codemode::{CodemodeSandbox, SandboxOptions};
    let noop = |name: &str| SandboxFunction::new(name, |_, _| async { Ok(None) });
    let create = |names: &[&str]| {
        CodemodeSandbox::new(SandboxOptions {
            globals: names.iter().map(|name| noop(name)).collect(),
            ..SandboxOptions::default()
        })
        .err()
        .map(|error| error.0)
        .unwrap_or_default()
    };
    for name in
        ["a.b.c", "a.", ".a", "tools.x", "store.x", "a.not-valid", "not-valid", "tools", "console", "store", "load"]
    {
        assert!(create(&[name]).contains("Invalid global"), "{name}");
    }
    assert!(create(&["models", "models.list"]).contains("conflicts with the namespace"));
}
