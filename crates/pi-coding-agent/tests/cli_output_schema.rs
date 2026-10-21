//! `--output-schema` through the `pi` binary against a local mock server.

mod cli_support;

use std::path::Path;
use std::process::Output;

use serde_json::{Value, json};

use cli_support::{events, mock_server, pi_async, setup, text_body};

const MODEL: [&str; 2] = ["--model", "mock/mock-model"];

fn write_schema(root: &Path) {
    let schema = json!({
        "type": "object",
        "properties": {"name": {"type": "string"}, "tags": {"type": "array", "items": {"type": "string"}}},
        "required": ["name", "tags"],
        "additionalProperties": false
    });
    std::fs::write(root.join("work/schema.json"), schema.to_string()).unwrap();
}

async fn run(root: &Path, flags: &[&str]) -> Output {
    let mut args: Vec<&str> = MODEL.to_vec();
    args.extend_from_slice(&["--output-schema", "schema.json"]);
    args.extend_from_slice(flags);
    pi_async(root, &args).await
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

/// The text of the last user message of a recorded request.
fn last_user_text(request: &Value) -> String {
    let messages = request["messages"].as_array().unwrap();
    let user = messages.iter().rev().find(|message| message["role"] == "user").unwrap();
    match &user["content"] {
        Value::String(text) => text.clone(),
        content => content.as_array().unwrap().iter().filter_map(|part| part["text"].as_str()).collect(),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn prints_the_valid_json_answer_compactly() {
    let root = tempfile::tempdir().unwrap();
    let (url, requests) = mock_server(vec![text_body("{\n  \"name\": \"pi\",\n  \"tags\": [\"a\", \"b\"]\n}")]).await;
    setup(root.path(), &url);
    write_schema(root.path());
    let output = run(root.path(), &["-p", "Describe pi"]).await;
    assert_eq!(output.status.code(), Some(0), "stderr: {}", stderr(&output));
    assert_eq!(stdout(&output), "{\"name\":\"pi\",\"tags\":[\"a\",\"b\"]}\n");
    let requests = requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 1);
    let system = requests[0]["messages"][0]["content"].as_str().unwrap().to_string();
    assert!(system.contains("Your final message must be only a JSON value"), "{system}");
    assert!(system.contains("\"additionalProperties\": false"), "{system}");
}

#[tokio::test(flavor = "multi_thread")]
async fn accepts_a_fenced_answer() {
    let root = tempfile::tempdir().unwrap();
    let (url, _) = mock_server(vec![text_body("```json\n{\"name\": \"pi\", \"tags\": []}\n```")]).await;
    setup(root.path(), &url);
    write_schema(root.path());
    let output = run(root.path(), &["-p", "Describe pi"]).await;
    assert_eq!(output.status.code(), Some(0), "stderr: {}", stderr(&output));
    assert_eq!(stdout(&output), "{\"name\":\"pi\",\"tags\":[]}\n");
}

#[tokio::test(flavor = "multi_thread")]
async fn sends_one_corrective_prompt_with_the_errors() {
    let root = tempfile::tempdir().unwrap();
    let bodies = vec![text_body("{\"name\": 7}"), text_body("{\"name\": \"pi\", \"tags\": [\"x\"]}")];
    let (url, requests) = mock_server(bodies).await;
    setup(root.path(), &url);
    write_schema(root.path());
    let output = run(root.path(), &["-p", "Describe pi"]).await;
    assert_eq!(output.status.code(), Some(0), "stderr: {}", stderr(&output));
    assert_eq!(stdout(&output), "{\"name\":\"pi\",\"tags\":[\"x\"]}\n");
    let requests = requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 2);
    let correction = last_user_text(&requests[1]);
    assert!(correction.contains("does not match the required JSON Schema"), "{correction}");
    assert!(correction.contains("- tags: must have required properties tags"), "{correction}");
    assert!(correction.contains("- name: must be string"), "{correction}");
}

#[tokio::test(flavor = "multi_thread")]
async fn exits_4_with_the_errors_when_the_answer_stays_invalid() {
    let root = tempfile::tempdir().unwrap();
    let (url, requests) = mock_server(vec![text_body("Sure, here it is."), text_body("{\"name\": \"pi\"}")]).await;
    setup(root.path(), &url);
    write_schema(root.path());
    let output = run(root.path(), &["-p", "Describe pi"]).await;
    assert_eq!(output.status.code(), Some(4), "stderr: {}", stderr(&output));
    assert!(output.stdout.is_empty(), "stdout: {}", stdout(&output));
    assert_eq!(
        stderr(&output).trim_end(),
        "pi: the final response does not match --output-schema:\n  tags: must have required properties tags"
    );
    assert_eq!(requests.lock().unwrap().len(), 2);

    let (url, requests) = mock_server(vec![text_body("not json")]).await;
    setup(root.path(), &url);
    let output = run(root.path(), &["--output-retries", "0", "-p", "Describe pi"]).await;
    assert_eq!(output.status.code(), Some(4), "stderr: {}", stderr(&output));
    assert!(stderr(&output).contains("\n  the response is not valid JSON: expected"), "{}", stderr(&output));
    assert_eq!(requests.lock().unwrap().len(), 1, "no corrective prompt with --output-retries 0");
}

#[tokio::test(flavor = "multi_thread")]
async fn json_mode_ends_with_an_output_event() {
    let root = tempfile::tempdir().unwrap();
    let (url, _) = mock_server(vec![text_body("oops"), text_body("{\"name\": \"pi\", \"tags\": []}")]).await;
    setup(root.path(), &url);
    write_schema(root.path());
    let output = run(root.path(), &["--mode", "json", "-p", "Describe pi"]).await;
    assert_eq!(output.status.code(), Some(0), "stderr: {}", stderr(&output));
    let events = events(&output);
    assert_eq!(events.last().unwrap(), &json!({"type": "output", "value": {"name": "pi", "tags": []}}));
    let ends = events.iter().filter(|event| event["type"] == "agent_end").count();
    assert_eq!(ends, 2, "the corrective prompt runs like any other");
}

#[tokio::test(flavor = "multi_thread")]
async fn rejects_bad_schemas_and_modes_before_starting() {
    let root = tempfile::tempdir().unwrap();
    setup(root.path(), "http://127.0.0.1:9/v1");
    std::fs::write(root.path().join("work/broken.json"), "{\"type\": \"object\",}").unwrap();
    std::fs::write(root.path().join("work/remote.json"), "{\"$ref\": \"https://example.com/s.json\"}").unwrap();
    write_schema(root.path());
    for (flags, error) in [
        (vec!["--output-schema", "broken.json", "-p", "hi"], "Error: --output-schema: broken.json is not valid JSON:"),
        (
            vec!["--output-schema", "remote.json", "-p", "hi"],
            "Error: --output-schema: remote.json is not a supported JSON Schema:\n  #/$ref:",
        ),
        (vec!["--output-schema", "missing.json", "-p", "hi"], "Error: --output-schema: cannot read "),
        (vec!["--output-schema", "schema.json", "--mode", "rpc"], "Error: --output-schema requires print mode"),
        (vec!["--output-schema", "schema.json", "--mode", "acp"], "Error: --output-schema requires print mode"),
        (vec!["--output-retries", "2", "-p", "hi"], "Error: --output-retries requires --output-schema"),
    ] {
        let output = pi_async(root.path(), &flags).await;
        assert_eq!(output.status.code(), Some(1), "{flags:?}: {}", stderr(&output));
        assert!(stderr(&output).starts_with(error), "{flags:?}: {}", stderr(&output));
        assert!(output.stdout.is_empty());
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_extension_that_registers_the_flag_keeps_it() {
    let root = tempfile::tempdir().unwrap();
    let (url, requests) = mock_server(vec![text_body("Plain answer")]).await;
    setup(root.path(), &url);
    write_schema(root.path());
    let source = r#"import { writeFileSync } from "node:fs";
export default function (pi) {
	pi.registerFlag("output-schema", { type: "string" });
	pi.on("session_start", () => writeFileSync("flag.txt", String(pi.getFlag("output-schema"))));
}
"#;
    std::fs::write(root.path().join("schema-flag.ts"), source).unwrap();
    let extension = root.path().join("schema-flag.ts").display().to_string();
    let output = run(root.path(), &["-e", &extension, "-p", "Describe pi"]).await;
    assert_eq!(output.status.code(), Some(0), "stderr: {}", stderr(&output));
    assert_eq!(stdout(&output).trim(), "Plain answer", "the built-in check is off");
    assert_eq!(std::fs::read_to_string(root.path().join("work/flag.txt")).unwrap(), "schema.json");
    let requests = requests.lock().unwrap().clone();
    let system = requests[0]["messages"][0]["content"].as_str().unwrap();
    assert!(!system.contains("Your final message must be only a JSON value"), "{system}");
}
