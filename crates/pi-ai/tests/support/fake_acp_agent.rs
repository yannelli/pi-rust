//! A scripted ACP agent for the `acp` provider tests. The prompt text picks the script: `tools`
//! edits a file with permission, `wait` runs until `session/cancel`, anything else echoes.
//! `own-edit` edits files with the agent's own tools, `probe` tries what a read-only session
//! refuses, `mcp` lists the MCP servers, `proxy-call` calls a tool of pi's MCP proxy,
//! `wait-child` starts a `sleep` child and waits for `session/cancel`, and `/name` echoes a
//! slash command. `FAKE_ACP_CONFIG` picks the config options (`fake_acp_config.rs`).

#[path = "fake_acp_config.rs"]
mod fake_acp_config;

use std::io::{BufRead, Read, Write};

use serde_json::{Value, json};

struct Agent {
    lines: std::io::Lines<std::io::StdinLock<'static>>,
    next_id: u64,
    cancelled: bool,
    mode: String,
    model: String,
    turns: u32,
    mcp: Vec<Value>,
    variant: String,
    effort: String,
    /// The `session/new` requests so far.
    news: u32,
}

/// One MCP request to a streamable HTTP server, reading the first JSON-RPC reply of the stream.
fn mcp_post(url: &str, authorization: &str, body: &Value) -> Value {
    let rest = url.strip_prefix("http://").unwrap_or(url);
    let (address, path) = rest.split_once('/').unwrap_or((rest, ""));
    let Ok(mut stream) = std::net::TcpStream::connect(address) else { return Value::Null };
    let body = body.to_string();
    let request = format!(
        "POST /{path} HTTP/1.1\r\nhost: {address}\r\nauthorization: {authorization}\r\n\
         content-type: application/json\r\naccept: application/json, text/event-stream\r\n\
         content-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    let mut response = String::new();
    if stream.write_all(request.as_bytes()).is_err() || stream.read_to_string(&mut response).is_err() {
        return Value::Null;
    }
    let data = response.lines().find_map(|line| line.strip_prefix("data: "));
    data.and_then(|data| serde_json::from_str(data).ok()).unwrap_or_default()
}

fn send(message: &Value) {
    let mut stdout = std::io::stdout().lock();
    let _ = writeln!(stdout, "{message}");
    let _ = stdout.flush();
}

fn update(update: Value) {
    send(&json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "s1", "update": update}}));
}

fn chunk(kind: &str, text: &str) {
    update(json!({"sessionUpdate": kind, "content": {"type": "text", "text": text}}));
}

impl Agent {
    fn session(&self) -> Value {
        let modes = json!([{"id": "ask", "name": "Ask"}, {"id": "code", "name": "Code"}]);
        let mut session = json!({"sessionId": "s1", "modes": {"currentModeId": self.mode, "availableModes": modes}});
        match self.config() {
            Some(config) => session["configOptions"] = config,
            None => session["models"] = fake_acp_config::legacy_models(&self.model),
        }
        session
    }

    fn config(&self) -> Option<Value> {
        fake_acp_config::options(&self.variant, &self.model, &self.effort, &self.mode)
    }

    /// Sends a request to the client and reads until its reply; notes a `session/cancel` on the way.
    fn call(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        while let Some(Ok(line)) = self.lines.next() {
            let message: Value = serde_json::from_str(&line).unwrap_or_default();
            if message["method"] == "session/cancel" {
                self.cancelled = true;
            } else if message["id"] == id && message.get("method").is_none() {
                return message.get("result").cloned().unwrap_or_else(|| json!({"error": message["error"]}));
            }
        }
        Value::Null
    }

