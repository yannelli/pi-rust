//! The built-in `codemode` tool in a session (upstream `agent-session-codemode.test.ts`): how the
//! callable tools are presented per `codemode.mode`, nested calls and their usage, failures, and
//! the structured results of the built-in bash and read tools.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use base64::Engine as _;
use pi_agent::{AgentTool, AgentToolResult, SharedTool, ToolUpdateCallback};
use pi_ai::providers::faux::FauxResponse;
use pi_ai::{ContentBlock, Cost, Message, ToolResultMessage, Usage};
use regex::Regex;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::harness::{Harness, Options, harness_with};
use crate::mcp_fake::TINY_PNG;
use crate::tool_support::{declared_description, tool_result};

pub struct TestTool {
    name: &'static str,
    description: &'static str,
    parameters: Value,
    output_schema: Option<Value>,
    run: fn(&Value) -> AgentToolResult,
}

#[async_trait]
impl AgentTool for TestTool {
    fn name(&self) -> &str {
        self.name
    }
    fn description(&self) -> &str {
        self.description
    }
    fn parameters(&self) -> Value {
        self.parameters.clone()
    }
    fn output_schema(&self) -> Option<Value> {
        self.output_schema.clone()
    }
    async fn execute(
        &self,
        _: &str,
        args: Value,
        _: CancellationToken,
        _: ToolUpdateCallback,
    ) -> anyhow::Result<AgentToolResult> {
        Ok((self.run)(&args))
    }
}

fn no_params() -> Value {
    json!({ "type": "object", "properties": {} })
}

pub fn usage(input: u64, cost: f64) -> Usage {
    Usage { input, total_tokens: input, cost: Cost { input: cost, total: cost, ..Cost::default() }, ..Usage::default() }
}

/// `echo`, `stats` (with an output schema), and `screenshot`.
pub fn test_tools() -> Vec<SharedTool> {
    let echo = TestTool {
        name: "echo",
        description: "Echo text back.\n\nSecond paragraph.",
        parameters: json!({ "type": "object", "properties": { "text": { "type": "string", "description": "Text to echo" } }, "required": ["text"] }),
        output_schema: None,
        run: |args| AgentToolResult::text(format!("echo: {}", args["text"].as_str().unwrap_or_default())),
    };
    let stats = TestTool {
        name: "stats",
        description: "Return structured stats",
        parameters: no_params(),
        output_schema: Some(json!({ "type": "object", "properties": { "files": { "type": "number" },
            "names": { "type": "array", "items": { "type": "string" } } }, "required": ["files", "names"] })),
        run: |_| AgentToolResult {
            structured_content: Some(json!({ "files": 2, "names": ["a", "b"] })),
            ..AgentToolResult::text("2 files")
        },
    };
    let screenshot = TestTool {
        name: "screenshot",
        description: "Return a screenshot",
        parameters: no_params(),
        output_schema: None,
        run: |_| AgentToolResult {
            content: vec![ContentBlock::text("captured"), image_block()],
            ..AgentToolResult::default()
        },
    };
    vec![Arc::new(echo), Arc::new(stats), Arc::new(screenshot)]
}

fn billed_tool() -> SharedTool {
    Arc::new(TestTool {
        name: "billed",
        description: "Run a model",
        parameters: no_params(),
        output_schema: None,
        run: |_| AgentToolResult { usage: Some(usage(100, 0.25)), ..AgentToolResult::text("ran") },
    })
}

pub fn image_block() -> ContentBlock {
    ContentBlock::Image { data: TINY_PNG.into(), mime_type: "image/png".into() }
}

/// A session whose `defaultTools` are `defaults`, with `tools` registered by the SDK.
pub async fn codemode_harness(defaults: &[&str], tools: Vec<SharedTool>, options: Options) -> Harness {
    let mut settings = options.settings.clone();
    settings["defaultTools"] = json!(defaults);
    harness_with(Options { settings, ..options }, |create| create.custom_tools = tools).await
}

static NEXT_CALL: AtomicU64 = AtomicU64::new(1);

