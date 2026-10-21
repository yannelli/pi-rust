//! Port of upstream `test/codemode.test.ts`: what scripts get for tool results, the result's layout and files, and
//! tool search.

use pi_ai::ToolResultMessage;
use pi_durable::env::SharedEnv;

use super::env::{HookedEnv, local_env};
use super::*;

const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==";

fn exit_schema() -> Value {
    json!({
        "type": "object",
        "properties": { "output": { "type": "string" }, "exitCode": { "type": "number" } },
        "required": ["output", "exitCode"],
    })
}

#[tokio::test]
async fn resolves_a_schema_tool_s_error_result_that_carries_its_value_and_rejects_one_without_with_its_text() {
    let chat = codemode_chat(vec![]);
    let empty = json!({ "type": "object", "properties": {} });
    // Like bash on a nonzero exit: an error result with its value.
    let run = define_tool("run", "run", empty.clone(), |_, _, _| async {
        Ok(ToolExecutionResult {
            structured_output: Some(Json::from(json!({ "output": "no matches", "exitCode": 1 }))),
            is_error: Some(true),
            ..ToolExecutionResult::text("no matches")
        })
    });
    add_tool(&chat.setup, run.with_structured_output_schema(exit_schema()));
    let denied =
        define_tool("denied", "denied", empty, |_, _, _| async { Ok(ToolExecutionResult::error("permission denied")) });
    add_tool(&chat.setup, denied.with_structured_output_schema(exit_schema()));
    chat.holds.block.store(false, Ordering::SeqCst);
    let code = r#"const r = await tools.run({});
        let e; try { await tools.denied({}); } catch (error) { e = [error.message, error.structuredOutput ?? null]; }
        return [r.exitCode, r.output, e];"#;
    chat.setup.faux.set_responses([script(code), done()]);
    let directory = tempfile::tempdir().unwrap();
    let (harness, root) = run_once(&chat, &directory).await;
    assert_eq!(result_text(&root).await, (r#"[1,"no matches",["permission denied",null]]"#.into(), false));
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn resolves_a_schema_less_tool_with_its_text_as_its_declaration_says_also_when_it_returned_images() {
    let chat = codemode_chat(vec![]);
    let parameters =
        json!({ "type": "object", "properties": { "caption": { "type": "boolean" } }, "required": ["caption"] });
    let picture = define_tool("picture", "picture", parameters, |args, _, _| async move {
        let mut output = Vec::new();
        if args.get("caption") == Some(&Value::Bool(true)) {
            output.push(ContentBlock::text("a cat"));
        }
        output.push(ContentBlock::Image { data: PNG.into(), mime_type: "image/png".into() });
        Ok(ToolExecutionResult { output: Some(output), ..ToolExecutionResult::default() })
    });
    add_tool(&chat.setup, picture);
    chat.holds.block.store(false, Ordering::SeqCst);
    let code = "return [await tools.picture({ caption: true }), await tools.picture({ caption: false })];";
    chat.setup.faux.set_responses([script(code), done()]);
    let directory = tempfile::tempdir().unwrap();
    let (harness, root) = run_once(&chat, &directory).await;
    assert_eq!(result_text(&root).await, (r#"["a cat",""]"#.into(), false));
    harness.close(&ctx()).await.unwrap();
}

/// The code mode call's result message, and its content as text with `<image>` for images.
pub async fn tool_result(root: &Conversation) -> (String, ToolResultMessage) {
    let entries = all_entries(root).await;
    let entry = entries.iter().find(|entry| entry.kind == "pi.tool-result").unwrap();
    let Some(Message::ToolResult(result)) = entry.model.as_ref().and_then(|model| model.first()) else { panic!() };
    let text = result.content.iter().map(|item| match item {
        ContentBlock::Text { text, .. } => text.clone(),
        ContentBlock::Image { .. } => "<image>".into(),
        _ => "<other>".into(),
    });
    (text.collect(), result.clone())
}

pub fn details_of(result: &ToolResultMessage) -> CodemodeToolDetails {
    serde_json::from_value(result.details.clone().unwrap()).unwrap()
}

/// Remove the directory of a result file, which lives in the system's temp directory.
pub fn remove_parent(path: &str) {
    if let Some(parent) = std::path::Path::new(path).parent() {
        let _ = std::fs::remove_dir_all(parent);
    }
}

/// Open over a fresh SQLite file in `directory` with `env`.
async fn open_with_env(path: &str, chat: &Chat, env: SharedEnv) -> (Harness, Conversation) {
    let options = chat.setup.options().with_shared_env(env);
    let opened = open_chat_with(Arc::new(open_sqlite_storage(path).await.unwrap()), &chat.setup, options).await;
    opened.0.resume().unwrap();
    opened
}

/// Run `code` once in a chat with a local environment; the result's text and message.
async fn run_script(code: &str, chat: Chat) -> (String, ToolResultMessage) {
    chat.holds.block.store(false, Ordering::SeqCst);
    let (cwd, directory) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    chat.setup.faux.set_responses([script(code), done()]);
    let (harness, root) = open_with_env(&sqlite_path(&directory), &chat, local_env(cwd.path())).await;
    let id = submit(&root, "go").await;
    wait(&harness, id).await;
    let ran = tool_result(&root).await;
    harness.close(&ctx()).await.unwrap();
    if let Some(path) = details_of(&ran.1).full_output_path {
        remove_parent(&path);
    }
    ran
}

/// Whether `text` starts with the header `head`, its wall time, and `Output:`.
fn has_header(text: &str, head: &str) -> bool {
    text.starts_with(&format!("{head}\nWall time ")) && without_header(text) != text
}

#[tokio::test]
async fn lays_out_text_items_console_lines_and_the_returned_value_under_a_header() {
    let code = r#"text("one"); console.log("log", 1); text({ two: 2 }); return "three";"#;
    let (text, _) = run_script(code, codemode_chat(vec![])).await;
    assert!(has_header(&text, "Script completed"), "{text}");
    assert!(text.contains("==> text 1/3 <==\none\n==> text 2/3 <==\n{\"two\":2}\n==> text 3/3 <==\nthree"), "{text}");
    assert!(text.contains("<console_output>\nlog 1\n</console_output>"), "{text}");
}

#[tokio::test]
async fn cuts_text_past_the_token_budget_to_its_start_and_end_with_the_full_text_in_a_file() {
    let chat = codemode_chat(vec![]);
    chat.holds.block.store(false, Ordering::SeqCst);
    let (cwd, directory) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let code = "// @options: {\"max_output_tokens\": 20}\ntext(\"BEGIN\" + \"x\".repeat(1000) + \"END\");";
    chat.setup.faux.set_responses([script(code), done()]);
    let (harness, root) = open_with_env(&sqlite_path(&directory), &chat, local_env(cwd.path())).await;
    let id = submit(&root, "go").await;
    wait(&harness, id).await;
    let (text, result) = tool_result(&root).await;
    harness.close(&ctx()).await.unwrap();
    for part in ["Warning: truncated output", "BEGIN", "END"] {
        assert!(text.contains(part), "{text}");
    }
    let path = details_of(&result).full_output_path.unwrap();
    assert!(text.contains(&format!("[Full output: {path}")), "{text}");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), format!("BEGIN{}END", "x".repeat(1000)));
    remove_parent(&path);
}

#[tokio::test]
async fn keeps_a_result_inside_the_tool_s_output_limits_whatever_max_output_tokens_says_spilling_the_rest() {
    // 6 MiB of text: over the Harness's 4 MiB limit, which would cut it without a spill file.
    let chat = codemode_chat(vec![]);
    chat.holds.block.store(false, Ordering::SeqCst);
    let (cwd, directory) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let code = "// @options: {\"max_output_tokens\": 2000000}\nreturn \"x\".repeat(6 * 1024 * 1024);";
    chat.setup.faux.set_responses([script(code), done()]);
    let (harness, root) = open_with_env(&sqlite_path(&directory), &chat, local_env(cwd.path())).await;
    let id = submit(&root, "go").await;
    wait(&harness, id).await;
    let (text, result) = tool_result(&root).await;
    harness.close(&ctx()).await.unwrap();
    assert!(text.contains("Warning: truncated output"));
    assert!(text.len() <= 1024 * 1024 + 1000, "{} bytes", text.len());
    let path = details_of(&result).full_output_path.unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap().len(), 6 * 1024 * 1024);
    remove_parent(&path);
    // 200,000 short lines: within the byte limit, over the 100,000 lines one.
    let code = "// @options: {\"max_output_tokens\": 2000000}\nreturn Array.from({ length: 200000 }, () => \"a\").join(\"\\n\");";
    let (text, result) = run_script(code, codemode_chat(vec![])).await;
    assert!(text.contains("Warning: truncated output"));
    assert!(text.split('\n').count() < 100_000);
    assert!(details_of(&result).full_output_path.is_some());
}

#[tokio::test]
async fn writes_the_result_s_files_to_the_same_paths_when_it_reruns_after_a_crash() {
    let chat = codemode_chat(vec![]);
    chat.holds.block.store(false, Ordering::SeqCst);
    let (cwd, directory) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (written, first) = (Deferred::default(), Arc::new(Mutex::new(String::new())));
    // The process dies after the full output was written and before the result was committed.
    let mut crashing = HookedEnv::new(cwd.path());
    let (gate, path) = (written.clone(), first.clone());
    crashing.after_write = Some(Arc::new(move |written: String, context: Context| {
        *path.lock().unwrap() = written;
        gate.resolve();
        Box::pin(async move { aborted(context.abort_signal().unwrap()).await })
    }));
    let storage = sqlite_path(&directory);
    chat.setup
        .faux
        .set_responses([script("// @options: {\"max_output_tokens\": 20}\nreturn \"x\".repeat(1000);"), done()]);
    let (harness, root) = open_with_env(&storage, &chat, Arc::new(crashing)).await;
    let id = submit(&root, "go").await;
    written.wait().await;
    harness.close(&ctx()).await.unwrap();
    let (harness, root) = open_with_env(&storage, &chat, local_env(cwd.path())).await;
    assert_eq!(wait(&harness, id).await, SubmissionStatus::Done);
    let (_, result) = tool_result(&root).await;
    let first = first.lock().unwrap().clone();
    assert_eq!(details_of(&result).full_output_path.as_deref(), Some(first.as_str()));
    let parent = std::path::Path::new(&first).parent().unwrap();
    let names: Vec<String> =
        std::fs::read_dir(parent).unwrap().map(|entry| entry.unwrap().file_name().to_string_lossy().into()).collect();
    assert_eq!(names, ["output.txt"]);
    remove_parent(&first);
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn saves_images_to_files_and_names_them_next_to_the_image() {
    let (text, result) = run_script(&format!("image(\"data:image/png;base64,{PNG}\");"), codemode_chat(vec![])).await;
    let saved = text.split("[Image saved to ").nth(1).and_then(|rest| rest.split_once(" (image/png, "));
    let (path, rest) = saved.unwrap_or_else(|| panic!("{text}"));
    assert!(rest.split_once(" bytes)]").is_some_and(|(bytes, _)| bytes.parse::<usize>().is_ok()), "{text}");
    use base64::Engine;
    let saved = base64::engine::general_purpose::STANDARD.encode(std::fs::read(path).unwrap());
    assert_eq!(saved, PNG);
    assert!(result.content.iter().any(|item| matches!(item, ContentBlock::Image { .. })));
    remove_parent(path);
}

#[tokio::test]
async fn reports_a_failed_script_with_its_error_and_the_calls_already_made() {
    let code = r#"await tools.echo({ text: "a" }); throw new Error("boom");"#;
    let (text, result) = run_script(code, codemode_chat(vec![])).await;
    assert!(result.is_error);
    assert!(has_header(&text, "Script failed"), "{text}");
    assert!(text.contains("Error: boom"), "{text}");
    assert!(text.contains("Tool calls made before the failure (they are not undone): echo (ok)"), "{text}");
}

#[tokio::test]
async fn finds_tools_with_search_tools_describe_tool_and_describe_namespace_and_lists_them_in_all_tools() {
    let github = CodemodeNamespace::new("github", "mcp__github__").with_description("GitHub MCP server");
    let chat = codemode_chat_with(vec![], vec![github]);
    let list = tool("mcp__github__list_issues", |_, _, _| async {
        Ok(ToolExecutionResult { output: Some(Vec::new()), ..ToolExecutionResult::default() })
    });
    add_tool(&chat.setup, list);
    let code = r#"const found = await searchTools("issues");
        const inNamespace = await searchTools("echo", { namespace: "github" });
        return {
            found: found.map((tool) => tool.name),
            inNamespace,
            described: (await describeTool("echo")).includes("echo(args:"),
            missing: (await describeTool("nope")) ?? null,
            namespace: await describeNamespace("github"),
            all: ALL_TOOLS.map((tool) => tool.name).includes("mcp__github__list_issues"),
        };"#;
    let (text, _) = run_script(code, chat).await;
    let value: Value = serde_json::from_str(&without_header(&text)).unwrap();
    let namespace =
        json!({ "name": "github", "description": "GitHub MCP server", "tools": ["mcp__github__list_issues"] });
    let expected = json!({
        "found": ["mcp__github__list_issues"],
        "inNamespace": [],
        "described": true,
        "missing": null,
        "namespace": namespace,
        "all": true,
    });
    assert_eq!(value, expected);
}