    fn tools(&mut self, cwd: &str) {
        let path = format!("{cwd}/a.txt");
        let call = json!({"toolCallId": "t1", "title": "Edit a.txt", "kind": "edit", "status": "pending"});
        update(
            json!({"sessionUpdate": "tool_call", "toolCallId": "t1", "title": "Edit a.txt", "kind": "edit", "status": "pending"}),
        );
        let options = json!([{"optionId": "yes", "name": "Allow", "kind": "allow_once"},
            {"optionId": "always", "name": "Always allow", "kind": "allow_always"},
            {"optionId": "no", "name": "Reject", "kind": "reject_once"}]);
        let answer =
            self.call("session/request_permission", json!({"sessionId": "s1", "toolCall": call, "options": options}));
        if answer.pointer("/outcome/optionId") != Some(&json!("yes")) {
            update(json!({"sessionUpdate": "tool_call_update", "toolCallId": "t1", "status": "failed"}));
            return;
        }
        let old = self.call("fs/read_text_file", json!({"sessionId": "s1", "path": path}));
        self.call("fs/write_text_file", json!({"sessionId": "s1", "path": path, "content": "new\n"}));
        let outside = self.call("fs/read_text_file", json!({"sessionId": "s1", "path": "/etc/hosts"}));
        let terminal =
            self.call("terminal/create", json!({"sessionId": "s1", "command": "echo", "args": ["hi from terminal"]}));
        let terminal_id = terminal["terminalId"].clone();
        self.call("terminal/wait_for_exit", json!({"sessionId": "s1", "terminalId": terminal_id}));
        let output = self.call("terminal/output", json!({"sessionId": "s1", "terminalId": terminal_id}));
        update(json!({"sessionUpdate": "tool_call_update", "toolCallId": "t1", "status": "completed", "content": [
            {"type": "diff", "path": path, "oldText": old["content"], "newText": "new\n"},
            {"type": "terminal", "terminalId": terminal_id}]}));
        self.call("terminal/release", json!({"sessionId": "s1", "terminalId": terminal_id}));
        update(
            json!({"sessionUpdate": "plan", "entries": [{"content": "Edit a.txt", "priority": "high", "status": "completed"}]}),
        );
        let rejected = outside.get("error").is_some();
        chunk("agent_message_chunk", &format!("exit={} outside-rejected={rejected}", output["exitStatus"]["exitCode"]));
    }

    /// Changes files with the agent's own tools: an edit it announces, and a shell command that
    /// names no file.
    fn own_edit(&mut self, cwd: &str) {
        let (edited, shell) = (format!("{cwd}/b.txt"), format!("{cwd}/c.txt"));
        update(json!({"sessionUpdate": "tool_call", "toolCallId": "e1", "title": "Edit b.txt", "kind": "edit",
            "status": "pending", "locations": [{"path": edited}]}));
        update(json!({"sessionUpdate": "tool_call", "toolCallId": "x1", "title": "Run sed", "kind": "execute",
            "status": "pending"}));
        let call = json!({"toolCallId": "e1", "title": "Edit b.txt", "kind": "edit"});
        let options = json!([{"optionId": "yes", "name": "Allow", "kind": "allow_once"}]);
        self.call("session/request_permission", json!({"sessionId": "s1", "toolCall": call, "options": options}));
        let _ = std::fs::write(&edited, "agent edit\n");
        let _ = std::fs::write(&shell, "shell edit\n");
        let _ = std::fs::remove_file(format!("{cwd}/gone.txt"));
        if let Ok(file) = std::fs::File::options().write(true).open(format!("{cwd}/same.txt")) {
            let modified = file.metadata().and_then(|meta| meta.modified());
            let _ = std::fs::write(format!("{cwd}/same.txt"), "XYZ\n");
            if let Ok(modified) = modified {
                let _ = file.set_modified(modified);
            }
        }
        for id in ["e1", "x1"] {
            update(json!({"sessionUpdate": "tool_call_update", "toolCallId": id, "status": "completed"}));
        }
    }