/// Prompt once with a codemode call of `code` and return its result.
pub async fn run(h: &Harness, code: &str) -> ToolResultMessage {
    let id = format!("cm{}", NEXT_CALL.fetch_add(1, Ordering::Relaxed));
    h.provider.push(FauxResponse::tool_call(&id, "codemode", json!({ "code": code })));
    h.provider.push(FauxResponse::text("done"));
    h.session.prompt("go", Vec::new()).await.unwrap();
    tool_result(&h.session, "codemode")
}

/// The output after the script header, which is checked on the way; images read `<image>`.
pub fn output(result: &ToolResultMessage) -> String {
    let header = result.content[0].as_text().expect("text header");
    let pattern = Regex::new(r"^Script (completed|failed)\nWall time \d+\.\d seconds\nOutput:\n$").unwrap();
    assert!(pattern.is_match(header), "header: {header:?}");
    let items = result.content[1..].iter().map(|block| match block {
        ContentBlock::Text { text, .. } => text.clone(),
        _ => "<image>".into(),
    });
    items.collect::<Vec<_>>().join("\n")
}

/// Replace `[Image saved to ...]` labels with `<saved>` after checking each file holds the PNG.
pub fn check_saved_images(text: &str) -> String {
    let label = Regex::new(r"^\[Image saved to (\S+\.png) \(image/png, \d+B\)\]$").unwrap();
    let lines = text.lines().map(|line| match label.captures(line) {
        Some(captures) => {
            let path = &captures[1];
            let bytes = std::fs::read(path).expect("saved image");
            let _ = std::fs::remove_file(path);
            assert_eq!(base64::engine::general_purpose::STANDARD.encode(bytes), TINY_PNG);
            "<saved>".to_string()
        }
        None => line.to_string(),
    });
    lines.collect::<Vec<_>>().join("\n")
}

fn names(names: &[&str]) -> Vec<String> {
    names.iter().map(|name| name.to_string()).collect()
}

fn calls(result: &ToolResultMessage) -> Vec<Value> {
    result.details.as_ref().and_then(|details| details["calls"].as_array().cloned()).unwrap_or_default()
}

#[tokio::test]
async fn presents_callable_tools_per_codemode_mode() {
    let h = codemode_harness(&["codemode"], test_tools(), Options::default()).await;
    let session = &h.session;
    let description = |name: &str| declared_description(session, name).unwrap_or_default();

    // on: declared tools say how scripts call them and are not listed again in codemode.
    session.set_active_tools_by_name(&names(&["read", "echo", "codemode"]));
    assert!(description("echo").contains("Codemode: `tools.echo(args)` resolves to"));
    assert!(!description("echo").contains("codemode tool declaration:"));
    assert!(!description("codemode").contains("### `echo`"));
    h.provider.push(FauxResponse::text("ok"));
    session.prompt("on", Vec::new()).await.unwrap();
    let request = h.requests().pop().unwrap();
    let mut declared: Vec<&str> = request.tools.iter().map(|tool| tool.name.as_str()).collect();
    declared.sort();
    assert_eq!(declared, ["codemode", "echo", "read"]);
    assert!(request.system_prompt.unwrap().contains("\n- read: "));

    // only: codemode lists echo, which stays active but is left out of requests.
    h.settings.apply_overrides(json!({ "codemode": { "mode": "only" } }));
    session.set_active_tools_by_name(&names(&["read", "echo", "codemode"]));
    assert!(!description("echo").contains("Codemode: `tools.echo"));
    assert!(description("codemode").contains("### `echo`"));
    assert!(!description("codemode").contains("### `stats`"));
    h.provider.push(FauxResponse::text("ok"));
    session.prompt("only", Vec::new()).await.unwrap();
    let request = h.requests().pop().unwrap();
    let declared: Vec<&str> = request.tools.iter().map(|tool| tool.name.as_str()).collect();
    assert_eq!(declared, ["codemode"]);
    // The prompt's tool list matches the declarations, and hidden tools' guidelines move to codemode.
    let prompt = request.system_prompt.unwrap();
    assert!(!prompt.contains("\n- read: "));
    assert!(prompt.contains("\n- codemode: "));
    assert!(!session.system_prompt().contains("\n- read: "));
    assert!(!prompt.contains("Use read to examine files"));
    assert!(description("codemode").contains("- Use read to examine files instead of cat or sed."));

    // Without codemode, tools keep their plain descriptions.
    session.set_active_tools_by_name(&names(&["echo"]));
    assert_eq!(description("echo"), "Echo text back.\n\nSecond paragraph.");
}

