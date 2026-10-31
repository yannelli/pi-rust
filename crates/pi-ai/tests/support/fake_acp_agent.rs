//! A scripted ACP agent for the `acp` provider tests. The prompt text picks the script: `tools`
//! edits a file with permission, `wait` runs until `session/cancel`, anything else echoes.

use std::io::{BufRead, Write};

use serde_json::{Value, json};

struct Agent {
    lines: std::io::Lines<std::io::StdinLock<'static>>,
    next_id: u64,
    cancelled: bool,
    mode: String,
    model: String,
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

fn config(model: &str) -> Value {
    json!([
        {"id": "model", "name": "Model", "category": "model", "type": "select", "currentValue": model,
         "options": [{"value": "fast", "name": "Fast"}, {"value": "smart", "name": "Smart"}]},
        {"id": "effort", "name": "Effort", "category": "thought_level", "type": "select", "currentValue": "low",
         "options": [{"value": "low", "name": "Low"}, {"value": "high", "name": "High"}]},
    ])
}

impl Agent {
    fn session(&self) -> Value {
        json!({"sessionId": "s1", "configOptions": config(&self.model),
            "modes": {"currentModeId": self.mode, "availableModes": [{"id": "ask", "name": "Ask"}, {"id": "code", "name": "Code"}]}})
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

    fn prompt(&mut self, params: &Value, cwd: &str) -> Value {
        let blocks = params["prompt"].as_array().cloned().unwrap_or_default();
        let types: Vec<&str> = blocks.iter().filter_map(|block| block["type"].as_str()).collect();
        let text = blocks.iter().rev().find_map(|block| block["text"].as_str()).unwrap_or_default().to_string();
        chunk("agent_thought_chunk", "thinking");
        chunk("agent_message_chunk", &format!("Hello [{}] mode={} model={} ", types.join(","), self.mode, self.model));
        match text.as_str() {
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
                        "promptCapabilities": {"image": true, "embeddedContext": true}}})
                }
                "authenticate" => json!({}),
                "session/new" | "session/load" => {
                    cwd = params["cwd"].as_str().unwrap_or_default().to_string();
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
                    if params["configId"] == "model" {
                        self.model = params["value"].as_str().unwrap_or_default().to_string();
                    }
                    json!({"configOptions": config(&self.model)})
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
        }
    }
}

fn main() {
    println!("fake agent log line on stdout");
    let lines = std::io::stdin().lock().lines();
    Agent { lines, next_id: 100, cancelled: false, mode: "ask".into(), model: "fast".into() }.run();
}