    /// Tries a write, a terminal, and a permission request, and reports what pi answered.
    fn probe(&mut self, cwd: &str) {
        let write = self.call(
            "fs/write_text_file",
            json!({"sessionId": "s1", "path": format!("{cwd}/probe.txt"),
            "content": "x"}),
        );
        let terminal = self.call("terminal/create", json!({"sessionId": "s1", "command": "echo", "args": []}));
        let call = json!({"toolCallId": "p1", "title": "Probe", "kind": "edit"});
        let options = json!([{"optionId": "yes", "name": "Allow", "kind": "allow_once"}]);
        let answer =
            self.call("session/request_permission", json!({"sessionId": "s1", "toolCall": call, "options": options}));
        let child = std::process::Command::new("sleep").arg("30").spawn().map(|child| child.id()).unwrap_or_default();
        chunk(
            "agent_message_chunk",
            &format!(
                "write-rejected={} terminal-rejected={} permission={} child={child} pid={}",
                write.get("error").is_some(),
                terminal.get("error").is_some(),
                answer["outcome"]["outcome"],
                std::process::id()
            ),
        );
    }

    /// Lists the tools of pi's MCP proxy and calls the first one.
    fn proxy_call(&self) {
        let Some(proxy) = self.mcp.iter().find(|server| server["name"] == "pi-mcp") else {
            return chunk("agent_message_chunk", "no proxy");
        };
        let (url, authorization) = (proxy["url"].as_str().unwrap_or_default(), &proxy["headers"][0]["value"]);
        let authorization = authorization.as_str().unwrap_or_default();
        let list = mcp_post(url, authorization, &json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}));
        let tools = list.pointer("/result/tools").and_then(Value::as_array).cloned().unwrap_or_default();
        let names: Vec<&str> = tools.iter().filter_map(|tool| tool["name"].as_str()).collect();
        let call = json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
            "params": {"name": names.first().copied().unwrap_or_default(), "arguments": {"query": "rust"}}});
        let result = mcp_post(url, authorization, &call);
        let text = result.pointer("/result/content/0/text").and_then(Value::as_str).unwrap_or_default();
        chunk("agent_message_chunk", &format!("proxy-tools={} proxy-result={text}", names.join(",")));
    }

    fn prompt(&mut self, params: &Value, cwd: &str) -> Value {
        let blocks = params["prompt"].as_array().cloned().unwrap_or_default();
        let types: Vec<&str> = blocks.iter().filter_map(|block| block["type"].as_str()).collect();
        let text = blocks.iter().rev().find_map(|block| block["text"].as_str()).unwrap_or_default().to_string();
        self.turns += 1;
        chunk("agent_thought_chunk", "thinking");
        let (mode, model, turns, effort, news) = (&self.mode, &self.model, self.turns, &self.effort, self.news);
        let greeting =
            format!("Hello [{}] mode={mode} model={model} turn={turns} effort={effort} news={news} ", types.join(","));
        chunk("agent_message_chunk", &greeting);
        match text.as_str() {
            "own-edit" => self.own_edit(cwd),
            "probe" => self.probe(cwd),
            "mcp" => {
                let names: Vec<&str> = self.mcp.iter().filter_map(|server| server["name"].as_str()).collect();
                chunk("agent_message_chunk", &format!("mcp={}", names.join(",")));
            }
            "proxy-call" => self.proxy_call(),
            "drop-commands" => update(json!({"sessionUpdate": "available_commands_update", "availableCommands": []})),
            "wait-child" => {
                let child = std::process::Command::new("sleep").arg("30").spawn().map(|child| child.id());
                chunk("agent_message_chunk", &format!("child={}", child.unwrap_or_default()));
                while !self.cancelled {
                    match self.lines.next() {
                        Some(Ok(line)) if line.contains("session/cancel") => self.cancelled = true,
                        Some(Ok(_)) => {}
                        _ => break,
                    }
                }
                return json!({"stopReason": "cancelled"});
            }
            command if command.starts_with('/') => chunk("agent_message_chunk", &format!("command={command}")),
            "tools" => self.tools(cwd),
            "wait" => {
                while !self.cancelled {
                    match self.lines.next() {
                        Some(Ok(line)) if line.contains("session/cancel") => self.cancelled = true,
                        Some(Ok(_)) => {}
                        _ => break,
                    }
                }
                return json!({"stopReason": "cancelled"});
            }
            _ => {}
        }
        update(
            json!({"sessionUpdate": "usage_update", "used": 1234, "size": 200000, "cost": {"amount": 0.5, "currency": "USD"}}),
        );
        json!({"stopReason": "end_turn", "usage": {"inputTokens": 10, "outputTokens": 5, "totalTokens": 15}})
    }

    fn run(&mut self) {
        let mut cwd = String::new();
        while let Some(Ok(line)) = self.lines.next() {
            let message: Value = serde_json::from_str(&line).unwrap_or_default();
            let (Some(method), id) = (message["method"].as_str().map(str::to_string), message["id"].clone()) else {
                continue;
            };
            let params = &message["params"];
            let result = match method.as_str() {
                "initialize" => {
                    let mut methods = vec![json!({"id": "agent-login", "name": "Log in"})];
                    if params.pointer("/clientCapabilities/auth/terminal") == Some(&json!(true)) {
                        methods.push(
                            json!({"id": "term", "name": "Terminal login", "type": "terminal", "args": ["--login"]}),
                        );
                    }
                    json!({"protocolVersion": 1, "authMethods": methods, "agentCapabilities": {"loadSession": true,
                        "mcpCapabilities": {"http": true},
                        "promptCapabilities": {"image": true, "embeddedContext": true}}})
                }
                "authenticate" => json!({}),
                "session/new" | "session/load" => {
                    self.news += u32::from(method == "session/new");
                    if let Ok(path) = std::env::var("FAKE_ACP_PID_FILE") {
                        let _ = std::fs::write(path, std::process::id().to_string());
                    }
                    cwd = params["cwd"].as_str().unwrap_or_default().to_string();
                    self.mcp = params["mcpServers"].as_array().cloned().unwrap_or_default();
                    if method == "session/load" {
                        chunk("agent_message_chunk", "replayed history");
                    }
                    self.session()
                }
                "session/set_mode" => {
                    self.mode = params["modeId"].as_str().unwrap_or_default().to_string();
                    json!({})
                }
                "session/set_config_option" => {
                    let value = params["value"].as_str().unwrap_or_default().to_string();
                    match params["configId"].as_str().unwrap_or_default() {
                        "model" => self.model = value,
                        "effort" | "effortLevel" => self.effort = value,
                        "permission" if fake_acp_config::PERMISSIONS.contains(&value.as_str()) => self.mode = value,
                        _ => {}
                    }
                    json!({"configOptions": self.config()})
                }
                "session/set_model" => {
                    self.model = params["modelId"].as_str().unwrap_or_default().to_string();
                    json!({})
                }
                "session/prompt" => {
                    self.cancelled = false;
                    self.prompt(params, &cwd)
                }
                _ if id.is_null() => continue,
                _ => {
                    send(&json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "unknown"}}));
                    continue;
                }
            };
            send(&json!({"jsonrpc": "2.0", "id": id, "result": result}));
            if method == "session/new" || method == "session/load" {
                update(json!({"sessionUpdate": "available_commands_update", "availableCommands": [
                    {"name": "web", "description": "Search the web", "input": {"hint": "query"}},
                    {"name": "compact", "description": "Compact the agent context"}]}));
            }
        }
    }
}

pub fn main() {
    println!("fake agent log line on stdout");
    let lines = std::io::stdin().lock().lines();
    let (mode, model) = ("ask".into(), "fast".into());
    let variant = std::env::var("FAKE_ACP_CONFIG").unwrap_or_default();
    let effort = fake_acp_config::initial_effort(&variant).to_string();
    Agent { lines, next_id: 100, cancelled: false, mode, model, turns: 0, mcp: Vec::new(), variant, effort, news: 0 }
        .run();
}
