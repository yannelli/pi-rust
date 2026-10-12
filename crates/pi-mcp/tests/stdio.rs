//! Stdio transport tests against the fake server binary, ported from `packages/mcp/test/stdio.test.ts`.

mod common;

use std::sync::{Arc, Mutex};
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
    let alive = || std::path::Path::new(&format!("/proc/{grandchild}")).exists() && !is_zombie(grandchild);
    for _ in 0..100 {
        if !alive() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    if cfg!(target_os = "linux") {
        assert!(!alive(), "grandchild {grandchild} survived");
    }
}

#[cfg(unix)]
fn is_zombie(pid: i32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .map(|stat| stat.rsplit(')').next().unwrap_or_default().trim_start().starts_with('Z'))
        .unwrap_or(true)
}
