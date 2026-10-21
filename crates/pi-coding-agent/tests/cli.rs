//! End-to-end runs of the `pi` binary against a local OpenAI-compatible server.

mod cli_support;

use serde_json::{Value, json};

use cli_support::{events, isolate, mock_server, pi, pi_async, setup, text_body, tool_call_body, write_settings};

#[tokio::test(flavor = "multi_thread")]
async fn print_mode_runs_tools_and_saves_the_session() {
    let root = tempfile::tempdir().unwrap();
    let (url, requests) = mock_server(vec![
        tool_call_body("call_1", "bash", r#"{"command":"echo from-bash > out.txt && cat out.txt"}"#),
        text_body("Done: from-bash"),
    ])
    .await;
    setup(root.path(), &url);
    let output = tokio::task::spawn_blocking({
        let root = root.path().to_path_buf();
        move || pi(&root, &["--model", "mock/mock-model", "-p", "Run the command"], None)
    })
    .await
    .unwrap();
    assert!(output.status.success(), "stderr: {}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "Done: from-bash");
    assert_eq!(std::fs::read_to_string(root.path().join("work/out.txt")).unwrap().trim(), "from-bash");

    let requests = requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0]["model"], "mock-model");
    assert_eq!(requests[0]["stream"], true);
    let tool_names: Vec<&str> = requests[0]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["function"]["name"].as_str().unwrap())
        .collect();
    assert_eq!(tool_names, ["read", "bash", "edit", "write"]);
    let tool_message =
        requests[1]["messages"].as_array().unwrap().iter().find(|message| message["role"] == "tool").unwrap();
    assert!(tool_message["content"].as_str().unwrap().contains("from-bash"));

    let sessions = root.path().join("agent/sessions");
    let dir = std::fs::read_dir(&sessions).unwrap().next().unwrap().unwrap().path();
    let file = std::fs::read_dir(&dir).unwrap().next().unwrap().unwrap().path();
    let lines: Vec<Value> =
        std::fs::read_to_string(&file).unwrap().lines().map(|line| serde_json::from_str(line).unwrap()).collect();
    assert_eq!(lines[0]["type"], "session");
    assert!(lines.iter().any(|line| line["type"] == "model_change" && line["modelId"] == "mock-model"));
    let messages: Vec<&Value> = lines.iter().filter(|line| line["type"] == "message").collect();
    assert_eq!(messages.len(), 5);
    assert_eq!(messages[0]["message"]["role"], "system", "the system prompt leads the transcript");
}

