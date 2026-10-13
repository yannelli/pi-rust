//! A small stdio MCP server for tests. By default it answers `initialize`, `tools/list`, `tools/call`,
//! and `ping`; `--stubborn` ignores stdin EOF and SIGTERM and starts a grandchild that does the same.

use std::io::{BufRead, Write};

use serde_json::{Value, json};

fn reply(id: &Value, result: Value) {
    write(&json!({ "jsonrpc": "2.0", "id": id, "result": result }));
}

fn write(message: &Value) {
    let mut stdout = std::io::stdout().lock();
    let _ = writeln!(stdout, "{message}");
    let _ = stdout.flush();
}

fn initialize_result(name: &str, tools: bool) -> Value {
    let capabilities = if tools { json!({ "tools": {} }) } else { json!({}) };
    json!({ "protocolVersion": "2025-06-18", "capabilities": capabilities, "serverInfo": { "name": name, "version": "1.0.0" } })
}

fn serve() {
    eprintln!("stdio fixture ready");
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        let Ok(message) = serde_json::from_str::<Value>(&line) else { continue };
        let Some(id) = message.get("id").cloned() else { continue };
        let method = message.get("method").and_then(Value::as_str).unwrap_or_default();
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        match method {
            "initialize" => reply(&id, initialize_result("stdio-fixture", true)),
            "tools/list" => reply(
                &id,
                json!({ "tools": [
                    { "name": "echo", "inputSchema": { "type": "object" } },
                    { "name": "cwd", "inputSchema": { "type": "object" } },
                    { "name": "log", "inputSchema": { "type": "object" } }
                ] }),
            ),
            "tools/call" => {
                let arguments = &params["arguments"];
                let text = match params["name"].as_str().unwrap_or_default() {
                    "cwd" => std::env::current_dir().map(|dir| dir.display().to_string()).unwrap_or_default(),
                    "log" => {
                        let data = arguments.get("data").cloned().unwrap_or(json!("log line"));
                        write(
                            &json!({ "jsonrpc": "2.0", "method": "notifications/message", "params": { "level": "info", "data": data } }),
                        );
                        "logged".into()
                    }
                    _ => match &arguments["text"] {
                        Value::String(text) => text.clone(),
                        Value::Null => "undefined".into(),
                        other => other.to_string(),
                    },
                };
                reply(&id, json!({ "content": [{ "type": "text", "text": text }] }));
            }
            "ping" => reply(&id, json!({})),
            _ => write(&json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32601, "message": "not found" } })),
        }
    }
}

fn ignore_sigterm_forever() -> ! {
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
    runtime.block_on(async {
        #[cfg(unix)]
        let _signal = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate());
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
    })
}

fn stubborn() {
    #[cfg(unix)]
    let _sigterm = {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
        let signal =
            runtime.block_on(async { tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) });
        (runtime, signal)
    };
    let exe = std::env::current_exe().expect("current exe");
    let grandchild = std::process::Command::new(exe)
        .arg("--grandchild")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn grandchild");
    eprintln!("grandchild {}", grandchild.id());
    // The grandchild outlives this process on purpose.
    std::mem::forget(grandchild);
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        let Ok(message) = serde_json::from_str::<Value>(&line) else { continue };
        if message.get("method").and_then(Value::as_str) == Some("initialize") {
            reply(&message["id"], initialize_result("stubborn-fixture", false));
        }
    }
    ignore_sigterm_forever();
}

fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("--stubborn") => stubborn(),
        Some("--grandchild") => ignore_sigterm_forever(),
        _ => serve(),
    }
}