#[tokio::test]
async fn shows_guidelines_of_tools_outside_the_inline_budget_through_describe_tool() {
    let h = codemode_harness(&["codemode"], test_tools(), Options::default()).await;
    h.settings.apply_overrides(json!({ "codemode": { "mode": "only", "inlineBudget": 0 } }));
    h.session.set_active_tools_by_name(&names(&["read", "codemode"]));
    assert!(!declared_description(&h.session, "codemode").unwrap().contains("### `read`"));
    let result = run(&h, r#"text(await describeTool("read"))"#).await;
    assert!(output(&result).contains("- Use read to examine files instead of cat or sed."));
}

#[tokio::test]
async fn runs_nested_calls_in_parallel_and_returns_only_the_script_result() {
    let h = codemode_harness(&["codemode"], test_tools(), Options::default()).await;
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
    let result = run(&h, code).await;
    assert!(!result.is_error);
    assert_eq!(
        output(&result),
        "==> text 1/2 <==\necho,stats,screenshot\n==> text 2/2 <==\n{\"a\":\"echo: one\",\"b\":\"echo: two\",\"names\":[\"a\",\"b\"]}\n<console_output>\nfiles 2\n</console_output>"
    );
    let calls = calls(&result);
    let summary: Vec<(&str, &str)> =
        calls.iter().map(|call| (call["name"].as_str().unwrap(), call["status"].as_str().unwrap())).collect();
    assert_eq!(summary, [("echo", "ok"), ("echo", "ok"), ("stats", "ok")]);
    let prefix = format!("{}/", result.tool_call_id);
    assert!(calls.iter().all(|call| call["id"].as_str().unwrap().starts_with(&prefix)));
    // Nested calls are recorded on the result and never become transcript tool results.
    let nested = result.nested_calls.as_ref().expect("nested calls");
    assert_eq!(nested.calls.iter().map(|call| call.name.as_str()).collect::<Vec<_>>(), ["echo", "echo", "stats"]);
    let results = h.session.messages().iter().filter(|message| matches!(message, Message::ToolResult(_))).count();
    assert_eq!(results, 1);
    let starts = h.events_of("tool_execution_start");
    let nested_starts = starts.iter().filter(|event| event["parentToolCallId"] == result.tool_call_id).count();
    assert_eq!(nested_starts, 3);
}

#[tokio::test]
async fn adds_the_usage_of_nested_results_to_the_codemode_result() {
    let mut tools = test_tools();
    tools.push(billed_tool());
    let h = codemode_harness(&["codemode"], tools, Options::default()).await;
    let result = run(&h, r#"await tools.billed({}); await tools.billed({}); await tools.echo({ text: "x" });"#).await;

    let usage = result.usage.expect("usage");
    assert_eq!((usage.input, usage.total_tokens, usage.cost.total), (200, 200, 0.5));
    // The usage is persisted with the result, so session totals count it.
    let persisted = h.entries().into_iter().find(|entry| entry["message"]["role"] == "toolResult").unwrap();
    assert_eq!(persisted["message"]["usage"], serde_json::to_value(usage).unwrap());
    assert_eq!(h.session.session_stats().stats.cost, 0.5);
}

#[tokio::test]
async fn attaches_only_the_images_passed_to_image_each_after_its_saved_path() {
    let h = codemode_harness(&["codemode"], test_tools(), Options::default()).await;
    let code = format!(
        "const shot = await tools.screenshot({{}});\ntext(shot);\nimage(\"data:image/png;base64,{TINY_PNG}\");\nimage(\"data:image/png;base64,{TINY_PNG}\");\ntext(\"after\");"
    );
    let result = run(&h, &code).await;
    let text = output(&result);
    let lines: Vec<&str> = text.lines().collect();
    // The same image shown twice is saved once, so both labels name one file.
    assert_eq!(
        lines,
        ["==> text 1/2 <==", "captured", lines[2], "<image>", lines[2], "<image>", "==> text 2/2 <==", "after"]
    );
    assert_eq!(check_saved_images(lines[2]), "<saved>");
    assert_eq!(result.content[2], image_block());
}

#[tokio::test]
async fn marks_where_each_text_item_starts_and_puts_console_lines_last() {
    let h = codemode_harness(&["codemode"], test_tools(), Options::default()).await;
    let result =
        run(&h, "text(\"one\\ntwo\");\nconsole.log(\"a\");\nconsole.log(\"b\");\ntext(\"three\\n\");\nreturn 4;").await;
    assert_eq!(result.content.len(), 2);
    assert_eq!(
        output(&result),
        "==> text 1/3 <==\none\ntwo\n==> text 2/3 <==\nthree\n==> text 3/3 <==\n4\n<console_output>\na\nb\n</console_output>"
    );
}

#[tokio::test]
async fn reports_script_failures_with_partial_output_and_the_calls_that_ran() {
    let h = codemode_harness(&["codemode"], test_tools(), Options::default()).await;
    let code =
        "text(\"partial\");\nconsole.log(\"log\");\nawait tools.echo({ text: \"x\" });\nthrow new Error(\"boom\");";
    let result = run(&h, code).await;
    assert!(result.is_error);
    assert!(result.content[0].as_text().unwrap().starts_with("Script failed\n"));
    let text = output(&result);
    assert!(
        text.starts_with("partial\n<console_output>\nlog\n</console_output>\nScript error:\nError: boom\n"),
        "{text}"
    );
    assert!(text.contains("codemode.js:4"), "{text}");
    assert!(text.contains("Tool calls made before the failure (they are not undone): echo (ok)"));
    assert_eq!(calls(&result).iter().map(|call| call["name"].clone()).collect::<Vec<_>>(), [json!("echo")]);
}

#[tokio::test]
async fn applies_the_timeout_option_and_rejects_invalid_options() {
    let h = codemode_harness(&["codemode"], Vec::new(), Options::default()).await;
    let timed_out = run(&h, "// @options: {\"timeout_ms\": 200}\nwhile (true) {}").await;
    assert!(timed_out.is_error);
    assert!(output(&timed_out).contains("Script error:\nScript timed out"));

    let invalid = run(&h, "// @options: {\"yield\": 1}\ntext(1)").await;
    assert!(invalid.is_error);
    assert_eq!(
        invalid.content,
        [ContentBlock::text("@options only supports `max_output_tokens` and `timeout_ms`; got `yield`")]
    );

    // #10631: a misplaced options line fails before the script runs instead of running without a deadline.
    let misplaced = run(&h, "// count\n// @options: {\"timeout_ms\": 200}\nwhile (true) {}").await;
    assert!(misplaced.is_error);
    assert_eq!(
        misplaced.content,
        [ContentBlock::text("The // @options: line must be the first line of the input; found one on line 2")]
    );
}

#[tokio::test]
async fn resolves_bash_calls_to_structured_results_also_for_non_zero_exit_codes() {
    let h = codemode_harness(&["codemode", "bash"], Vec::new(), Options::default()).await;
    let code = "const r = await tools.bash({ command: \"echo out; exit 3\" });\ntext(JSON.stringify([r.output, r.exit_code, typeof r.wall_time_seconds]));";
    let result = run(&h, code).await;
    assert!(!result.is_error, "{:?}", result.content);
    assert_eq!(output(&result), r#"["out\n",3,"number"]"#);
}

#[tokio::test]
async fn resolves_read_calls_to_text_and_to_images_that_image_shows() {
    let h = codemode_harness(&["codemode", "read"], Vec::new(), Options::default()).await;
    std::fs::write(h.cwd.join("notes.txt"), "hello").unwrap();
    std::fs::write(h.cwd.join("pixel.png"), base64::engine::general_purpose::STANDARD.decode(TINY_PNG).unwrap())
        .unwrap();
    let code = "text(await tools.read({ path: \"notes.txt\" }));\nconst shot = await tools.read({ path: \"pixel.png\" });\ntext(shot.note);\nimage(shot);";
    let result = run(&h, code).await;
    assert!(!result.is_error, "{:?}", result.content);
    assert_eq!(
        check_saved_images(&output(&result)),
        "==> text 1/2 <==\nhello\n==> text 2/2 <==\nRead image file [image/png]\n<saved>\n<image>"
    );
    assert_eq!(result.content.last(), Some(&image_block()));
}