#[tokio::test(flavor = "multi_thread")]
async fn json_mode_streams_events_and_reads_stdin_and_files() {
    let root = tempfile::tempdir().unwrap();
    let (url, requests) = mock_server(vec![text_body("Hello")]).await;
    setup(root.path(), &url);
    std::fs::write(root.path().join("work/context.txt"), "file contents").unwrap();
    let output = tokio::task::spawn_blocking({
        let root = root.path().to_path_buf();
        move || {
            pi(
                &root,
                &["--model", "mock-model", "--mode", "json", "--no-session", "-nt", "@context.txt", "-p", "Summarize"],
                Some("piped text"),
            )
        }
    })
    .await
    .unwrap();
    assert!(output.status.success(), "stderr: {}", String::from_utf8_lossy(&output.stderr));
    let events: Vec<Value> =
        String::from_utf8_lossy(&output.stdout).lines().map(|line| serde_json::from_str(line).unwrap()).collect();
    let kinds: Vec<&str> = events.iter().map(|event| event["type"].as_str().unwrap()).collect();
    assert_eq!(kinds[..2], ["session", "agent_start"], "the session header comes first");
    assert_eq!(events[0]["version"], 3);
    assert_eq!(kinds[kinds.len() - 2..], ["agent_end", "agent_settled"]);
    assert!(kinds.contains(&"message_update"));
    let request = &requests.lock().unwrap()[0];
    assert!(request.get("tools").is_none());
    let user = request["messages"].as_array().unwrap().iter().find(|message| message["role"] == "user").unwrap();
    let text = user["content"].to_string();
    assert!(
        text.contains("context.txt")
            && text.contains("file contents")
            && text.contains("piped text")
            && text.contains("Summarize")
    );
    assert!(!root.path().join("agent/sessions").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn reports_provider_errors_with_a_failing_exit_code() {
    let root = tempfile::tempdir().unwrap();
    let (url, _) = mock_server(vec![r#"ERROR{"error":{"message":"upstream exploded"}}"#.to_string()]).await;
    setup(root.path(), &url);
    write_settings(root.path(), json!({"retry": {"enabled": false}}));
    let output = tokio::task::spawn_blocking({
        let root = root.path().to_path_buf();
        move || pi(&root, &["--model", "mock/mock-model", "--no-session", "-p", "hi"], None)
    })
    .await
    .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("500") && stderr.contains("upstream exploded"), "{stderr}");
}

#[test]
fn rejects_unknown_flags_and_lists_models() {
    let root = tempfile::tempdir().unwrap();
    setup(root.path(), "http://127.0.0.1:9/v1");
    let output = pi(root.path(), &["--bogus"], None);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(String::from_utf8_lossy(&output.stderr).trim(), "Error: Unknown option: --bogus");
    let output = pi(root.path(), &["--list-models", "mock"], None);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = stdout.lines().collect();
    assert!(lines[0].starts_with("provider  model") && lines[0].contains("max-out"), "{stdout}");
    assert!(lines[1].starts_with("mock      mock-model  100K"), "{stdout}");
}

#[test]
fn metadata_commands_run_migrations_and_keep_stdout_free_outside_plain_mode() {
    let root = tempfile::tempdir().unwrap();
    setup(root.path(), "http://127.0.0.1:9/v1");
    std::fs::create_dir_all(root.path().join("agent/commands")).unwrap();
    let output = pi(root.path(), &["--help"], None);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Migrated Global commands/ → prompts/"), "{stdout}");
    assert!(stdout.contains(" - AI coding assistant"), "{stdout}");
    assert!(root.path().join("agent/prompts").is_dir());

    let output = pi(root.path(), &["-p", "--help"], None);
    assert!(output.stdout.is_empty(), "{}", String::from_utf8_lossy(&output.stdout));
    assert!(String::from_utf8_lossy(&output.stderr).contains(" - AI coding assistant"));

    let output = pi(root.path(), &["--mode", "json", "--list-models", "mock"], None);
    assert!(output.stdout.is_empty(), "{}", String::from_utf8_lossy(&output.stdout));
    assert!(String::from_utf8_lossy(&output.stderr).contains("mock-model"));
}

#[tokio::test(flavor = "multi_thread")]
async fn retries_transient_errors_and_streams_delta_events() {
    let root = tempfile::tempdir().unwrap();
    let error = r#"ERROR{"error":{"message":"overloaded"}}"#.to_string();
    let (url, requests) = mock_server(vec![error, text_body("Recovered")]).await;
    setup(root.path(), &url);
    write_settings(root.path(), json!({"retry": {"baseDelayMs": 1, "maxRetries": 2}}));
    let output = pi_async(root.path(), &["--model", "mock-model", "--mode", "json", "--no-session", "-nt", "hi"]).await;
    assert!(output.status.success(), "stderr: {}", String::from_utf8_lossy(&output.stderr));
    let events = events(&output);
    let kinds: Vec<&str> = events.iter().map(|event| event["type"].as_str().unwrap()).collect();
    let retry_start = kinds.iter().position(|kind| *kind == "auto_retry_start").unwrap();
    assert_eq!(events[retry_start]["attempt"], 1);
    assert_eq!(events[retry_start]["maxAttempts"], 2);
    assert_eq!(events[retry_start]["delayMs"], 1);
    let ends: Vec<&Value> = events.iter().filter(|event| event["type"] == "agent_end").collect();
    assert_eq!((ends[0]["willRetry"].as_bool(), ends[1]["willRetry"].as_bool()), (Some(true), Some(false)));
    let retry_end = events.iter().find(|event| event["type"] == "auto_retry_end").unwrap();
    assert_eq!((retry_end["success"].as_bool(), retry_end["attempt"].as_u64()), (Some(true), Some(1)));
    for update in events.iter().filter(|event| event["type"] == "message_update") {
        assert!(update.get("message").is_none() && update["assistantMessageEvent"].get("partial").is_none());
        assert!(update["usage"].is_object());
    }
    let requests = requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 2);
    let roles: Vec<&str> =
        requests[1]["messages"].as_array().unwrap().iter().map(|message| message["role"].as_str().unwrap()).collect();
    assert!(!roles.contains(&"assistant"), "the failed response is not sent again: {roles:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn redirected_stdout_runs_print_mode_with_prompt_flags_and_markers() {
    let root = tempfile::tempdir().unwrap();
    let (url, requests) = mock_server(vec![
        tool_call_body("call_1", "bash", r#"{"command":"echo $PI_CODING_AGENT $AI_AGENT"}"#),
        text_body("All done"),
    ])
    .await;
    setup(root.path(), &url);
    write_settings(root.path(), json!({"defaultTools": ["+grep"], "shellCommandPrefix": "export EXTRA=1"}));
    std::fs::write(root.path().join("work/system.txt"), "System from file.").unwrap();
    let args = [
        "--model",
        "mock/mock-model",
        "--no-session",
        "--system-prompt",
        "system.txt",
        "--append-system-prompt",
        "First addition.",
        "--append-system-prompt",
        "Second addition.",
        "-xt",
        "edit",
        "Go",
    ];
    let output = pi_async(root.path(), &args).await;
    assert!(output.status.success(), "stderr: {}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout), "All done\n");
    let requests = requests.lock().unwrap().clone();
    let first = requests[0]["messages"].to_string();
    assert!(first.contains("System from file.") && first.contains("First addition.\\n\\nSecond addition."), "{first}");
    let tools: Vec<&str> = requests[0]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["function"]["name"].as_str().unwrap())
        .collect();
    assert_eq!(tools, ["read", "bash", "write", "grep"]);
    assert!(requests[1]["messages"].to_string().contains("true pi"));
}

#[tokio::test(flavor = "multi_thread")]
async fn startup_errors_exit_with_status_one() {
    let root = tempfile::tempdir().unwrap();
    setup(root.path(), "http://127.0.0.1:9/v1");
    let output = pi_async(root.path(), &["--provider", "mock", "hi"]).await;
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--provider requires --model (for example: --provider mock --model <pattern>)"),
        "{stderr}"
    );
    let output = pi_async(root.path(), &["--model", "nothing-like-this", "hi"]).await;
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("Model \"nothing-like-this\" not found"));
    let output = pi_async(root.path(), &["--mode", "xml"]).await;
    assert_eq!(output.status.code(), Some(1));
}

#[test]
fn auth_commands_print_and_check_credentials() {
    let root = tempfile::tempdir().unwrap();
    setup(root.path(), "http://127.0.0.1:9/v1");
    let output = pi(root.path(), &["auth", "print-api-key", "--provider", "mock"], None);
    assert_eq!((output.status.code(), String::from_utf8_lossy(&output.stdout).as_ref()), (Some(0), "secret-key\n"));
    let output = pi(root.path(), &["auth", "check", "--model", "mock-model", "--json"], None);
    assert_eq!(output.status.code(), Some(0));
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result, json!({"status": "ready", "provider": "mock", "authType": "api_key"}));
    let output = pi(root.path(), &["auth", "check"], None);
    assert_eq!(output.status.code(), Some(2));
    let output = pi(root.path(), &["auth", "print-api-key", "--json"], None);
    assert_eq!(output.status.code(), Some(1));
}

/// Upstream `print-mode.ts` signal handlers: SIGTERM kills the running bash command's process
/// group, disposes the runtime, and exits 143. Startup migrations ran before the prompt.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn print_mode_exits_143_on_sigterm_and_kills_detached_children() {
    let root = tempfile::tempdir().unwrap();
    let command = r#"{"command":"echo $$ > pid.txt; sleep 60"}"#;
    let (url, _) = mock_server(vec![tool_call_body("call_1", "bash", command), text_body("unused")]).await;
    setup(root.path(), &url);
    std::fs::write(root.path().join("agent/oauth.json"), r#"{"legacy": {"access": "a"}}"#).unwrap();
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_pi"));
    isolate(command.as_std_mut());
    let mut child = command
        .args(["--model", "mock/mock-model", "-p", "Run it"])
        .current_dir(root.path().join("work"))
        .env("PI_CODING_AGENT_DIR", root.path().join("agent"))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let pid_file = root.path().join("work/pid.txt");
    let shell_pid = tokio::time::timeout(std::time::Duration::from_secs(20), async {
        loop {
            if let Some(pid) = std::fs::read_to_string(&pid_file).ok().filter(|text| text.ends_with('\n')) {
                return pid.trim().to_string();
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the bash command started");
    let auth: Value =
        serde_json::from_str(&std::fs::read_to_string(root.path().join("agent/auth.json")).unwrap()).unwrap();
    assert_eq!(auth["legacy"], json!({"type": "oauth", "access": "a"}));

    let pid = child.id().unwrap().to_string();
    assert!(tokio::process::Command::new("kill").args(["-TERM", &pid]).status().await.unwrap().success());
    let status = tokio::time::timeout(std::time::Duration::from_secs(20), child.wait()).await.unwrap().unwrap();
    assert_eq!(status.code(), Some(143));
    let mut alive = true;
    for _ in 0..40 {
        alive = std::process::Command::new("kill").args(["-0", &shell_pid]).status().unwrap().success();
        if !alive {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(!alive, "the bash command survived");
}
