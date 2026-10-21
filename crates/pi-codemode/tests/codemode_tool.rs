//! Ported from upstream `coding-agent/test/suite/agent-session-codemode.test.ts`: scripts, output,
//! options, and the store, against a host stand-in.

mod support;

use std::sync::Arc;

use pi_agent::AgentTool;
use pi_ai::ContentBlock;
use pi_codemode::{
    CodemodeNestedCallStatus, CodemodeTool, CodemodeToolInfo, CodemodeToolOptions, NestedToolOutcome, execute_codemode,
};
use serde_json::{Value, json};
use support::*;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn runs_nested_calls_in_parallel_and_returns_only_the_script_result() {
    let host = Arc::new(FakeHost::standard());
    let code = r#"
        const [a, b, stats] = await Promise.all([
            tools.echo({ text: "one" }),
            tools.echo({ text: "two" }),
            tools.stats({}),
        ]);
        console.log("files", stats.files);
        text(ALL_TOOLS.map((tool) => tool.name).join(","));
        return { a, b, names: stats.names };
    "#;
    let output = run_script(&host, code).await;
    assert!(!output.is_error);
    assert_eq!(
        result_text(&output),
        "==> text 1/2 <==\necho,stats,screenshot\n==> text 2/2 <==\n{\"a\":\"echo: one\",\"b\":\"echo: two\",\"names\":[\"a\",\"b\"]}\n<console_output>\nfiles 2\n</console_output>"
    );
    let calls: Vec<_> = output.details.calls.iter().map(|call| (call.name.as_str(), call.status)).collect();
    assert_eq!(
        calls,
        [
            ("echo", CodemodeNestedCallStatus::Ok),
            ("echo", CodemodeNestedCallStatus::Ok),
            ("stats", CodemodeNestedCallStatus::Ok)
        ]
    );
    assert!(output.details.calls.iter().all(|call| call.id.starts_with("call-1/")));
    assert_eq!(output.details.calls[0].args, "{\"text\":\"one\"}");
}

#[tokio::test]
async fn rejects_blocked_calls_with_the_tool_error_text() {
    let host = Arc::new(
        FakeHost::standard()
            .tool(CodemodeToolInfo::new("guarded", "Blocked by a hook", json!({ "type": "object" })), |_| {
                error_outcome("echo of forbidden text is blocked")
            }),
    );
    let code = "let blocked;\ntry { await tools.guarded({}); } catch (error) { blocked = error.message; }\nconst stats = await tools.stats({});\nreturn { blocked, stats };";
    let output = run_script(&host, code).await;
    let value: Value = serde_json::from_str(&result_text(&output)).unwrap();
    assert_eq!(
        value,
        json!({ "blocked": "echo of forbidden text is blocked", "stats": { "files": 2, "names": ["a", "b"] } })
    );
    let statuses: Vec<_> = output.details.calls.iter().map(|call| call.status).collect();
    assert_eq!(statuses, [CodemodeNestedCallStatus::Error, CodemodeNestedCallStatus::Ok]);
    assert_eq!(output.details.calls[0].error.as_deref(), Some("echo of forbidden text is blocked"));
}

#[tokio::test]
async fn resolves_structured_tools_to_text_when_a_result_has_no_structured_content() {
    let host = Arc::new(FakeHost::default().tool(
        CodemodeToolInfo {
            output_schema: Some(json!({ "type": "object" })),
            ..CodemodeToolInfo::new("stats", "", json!({}))
        },
        |_| text_outcome("redacted"),
    ));
    assert_eq!(result_text(&run_script(&host, "return await tools.stats({});").await), "redacted");
}

#[tokio::test]
async fn attaches_only_images_passed_to_image_in_order_after_their_saved_path() {
    let host = Arc::new(FakeHost::standard());
    let code = format!(
        "const shot = await tools.screenshot({{}});\ntext(shot);\nimage(\"data:image/png;base64,{TINY_PNG_BASE64}\");\nimage(\"data:image/png;base64,{TINY_PNG_BASE64}\");\ntext(\"after\");"
    );
    let output = run_script(&host, &code).await;
    let text = result_text(&output);
    let lines: Vec<&str> = text.split('\n').collect();
    // The same image shown twice is saved once, so both labels name one file.
    assert_eq!(
        lines,
        ["==> text 1/2 <==", "captured", lines[2], "<image>", lines[2], "<image>", "==> text 2/2 <==", "after"]
    );
    assert_eq!(check_saved_images(lines[2]), "<saved>");
    assert_eq!(output.content[2], ContentBlock::Image { data: TINY_PNG_BASE64.into(), mime_type: "image/png".into() });
}

