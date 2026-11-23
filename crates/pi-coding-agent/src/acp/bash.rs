//! Bash tool calls as display-only terminals (upstream `translate/bash.ts`): Zed renders an
//! `execute` tool with terminal content plus `terminal_info`, `terminal_output`, and
//! `terminal_exit` metadata. Every client also gets `rawInput` and `rawOutput`, and clients that
//! show no such terminals get the output as text content.

use super::schema::{ContentBlock, Meta, Terminal, TextContent, ToolCallContent};
use serde_json::{Map, Value, json};

/// Whether the client renders the terminal metadata. Zed says so with
/// `clientCapabilities._meta.terminal_output: true`; pi-acp sends the metadata to every client.
pub fn client_shows_terminal_output(client_meta: Option<&Map<String, Value>>) -> bool {
    client_meta.and_then(|meta| meta.get("terminal_output")).and_then(Value::as_bool) == Some(true)
}

pub fn is_bash_tool(tool_name: &str) -> bool {
    tool_name.eq_ignore_ascii_case("bash")
}

/// The command of a bash call, searched in the argument shapes pi and clients use.
pub fn bash_command(value: &Value) -> Option<String> {
    fn direct(record: &Value) -> Option<&Value> {
        record.get("command").filter(|value| !value.is_null()).or_else(|| record.get("cmd").filter(|v| !v.is_null()))
    }
    let mut command = direct(value);
    for key in ["args", "input", "rawInput", "toolInput", "details"] {
        if command.is_some() {
            break;
        }
        command = value.get(key).and_then(direct);
    }
    command.and_then(Value::as_str).filter(|command| !command.trim().is_empty()).map(String::from)
}

/// The text a bash result shows: text content, else stdout and stderr.
pub fn bash_result_text(result: &Value) -> String {
    if let Some(blocks) = result.get("content").and_then(Value::as_array) {
        let texts: Vec<&str> =
            blocks.iter().filter(|block| block["type"] == "text").filter_map(|block| block["text"].as_str()).collect();
        let joined = texts.concat();
        if !joined.is_empty() {
            return joined;
        }
    }
    let details = result.get("details").cloned().unwrap_or(Value::Null);
    let field = |record: &Value, key: &str| record.get(key).and_then(Value::as_str).map(String::from);
    let stdout = field(&details, "stdout")
        .or_else(|| field(result, "stdout"))
        .or_else(|| field(&details, "output"))
        .or_else(|| field(result, "output"));
    let stderr = field(&details, "stderr").or_else(|| field(result, "stderr"));
    [stdout, stderr].into_iter().flatten().filter(|part| !part.is_empty()).collect::<Vec<_>>().join("\n")
}

/// The exit code a bash result reports; 1 for errors without one, else 0.
pub fn bash_exit_code(result: &Value, is_error: bool) -> i64 {
    let details = result.get("details").cloned().unwrap_or(Value::Null);
    // pi's own bash tool reports the code in its structured content.
    let structured = result.get("structuredContent").and_then(|structured| structured.get("exit_code"));
    let code = [details.get("exitCode"), result.get("exitCode"), details.get("code"), result.get("code"), structured]
        .into_iter()
        .flatten()
        .find(|value| !value.is_null());
    match code.and_then(Value::as_f64) {
        Some(code) => code as i64,
        None => i64::from(is_error),
    }
}

/// Output added after the text already sent; all of `next` when the output was replaced.
pub fn bash_output_delta<'a>(sent: &str, next: &'a str) -> &'a str {
    next.strip_prefix(sent).unwrap_or(next)
}

pub fn bash_terminal_content(tool_call_id: &str) -> Vec<ToolCallContent> {
    vec![ToolCallContent::Terminal(Terminal::new(tool_call_id.to_string()))]
}

/// The terminal and the output so far as text, for clients that show no display-only terminals.
pub fn bash_text_content(tool_call_id: &str, output: &str) -> Vec<ToolCallContent> {
    let mut content = bash_terminal_content(tool_call_id);
    if !output.is_empty() {
        content.push(ToolCallContent::from(ContentBlock::Text(TextContent::new(output))));
    }
    content
}

/// `rawOutput` of a bash call: the output so far, and the exit code once the command ended.
pub fn bash_raw_output(output: &str, exit_code: Option<i64>) -> Value {
    let mut value = json!({"output": output});
    if let Some(code) = exit_code {
        value["exitCode"] = json!(code);
    }
    value
}

fn meta(key: &str, value: Value) -> Meta {
    let mut meta = Meta::new();
    meta.insert(key.into(), value);
    meta
}

pub fn bash_terminal_info_meta(tool_call_id: &str, cwd: &str) -> Meta {
    meta("terminal_info", json!({"terminal_id": tool_call_id, "cwd": cwd}))
}

pub fn bash_terminal_output_meta(tool_call_id: &str, data: &str) -> Meta {
    meta("terminal_output", json!({"terminal_id": tool_call_id, "data": data}))
}

pub fn bash_terminal_exit_meta(tool_call_id: &str, exit_code: i64) -> Meta {
    meta("terminal_exit", json!({"terminal_id": tool_call_id, "exit_code": exit_code, "signal": null}))
}
