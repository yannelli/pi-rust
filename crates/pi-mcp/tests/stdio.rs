//! Stdio transport tests against the fake server binary, ported from `packages/mcp/test/stdio.test.ts`.

mod common;

use std::sync::{Arc, Mutex};
#[cfg(unix)]
use std::time::{Duration, Instant};

use pi_mcp::{ClientState, McpClient, McpClientOptions, RequestOptions, StdioTransport, StdioTransportOptions};
use serde_json::json;

const SERVER: &str = env!("CARGO_BIN_EXE_mcp_fake_server");

#[tokio::test]
async fn connects_to_a_newline_delimited_server_and_captures_stderr() {
    let chunks = Arc::new(Mutex::new(String::new()));
    let sink = chunks.clone();
    let mut options = StdioTransportOptions::new(SERVER, Vec::new());
    options.on_stderr = Some(Arc::new(move |chunk: &str| sink.lock().unwrap().push_str(chunk)));
    let transport = Arc::new(StdioTransport::new(options));
    let client = McpClient::new(McpClientOptions::new("stdio-test", "1.0.0"));
    client.connect(transport.clone()).await.unwrap();
    let tools = client.list_tools(RequestOptions::default()).await.unwrap();
    assert_eq!(tools[0].name, "echo");
    let result = client.call_tool("echo", Some(json!({ "text": "hello" })), RequestOptions::default()).await.unwrap();
    assert_eq!(serde_json::to_value(result).unwrap(), json!({ "content": [{ "type": "text", "text": "hello" }] }));
    assert!(transport.pid().is_some());
    common::wait_until(|| transport.stderr_text().contains("stdio fixture ready")).await;
    assert!(chunks.lock().unwrap().contains("stdio fixture ready"));
    client.close().await;
    assert_eq!(client.connection_state(), ClientState::Closed);
}

#[tokio::test]
async fn servers_see_the_child_process_environment_overlay() {
    pi_ai::env::set_child_process_env("PI_MCP_STDIO_OVERLAY_TEST", "from-overlay");
    let transport = Arc::new(StdioTransport::new(StdioTransportOptions::new(SERVER, Vec::new())));
    let client = McpClient::new(McpClientOptions::new("stdio-test", "1.0.0"));
    client.connect(transport).await.unwrap();
    let arguments = json!({ "name": "PI_MCP_STDIO_OVERLAY_TEST" });
    let result = client.call_tool("env", Some(arguments), RequestOptions::default()).await.unwrap();
    let text = serde_json::to_value(result).unwrap()["content"][0]["text"].clone();
    assert_eq!(text, json!("from-overlay"));
    client.close().await;
}

#[tokio::test]
async fn reports_a_missing_command_like_node() {
    let transport = Arc::new(StdioTransport::new(StdioTransportOptions::new("pi-test-missing-mcp-server", Vec::new())));
    let client = McpClient::new(McpClientOptions::new("stdio-test", "1.0.0"));
    let error = client.connect(transport).await.unwrap_err();
    assert_eq!(error.to_string(), "spawn pi-test-missing-mcp-server ENOENT");
}

#[cfg(unix)]
#[tokio::test]
async fn kills_a_server_that_ignores_shutdown_including_its_children() {
    let mut options = StdioTransportOptions::new(SERVER, vec!["--stubborn".into()]);
    options.close_timeout = Duration::from_millis(100);
    let transport = Arc::new(StdioTransport::new(options));
    let client = McpClient::new(McpClientOptions::new("stdio-test", "1.0.0"));
    client.connect(transport.clone()).await.unwrap();
    common::wait_until(|| transport.stderr_text().contains("grandchild ")).await;
    let grandchild: i32 = transport
        .stderr_text()
        .split("grandchild ")
        .nth(1)
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .parse()
        .unwrap();

    let started = Instant::now();
    client.close().await;
    assert!(started.elapsed() < Duration::from_secs(5));
    // Decide once per poll: a killed process passes through zombie (Z) and dead (X) before /proc drops it.
    let mut alive = true;
    for _ in 0..100 {
        alive = is_running(grandchild);
        if !alive {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    if cfg!(target_os = "linux") {
        assert!(!alive, "grandchild {grandchild} survived");
    }
}

#[cfg(unix)]
fn is_running(pid: i32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .map(|stat| !matches!(stat.rsplit(')').next().unwrap_or_default().trim_start().chars().next(), Some('Z' | 'X')))
        .unwrap_or(false)
}