#[tokio::test]
async fn marks_text_items_and_puts_console_lines_last_in_one_block() {
    let host = Arc::new(FakeHost::default());
    let code = "text(\"one\\ntwo\");\nconsole.log(\"a\");\nconsole.log(\"b\");\ntext(\"three\\n\");\nreturn 4;";
    let output = run_script(&host, code).await;
    assert_eq!(output.content.len(), 2);
    assert_eq!(
        result_text(&output),
        "==> text 1/3 <==\none\ntwo\n==> text 2/3 <==\nthree\n==> text 3/3 <==\n4\n<console_output>\na\nb\n</console_output>"
    );
}

#[tokio::test]
async fn reports_failures_with_partial_output_and_the_calls_that_ran() {
    let host = Arc::new(FakeHost::standard());
    let code =
        "text(\"partial\");\nconsole.log(\"log\");\nawait tools.echo({ text: \"x\" });\nthrow new Error(\"boom\");";
    let output = run_script(&host, code).await;
    assert!(output.is_error);
    assert!(output.content[0].as_text().unwrap().starts_with("Script failed\n"));
    let text = result_text(&output);
    assert!(
        text.starts_with("partial\n<console_output>\nlog\n</console_output>\nScript error:\nError: boom\n"),
        "{text}"
    );
    assert!(text.contains("codemode.js:4"));
    assert!(text.contains("Tool calls made before the failure (they are not undone): echo (ok)"));
    assert_eq!(output.details.calls.iter().map(|call| call.name.as_str()).collect::<Vec<_>>(), ["echo"]);
}

#[tokio::test]
async fn applies_timeout_ms_and_rejects_invalid_options() {
    let host = Arc::new(FakeHost::default());
    let timed_out = run_script(&host, "// @options: {\"timeout_ms\": 200}\nwhile (true) {}").await;
    assert!(timed_out.is_error);
    assert!(result_text(&timed_out).contains("Script error:\nScript timed out"));

    let tool = CodemodeTool::new(host.clone(), CodemodeToolOptions::default());
    let invalid = tool.run("call-2", "// @options: {\"yield\": 1}\ntext(1)", CancellationToken::new(), None).await;
    assert_eq!(
        invalid.unwrap_err().to_string(),
        "@options only supports `max_output_tokens` and `timeout_ms`; got `yield`"
    );

    // #10631: a misplaced options line fails before the script runs instead of running without a deadline.
    let misplaced = tool.run(
        "call-3",
        "// count\n// @options: {\"timeout_ms\": 200}\nwhile (true) {}",
        CancellationToken::new(),
        None,
    );
    assert_eq!(
        misplaced.await.unwrap_err().to_string(),
        "The // @options: line must be the first line of the input; found one on line 2"
    );
}

#[tokio::test]
async fn limits_script_memory_so_runaway_allocations_fail_inside_the_script() {
    let host = Arc::new(FakeHost::default());
    let code = "// @options: {\"timeout_ms\": 30000}\nlet a = [];\ntry { while (true) a.push(\"x\".repeat(1 << 20) + a.length); } catch (error) { const n = a.length; a = null; return { n, error: String(error) }; }";
    let output = run_script(&host, code).await;
    assert!(!output.is_error);
    let value: Value = serde_json::from_str(&result_text(&output)).unwrap();
    assert!(value["error"].as_str().unwrap().contains("out of memory"));
    assert!(value["n"].as_u64().unwrap() < 512);
}

#[tokio::test]
async fn truncates_output_to_the_token_budget_and_spills_the_full_text() {
    let host = Arc::new(FakeHost::default());
    let code = format!(
        "// @options: {{\"max_output_tokens\": 30}}\nfor (let i = 0; i < 100; i++) text(\"row \" + i);\nimage(\"data:image/png;base64,{TINY_PNG_BASE64}\");"
    );
    let output = run_script(&host, &code).await;
    let path = output.details.full_output_path.clone().expect("spill file");
    let text = result_text(&output);
    let full = std::fs::read_to_string(&path).unwrap();
    std::fs::remove_file(&path).unwrap();
    assert!(text.starts_with("Warning: truncated output"));
    assert!(text.contains("row 0\n") && text.contains("tokens truncated") && text.contains("row 99\n"));
    assert!(!text.contains("row 50\n"));
    assert!(text.contains(&format!("[Full output: {path} (read with offset/limit)]")));
    // Images follow the truncated text, each after the path it was saved to.
    assert!(matches!(output.content.last(), Some(ContentBlock::Image { .. })));
    let lines: Vec<&str> = text.split('\n').collect();
    assert_eq!(check_saved_images(lines[lines.len() - 2]), "<saved>");
    let expected: Vec<String> = (0..100).map(|i| format!("==> text {}/100 <==\nrow {i}", i + 1)).collect();
    assert_eq!(full, expected.join("\n"));

    let small = run_script(&host, "return { ok: true };").await;
    assert_eq!(small.details.full_output_path, None);
}

