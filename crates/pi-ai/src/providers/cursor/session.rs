//! One run's server messages: interaction updates become pi events; exec, KV, and interaction
//! queries get the replies Cursor waits for. MCP tool calls are handed back to pi to run.

use std::collections::HashMap;

use serde_json::{Map, Value as Json};

use super::proto::{Value, Writer, field, fields};
use super::request::{MCP_PROVIDER, sanitize_call_id};
use super::value;
use crate::stream::MessageBuilder;
use crate::types::{ModelCost, StopReason, Usage};

/// The MCP result that tells Cursor to end the turn while pi runs the tool.
pub(crate) const HANDOFF_TEXT: &str = "Tool call received and handed off to the external client for execution. Do not retry or call it again; end the turn. The result will be provided in the next request.";
const HOOK_VARIANTS: [u32; 10] = [1, 2, 3, 4, 5, 6, 7, 8, 9, 11];

#[derive(Default)]
struct OpenCall {
    handle: usize,
    streamed: String,
}

pub(crate) struct Session {
    pub blobs: HashMap<Vec<u8>, Vec<u8>>,
    request_context: Writer,
    tools: Vec<Writer>,
    cost: ModelCost,
    /// Open tool calls by the update's `call_id`, and by MCP `tool_call_id`.
    calls: HashMap<String, OpenCall>,
    call_ids: HashMap<String, String>,
    pub turn_ended: bool,
    /// Context use from the newest conversation checkpoint.
    context_tokens: Option<u64>,
    step_completed_last: bool,
    handed_off: bool,
}

fn bytes_of<'a>(fields: &[(u32, Value<'a>)], number: u32) -> &'a [u8] {
    field(fields, number).and_then(Value::as_bytes).unwrap_or_default()
}

fn str_of<'a>(fields: &[(u32, Value<'a>)], number: u32) -> &'a str {
    field(fields, number).and_then(Value::as_str).unwrap_or_default()
}

/// Copies field `number` with its wire type, as the id of a reply.
fn echo(writer: Writer, fields: &[(u32, Value)], number: u32) -> Writer {
    match field(fields, number) {
        Some(Value::Varint(id)) => writer.varint(number, id),
        Some(Value::Bytes(id)) => writer.bytes(number, id),
        _ => writer,
    }
}

/// Name, id, and arguments of an `McpArgs`.
fn mcp_args(data: &[u8]) -> Result<(String, String, Map<String, Json>, bool), String> {
    let args = fields(data)?;
    let name = Some(str_of(&args, 5)).filter(|name| !name.is_empty()).unwrap_or(str_of(&args, 1)).to_string();
    let mut map = Map::new();
    for (_, entry) in args.iter().filter(|(number, _)| *number == 2) {
        let entry = fields(entry.as_bytes().unwrap_or_default())?;
        map.insert(str_of(&entry, 1).to_string(), value::decode(bytes_of(&entry, 2))?);
    }
    let approval_only = field(&args, 7).and_then(Value::as_u64) == Some(1);
    Ok((name, str_of(&args, 3).to_string(), map, approval_only))
}

/// The `McpArgs` of a `ToolCall` holding an MCP call (`ToolCall.mcp_tool_call.args`).
fn tool_call_args(tool_call: &[u8]) -> Option<&[u8]> {
    let call = fields(tool_call).ok()?;
    let mcp = fields(field(&call, 15)?.as_bytes()?).ok()?;
    field(&mcp, 1)?.as_bytes()
}

impl Session {
    pub fn new(blobs: HashMap<Vec<u8>, Vec<u8>>, request_context: Writer, tools: Vec<Writer>, cost: ModelCost) -> Self {
        Self {
            blobs,
            request_context,
            tools,
            cost,
            calls: HashMap::new(),
            call_ids: HashMap::new(),
            turn_ended: false,
            step_completed_last: false,
            context_tokens: None,
            handed_off: false,
        }
    }

    /// Handles one `AgentServerMessage` and returns the `AgentClientMessage`s to append.
    pub fn handle(&mut self, out: &mut MessageBuilder, data: &[u8]) -> Result<Vec<Vec<u8>>, String> {
        let message = fields(data)?;
        let mut replies = Vec::new();
        for (number, value) in &message {
            let Some(body) = value.as_bytes() else { continue };
            match number {
                1 => self.update(out, body)?,
                3 => self.checkpoint(body)?,
                2 => replies.extend(self.exec(out, body)?),
                4 => replies.extend(self.kv(body)?),
                7 => replies.extend(interaction_reply(body)?),
                _ => {}
            }
        }
        Ok(replies)
    }

