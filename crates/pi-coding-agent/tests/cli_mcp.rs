//! `pi mcp` through the binary: servers added with `pi mcp add` connect in `pi mcp list` and
//! give print mode their tools, `--no-mcp` leaves them out, and `pi mcp remove` drops them.

mod cli_support;

#[cfg(unix)]
mod unix {
    use serde_json::Value;

    use super::cli_support::{mock_server, pi_async, setup, text_body, tool_call_body};

    /// A stdio MCP server with one tool, `echo`, as a POSIX shell script.
    const SERVER: &str = r#"while IFS= read -r line; do
  id=$(printf '%s' "$line" | grep -o '"id":[0-9]*' | head -n 1 | cut -d: -f2)
  [ -z "$id" ] && continue
  case "$line" in
    *'"method":"initialize"'*) result='{"protocolVersion":"2025-06-18","capabilities":{"tools":{}},"serverInfo":{"name":"sh","version":"1"}}' ;;
    *'"method":"tools/list"'*) result='{"tools":[{"name":"echo","description":"Echo text.","inputSchema":{"type":"object","properties":{"text":{"type":"string"}}}}]}' ;;
    *'"method":"tools/call"'*) text=$(printf '%s' "$line" | sed -n 's/.*"text":"\([^"]*\)".*/\1/p'); result="{\"content\":[{\"type\":\"text\",\"text\":\"echo: $text\"}]}" ;;
    *) result='{}' ;;
  esac
  printf '{"jsonrpc":"2.0","id":%s,"result":%s}\n' "$id" "$result"
done
"#;

    fn stdout(output: &std::process::Output) -> String {
        String::from_utf8_lossy(&output.stdout).to_string()
    }

    fn tool_names(request: &Value) -> Vec<String> {
        let tools = request["tools"].as_array().cloned().unwrap_or_default();
        tools.iter().map(|tool| tool["function"]["name"].as_str().unwrap_or_default().to_string()).collect()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn mcp_subcommands_configure_the_servers_print_mode_uses() {
        let root = tempfile::tempdir().unwrap();
        let (url, requests) = mock_server(vec![
            tool_call_body("call_1", "mcp__docs__echo", r#"{"text":"hi"}"#),
            text_body("Done"),
            text_body("Plain"),
        ])
        .await;
        setup(root.path(), &url);
        let script = root.path().join("server.sh");
        std::fs::write(&script, SERVER).unwrap();
        let script = script.display().to_string();

        let add = pi_async(root.path(), &["mcp", "add", "docs", "--exposure", "direct", "--", "sh", &script]).await;
        assert!(add.status.success(), "stderr: {}", String::from_utf8_lossy(&add.stderr));
        assert!(stdout(&add).starts_with("Added global MCP server \"docs\""));

        let list = pi_async(root.path(), &["mcp", "list", "--json"]).await;
        assert!(list.status.success(), "{}", stdout(&list));
        let listed: Value = serde_json::from_str(&stdout(&list)).unwrap();
        let server = &listed["servers"][0];
        assert_eq!((server["name"].as_str(), server["state"].as_str()), (Some("docs"), Some("connected")));
        assert_eq!(server["tools"], serde_json::json!(["echo"]));

        let run = pi_async(root.path(), &["--model", "mock/mock-model", "-p", "Echo hi"]).await;
        assert!(run.status.success(), "stderr: {}", String::from_utf8_lossy(&run.stderr));
        assert_eq!(stdout(&run).trim(), "Done");
        let first = requests.lock().unwrap().clone();
        assert!(tool_names(&first[0]).contains(&"mcp__docs__echo".to_string()));
        let messages = first[1]["messages"].as_array().unwrap();
        let result = messages.iter().find(|message| message["role"] == "tool").unwrap();
        assert!(result["content"].as_str().unwrap().contains("echo: hi"), "{result}");

        let plain = pi_async(root.path(), &["--no-mcp", "--model", "mock/mock-model", "-p", "Hello"]).await;
        assert!(plain.status.success(), "stderr: {}", String::from_utf8_lossy(&plain.stderr));
        let third = requests.lock().unwrap()[2].clone();
        assert!(!tool_names(&third).iter().any(|name| name.starts_with("mcp__")));

        let remove = pi_async(root.path(), &["mcp", "remove", "docs"]).await;
        assert!(remove.status.success());
        assert!(stdout(&remove).starts_with("Removed global MCP server \"docs\""));
        let empty = pi_async(root.path(), &["mcp", "list", "--json"]).await;
        let listed: Value = serde_json::from_str(&stdout(&empty)).unwrap();
        assert_eq!(listed["servers"], serde_json::json!([]));
    }
}