const INCREMENT: &str = "const next = (load(\"count\") ?? 0) + 1;\nstore(\"count\", next);\nreturn next;";

#[tokio::test]
async fn persists_store_writes_as_entries_for_later_calls() {
    let host = Arc::new(FakeHost::default());
    assert_eq!(result_text(&run_script(&host, INCREMENT).await), "1");
    assert_eq!(result_text(&run_script(&host, INCREMENT).await), "2");
    assert_eq!(
        host.store_entries(),
        [json!({ "set": { "count": 1 }, "delete": [] }), json!({ "set": { "count": 2 }, "delete": [] })]
    );
    let deleted = run_script(&host, "store(\"count\", undefined);\nreturn load(\"count\") === undefined;").await;
    assert_eq!(result_text(&deleted), "true");
    assert_eq!(host.store_entries().last(), Some(&json!({ "set": {}, "delete": ["count"] })));
}

#[tokio::test]
async fn appends_nothing_for_failed_scripts_or_scripts_without_writes() {
    let host = Arc::new(FakeHost::default());
    assert!(run_script(&host, "store(\"count\", 5);\nthrow new Error(\"boom\");").await.is_error);
    assert!(!run_script(&host, "return load(\"count\") ?? \"missing\";").await.is_error);
    assert!(host.store_entries().is_empty());
}

#[tokio::test]
async fn runs_through_the_agent_tool_interface() {
    let host = Arc::new(FakeHost::default());
    let tool = CodemodeTool::new(host, CodemodeToolOptions::default());
    assert_eq!(tool.name(), "codemode");
    assert!(tool.declaration().constrained_sampling.is_some());
    let result =
        tool.execute("direct", json!({ "code": INCREMENT }), CancellationToken::new(), Arc::new(|_| {})).await.unwrap();
    assert_eq!(result.content[1], ContentBlock::text("1"));
    assert_eq!(result.details, Some(json!({ "calls": [] })));
}

#[tokio::test]
async fn publishes_nested_call_rows_while_the_script_runs() {
    let host = Arc::new(FakeHost::standard());
    let updates = Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = updates.clone();
    let on_update: pi_agent::ToolUpdateCallback = Arc::new(move |result| seen.lock().unwrap().push(result.details));
    let dyn_host: Arc<dyn pi_codemode::CodemodeHost> = host;
    let options = CodemodeToolOptions::default();
    execute_codemode(
        dyn_host,
        "id",
        "await tools.echo({ text: 'a' })",
        CancellationToken::new(),
        Some(on_update),
        &options,
    )
    .await
    .unwrap();
    let updates = updates.lock().unwrap();
    assert_eq!(updates.len(), 2);
    assert_eq!(updates[0].as_ref().unwrap()["calls"][0]["status"], json!("running"));
    assert_eq!(updates[1].as_ref().unwrap()["calls"][0]["status"], json!("ok"));
}

#[tokio::test]
async fn cancels_the_script_and_its_nested_calls() {
    let host = Arc::new(FakeHost::default().async_tool(CodemodeToolInfo::new("hang", "", json!({})), |_| {
        Box::pin(futures::future::pending::<NestedToolOutcome>())
    }));
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        trigger.cancel();
    });
    let dyn_host: Arc<dyn pi_codemode::CodemodeHost> = host;
    let output =
        execute_codemode(dyn_host, "id", "await tools.hang({})", cancel, None, &CodemodeToolOptions::default())
            .await
            .unwrap();
    assert!(output.is_error);
    assert!(result_text(&output).contains("Script aborted: Execution aborted"));
    assert_eq!(output.details.calls[0].status, CodemodeNestedCallStatus::Cancelled);
}