    fn start_call(&mut self, out: &mut MessageBuilder, call_id: &str, args: Option<&[u8]>) -> Result<usize, String> {
        if let Some(open) = self.calls.get(call_id) {
            return Ok(open.handle);
        }
        let (name, id, arguments, _) = args.map(mcp_args).transpose()?.unwrap_or_default();
        let id = if id.is_empty() { sanitize_call_id(call_id) } else { id };
        // An `mcp_args` exec can open the block before its interaction updates arrive.
        if let Some(handle) = self.call_ids.get(&id).and_then(|key| self.calls.get(key)).map(|open| open.handle) {
            self.calls.insert(call_id.to_string(), OpenCall { handle, streamed: String::new() });
            return Ok(handle);
        }
        let handle = out.tool_call_start(id.clone(), name);
        if !arguments.is_empty() {
            out.tool_call_arguments(handle, arguments);
        }
        self.calls.insert(call_id.to_string(), OpenCall { handle, streamed: String::new() });
        self.call_ids.insert(id, call_id.to_string());
        Ok(handle)
    }

    fn update(&mut self, out: &mut MessageBuilder, data: &[u8]) -> Result<(), String> {
        let update = fields(data)?;
        self.step_completed_last = false;
        for (number, value) in &update {
            let body = fields(value.as_bytes().unwrap_or_default())?;
            match number {
                1 => out.text_delta(str_of(&body, 1)),
                4 => out.thinking_delta(str_of(&body, 1)),
                5 => out.close_open_block(),
                2 | 7 | 3 => {
                    let call_id = str_of(&body, 1);
                    let Some(args) = tool_call_args(bytes_of(&body, 2)) else { continue };
                    let handle = self.start_call(out, call_id, Some(args))?;
                    if *number == 7 {
                        let snapshot = str_of(&body, 3);
                        let open = self.calls.get_mut(call_id).expect("started above");
                        let delta = snapshot.strip_prefix(open.streamed.as_str()).unwrap_or(snapshot).to_string();
                        open.streamed = snapshot.to_string();
                        out.tool_call_delta(handle, &delta);
                    } else if *number == 3 {
                        let streamed = self.calls.get(call_id).map(|open| open.streamed.clone()).unwrap_or_default();
                        let mut merged: Map<String, Json> = serde_json::from_str(&streamed).unwrap_or_default();
                        for (key, value) in mcp_args(args)?.2 {
                            let keep_streamed = value.is_string()
                                && merged.get(&key).is_some_and(|old| old.is_object() || old.is_array());
                            if !keep_streamed {
                                merged.insert(key, value);
                            }
                        }
                        out.tool_call_arguments(handle, merged);
                        out.end_tool_call(handle);
                    }
                }
                14 => {
                    let count = |number| field(&body, number).and_then(Value::as_u64).unwrap_or_default();
                    let (input, output, cache_read, cache_write) = (count(1), count(2), count(3), count(4));
                    if input + output + cache_read + cache_write > 0 {
                        let mut usage = Usage {
                            input: input.saturating_sub(cache_read + cache_write),
                            output,
                            cache_read,
                            cache_write,
                            reasoning: (count(5) > 0).then(|| count(5)),
                            ..Usage::default()
                        };
                        usage.total_tokens = usage.input + output + cache_read + cache_write;
                        usage.apply_cost(&self.cost);
                        out.message_mut().usage = usage;
                    }
                    self.turn_ended = true;
                }
                17 => self.step_completed_last = true,
                _ => {}
            }
        }
        Ok(())
    }

    fn exec(&mut self, out: &mut MessageBuilder, data: &[u8]) -> Result<Vec<Vec<u8>>, String> {
        let exec = fields(data)?;
        let reply = |(number, result): (u32, Writer)| {
            let message = echo(echo(Writer::new(), &exec, 1), &exec, 15).message(number, result);
            Writer::new().message(2, message).finish()
        };
        let variant = exec.iter().find(|(number, value)| !matches!(number, 1 | 15) && value.as_bytes().is_some());
        let Some((number, body)) = variant.map(|(number, value)| (*number, value.as_bytes().unwrap_or_default()))
        else {
            return Ok(Vec::new());
        };
        Ok(match number {
            10 => {
                let success = Writer::new().message(1, self.request_context.clone());
                vec![reply((10, Writer::new().message(1, success)))]
            }
            11 => {
                let (name, id, arguments, approval_only) = mcp_args(body)?;
                if approval_only {
                    let reason = format!("Tool \"{name}\" is not approved to run without asking.");
                    vec![reply((11, Writer::new().message(3, Writer::new().string(1, &reason))))]
                } else {
                    if !self.call_ids.contains_key(&id) {
                        let handle = self.start_call(out, &id, Some(body))?;
                        out.tool_call_arguments(handle, arguments);
                        out.end_tool_call(handle);
                    }
                    self.handed_off = true;
                    let text = Writer::new().message(1, Writer::new().string(1, HANDOFF_TEXT));
                    vec![reply((11, Writer::new().message(1, Writer::new().message(1, text))))]
                }
            }
            36 => {
                let wanted: Vec<&str> =
                    fields(body)?.iter().filter(|(n, _)| *n == 1).filter_map(|(_, v)| v.as_str()).collect();
                let mut servers = Writer::new();
                if wanted.is_empty() || wanted.contains(&MCP_PROVIDER) {
                    let server = Writer::new().string(1, MCP_PROVIDER).string(2, MCP_PROVIDER);
                    let server = self.tools.iter().cloned().fold(server, |server, tool| server.message(5, tool));
                    servers = servers.message(1, server.string(7, "connected"));
                }
                vec![reply((36, Writer::new().message(1, servers)))]
            }
            27 => {
                let request = fields(bytes_of(&fields(body)?, 1))?;
                match request.iter().map(|(n, _)| *n).find(|n| HOOK_VARIANTS.contains(n)) {
                    Some(hook) => {
                        vec![reply((27, Writer::new().message(1, Writer::new().message(hook, Writer::new()))))]
                    }
                    None => throw(&exec, "unsupported hook"),
                }
            }
            other => throw(&exec, &format!("exec variant {other} is not supported")),
        })
    }

