//! `--max-turns` and `--max-cost` through the `pi` binary against a local mock server.

mod cli_support;

use serde_json::json;

use cli_support::{events, mock_server, pi_async, setup, text_body, tool_call_body};

fn echo_call(id: &str) -> String {
    tool_call_body(id, "bash", r#"{"command":"echo turn"}"#)
}

#[tokio::test(flavor = "multi_thread")]
async fn print_mode_exits_3_with_a_message_and_the_session_continues() {
    let root = tempfile::tempdir().unwrap();
    let (url, requests) = mock_server(vec![echo_call("c1"), echo_call("c2"), text_body("Resumed fine")]).await;
    setup(root.path(), &url);
    let output = pi_async(root.path(), &["--model", "mock/mock-model", "--max-turns", "2", "-p", "Loop"]).await;
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(3), "stderr: {stderr}");
    assert!(stderr.contains("pi: stopped after 2 turns (--max-turns)"), "stderr: {stderr}");
    assert!(output.stdout.is_empty());
    assert_eq!(requests.lock().unwrap().len(), 2);

    let output = pi_async(root.path(), &["--model", "mock/mock-model", "-c", "-p", "Go on"]).await;
    assert!(output.status.success(), "stderr: {}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "Resumed fine");
    let requests = requests.lock().unwrap().clone();
    let roles: Vec<&str> =
        requests[2]["messages"].as_array().unwrap().iter().map(|message| message["role"].as_str().unwrap()).collect();
    assert_eq!(roles[roles.len() - 4..], ["tool", "assistant", "tool", "user"], "the stopped run resumes as stored");
}

#[tokio::test(flavor = "multi_thread")]
async fn json_mode_emits_a_run_limit_event_for_the_cost_limit() {
    let root = tempfile::tempdir().unwrap();
    let (url, requests) = mock_server(vec![echo_call("c1"), echo_call("c2"), echo_call("c3")]).await;
    setup(root.path(), &url);
    // 20 input and 5 output tokens at $10,000 per million: $0.25 per response.
    let models = json!({"providers": {"mock": {"baseUrl": url, "api": "openai-completions", "apiKey": "secret-key",
        "models": [{"id": "mock-model", "contextWindow": 100000, "maxTokens": 4000,
            "cost": {"input": 10000, "output": 10000, "cacheRead": 0, "cacheWrite": 0}}]}}});
    std::fs::write(root.path().join("agent/models.json"), models.to_string()).unwrap();
    let args = ["--model", "mock/mock-model", "--mode", "json", "--max-cost=0.3", "-p", "Loop"];
    let output = pi_async(root.path(), &args).await;
    assert_eq!(output.status.code(), Some(3), "stderr: {}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(requests.lock().unwrap().len(), 2);
    let events = events(&output);
    let limit = events.iter().find(|event| event["type"] == "run_limit").expect("a run_limit event");
    assert_eq!(
        (limit["limit"].as_str(), limit["max"].as_f64(), limit["turns"].as_u64()),
        (Some("maxCost"), Some(0.3), Some(2))
    );
    assert!((limit["cost"].as_f64().unwrap() - 0.5).abs() < 1e-9);
    let kinds: Vec<&str> = events.iter().map(|event| event["type"].as_str().unwrap()).collect();
    assert_eq!(kinds[kinds.len() - 3..], ["run_limit", "agent_end", "agent_settled"]);
    assert_eq!(events.last().unwrap()["aborted"], true);
}

#[tokio::test(flavor = "multi_thread")]
async fn rejects_invalid_limits_before_starting() {
    let root = tempfile::tempdir().unwrap();
    setup(root.path(), "http://127.0.0.1:9/v1");
    for (flags, error) in [
        (["--max-turns", "0"], "Error: --max-turns requires a positive integer, got \"0\""),
        (["--max-cost", "-1"], "Error: --max-cost requires a non-negative number of US dollars, got \"-1\""),
    ] {
        let output = pi_async(root.path(), &[flags[0], flags[1], "-p", "hi"]).await;
        assert_eq!(output.status.code(), Some(1));
        assert_eq!(String::from_utf8_lossy(&output.stderr).trim(), error);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_extension_that_registers_the_flag_keeps_it() {
    let root = tempfile::tempdir().unwrap();
    let (url, requests) = mock_server(vec![echo_call("c1"), text_body("Done")]).await;
    setup(root.path(), &url);
    let source = r#"import { writeFileSync } from "node:fs";
export default function (pi) {
	pi.registerFlag("max-turns", { type: "string" });
	pi.on("session_start", () => writeFileSync("flag.txt", String(pi.getFlag("max-turns"))));
}
"#;
    std::fs::write(root.path().join("limits.ts"), source).unwrap();
    let extension = root.path().join("limits.ts").display().to_string();
    let args = ["--model", "mock/mock-model", "-e", &extension, "--max-turns", "1", "-p", "Loop"];
    let output = pi_async(root.path(), &args).await;
    assert!(output.status.success(), "stderr: {}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "Done", "the built-in limit is off");
    assert_eq!(requests.lock().unwrap().len(), 2);
    assert_eq!(std::fs::read_to_string(root.path().join("work/flag.txt")).unwrap(), "1");
}