    fn kv(&mut self, data: &[u8]) -> Result<Vec<Vec<u8>>, String> {
        let kv = fields(data)?;
        let reply = |result: (u32, Writer)| {
            Writer::new().message(3, echo(Writer::new(), &kv, 1).message(result.0, result.1)).finish()
        };
        if let Some(get) = field(&kv, 2).and_then(Value::as_bytes) {
            let mut result = Writer::new();
            if let Some(blob) = self.blobs.get(bytes_of(&fields(get)?, 1)) {
                result = result.bytes(1, blob);
            }
            return Ok(vec![reply((2, result))]);
        }
        if let Some(set) = field(&kv, 3).and_then(Value::as_bytes) {
            let set = fields(set)?;
            self.blobs.insert(bytes_of(&set, 1).to_vec(), bytes_of(&set, 2).to_vec());
            return Ok(vec![reply((3, Writer::new()))]);
        }
        Ok(Vec::new())
    }

    /// `ConversationStateStructure.token_details.used_tokens`, Cursor's count of the context.
    fn checkpoint(&mut self, data: &[u8]) -> Result<(), String> {
        let details = fields(bytes_of(&fields(data)?, 5))?;
        if let Some(used) = field(&details, 1).and_then(Value::as_u64).filter(|used| *used > 0) {
            self.context_tokens = Some(used);
        }
        Ok(())
    }

    /// The stop reason once the body ended, or the incomplete-stream error. A checkpoint's
    /// context count becomes `total_tokens`, which pi reads as the context use.
    pub fn finish(&self, out: &mut MessageBuilder) -> Result<StopReason, String> {
        if let Some(used) = self.context_tokens {
            out.message_mut().usage.total_tokens = used;
        }
        let last_is_text = matches!(out.message().content.last(), Some(crate::types::ContentBlock::Text { .. }));
        if !self.turn_ended && !(self.step_completed_last && last_is_text && !self.handed_off) {
            return Err("Cursor stream ended before the turn ended".into());
        }
        let calls = out.message().content.iter().any(|block| matches!(block, crate::types::ContentBlock::ToolCall(_)));
        Ok(if calls { StopReason::ToolUse } else { StopReason::Stop })
    }
}

fn throw(exec: &[(u32, Value)], error: &str) -> Vec<Vec<u8>> {
    let thrown = echo(Writer::new(), exec, 1).string(2, error).string(4, "exec_variant_unsupported");
    let close = echo(Writer::new(), exec, 1);
    vec![
        Writer::new().message(5, Writer::new().message(2, thrown)).finish(),
        Writer::new().message(5, Writer::new().message(1, close)).finish(),
    ]
}

/// Approves web tools and declines questions, mode switches, and plans, which need Cursor's UI.
fn interaction_reply(data: &[u8]) -> Result<Vec<Vec<u8>>, String> {
    let query = fields(data)?;
    let Some((number, _)) = query.iter().find(|(number, value)| *number >= 2 && value.as_bytes().is_some()) else {
        return Ok(Vec::new());
    };
    let declined = |text: &str| Writer::new().string(1, text);
    let body = match number {
        8 => return Ok(Vec::new()),
        3 => Writer::new().message(1, Writer::new().message(3, declined("pi cannot show Cursor questions"))),
        4 => Writer::new().message(2, declined("pi keeps its own mode")),
        7 => Writer::new().message(1, Writer::new().message(2, declined("pi does not create Cursor plans"))),
        _ => Writer::new().message(1, Writer::new()),
    };
    let response = echo(Writer::new(), &query, 1).message(*number, body);
    Ok(vec![Writer::new().message(6, response).finish()])
}
