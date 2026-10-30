//! The `AgentRunRequest` of one call, the blobs its history refers to, and the request context
//! (rules and MCP tools) Cursor asks for while it runs.

use std::collections::HashMap;

use ring::digest::{SHA256, digest};
use serde_json::json;

use super::history::{self, Results, user_images, user_text};
use super::proto::Writer;
use super::value;
use crate::providers::common::{compat_str, compat_value};
use crate::simple_options::clamp_thinking_level;
use crate::types::{Context, Message, Model, ThinkingLevel};

pub(crate) const MCP_PROVIDER: &str = "pi-agent";
const DEFAULT_SYSTEM: &str = "You are a helpful assistant.";
/// Longest first, so `-extra-high` wins over `-high`.
const EFFORT_SUFFIXES: [(&str, &str); 8] = [
    ("-extra-high", "xhigh"),
    ("-none", "off"),
    ("-minimal", "minimal"),
    ("-medium", "medium"),
    ("-xhigh", "xhigh"),
    ("-high", "high"),
    ("-low", "low"),
    ("-max", "max"),
];

pub(crate) struct RunRequest {
    /// `AgentClientMessage{run_request}`.
    pub message: Vec<u8>,
    pub blobs: HashMap<Vec<u8>, Vec<u8>>,
    pub request_context: Writer,
    pub tools: Vec<Writer>,
}

/// Blob bytes keyed by their SHA-256, as Cursor's `getBlobArgs` asks for them.
#[derive(Default)]
pub(crate) struct Blobs(pub HashMap<Vec<u8>, Vec<u8>>);

impl Blobs {
    pub fn put(&mut self, bytes: Vec<u8>) -> Vec<u8> {
        let id = digest(&SHA256, &bytes).as_ref().to_vec();
        self.0.insert(id.clone(), bytes);
        id
    }
}

/// Cursor answers ids outside `[A-Za-z0-9_-]` or over 64 characters with `resource_exhausted`.
pub(crate) fn sanitize_call_id(id: &str) -> String {
    id.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).take(64).collect()
}

/// A version 4 shaped UUID from the first 16 of `bytes`.
pub(crate) fn uuid_from(bytes: &[u8]) -> String {
    let mut b: [u8; 16] = bytes[..16].try_into().expect("16 bytes");
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let hex: String = b.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &hex[..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..])
}

pub(crate) fn deterministic_uuid(seed: &str) -> String {
    uuid_from(digest(&SHA256, seed.as_bytes()).as_ref())
}

/// `(details id, requested id, parameters, max mode)`. GPT ids request their base model with a
/// `reasoning` parameter; other ids name the model in both places.
pub(crate) fn model_ids(id: &str) -> (String, String, Vec<(&'static str, String)>, bool) {
    let (lane, fast) = id.strip_suffix("-fast").map_or((id, ""), |rest| (rest, "-fast"));
    let effort =
        EFFORT_SUFFIXES.iter().find_map(|(suffix, effort)| lane.strip_suffix(suffix).map(|base| (base, *effort)));
    let max_mode = effort.is_some_and(|(_, effort)| matches!(effort, "xhigh" | "max"));
    match effort {
        Some((base, effort)) if id.starts_with("gpt-") => {
            let params = if effort == "off" { Vec::new() } else { vec![("reasoning", effort.to_string())] };
            (id.to_string(), format!("{base}{fast}"), params, max_mode)
        }
        _ if id == "composer-2.5" => (id.into(), id.into(), vec![("fast", "false".to_string())], max_mode),
        _ => (id.to_string(), id.to_string(), Vec::new(), max_mode),
    }
}

/// The route a discovered model keeps in `compat`: the `cursorLevels` entry of the thinking
/// level, else `cursorSlug` and `cursorParameters`. With `fast`, the same keys of `cursorFast`
/// apply when the model has it. Other models use the route [`model_ids`] derives from the id.
fn route(model: &Model, level: Option<ThinkingLevel>, fast: bool) -> (String, String, Vec<(String, String)>, bool) {
    let Some(requested) = compat_str(model, "cursorModel") else {
        let (details, requested, params, max_mode) = model_ids(&model.id);
        let params = params.into_iter().map(|(id, value)| (id.to_string(), value)).collect();
        return (details, requested, params, max_mode);
    };
    let level = clamp_thinking_level(model, level.unwrap_or(ThinkingLevel::Off));
    let fast_set = compat_value(model, "cursorFast").filter(|set| fast && set.is_object());
    let (levels, slug, params) = match fast_set {
        Some(set) => (set.get("levels"), set.get("slug").and_then(|slug| slug.as_str()), set.get("parameters")),
        None => (
            compat_value(model, "cursorLevels"),
            compat_str(model, "cursorSlug"),
            compat_value(model, "cursorParameters"),
        ),
    };
    let chosen = levels.and_then(|levels| levels.get(level.as_str()));
    let slug = chosen.and_then(|route| route["slug"].as_str()).or(slug);
    let params = chosen.map(|route| &route["parameters"]).or(params);
    let params = params
        .and_then(|params| params.as_array())
        .into_iter()
        .flatten()
        .filter_map(|param| Some((param["id"].as_str()?.to_string(), param["value"].as_str()?.to_string())))
        .collect();
    let max_mode = compat_value(model, "cursorMaxMode").and_then(|max| max.as_bool()).unwrap_or(false);
    (slug.unwrap_or(&model.id).to_string(), requested.to_string(), params, max_mode)
}

/// Every pi tool as an MCP tool of the `pi-agent` server.
pub(crate) fn tool_definitions(context: &Context) -> Vec<Writer> {
    let fallback = json!({"type": "object", "properties": {}, "required": []});
    let tools = context.tools.iter().map(|tool| {
        let schema = if tool.parameters.is_object() { &tool.parameters } else { &fallback };
        Writer::new()
            .string(1, &tool.name)
            .string(2, &tool.description)
            .bytes(3, &value::encode(schema).finish())
            .string(4, MCP_PROVIDER)
            .string(5, &tool.name)
    });
    tools.collect()
}

fn action(user: Option<&crate::types::UserMessage>, message_id: &str) -> Writer {
    let Some(user) = user else { return Writer::new().message(2, Writer::new()) };
    let images = user_images(&user.content).into_iter().filter_map(|(mime, data)| {
        let bytes = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, data).ok()?;
        Some(Writer::new().string(2, &deterministic_uuid(data)).string(7, mime).bytes(8, &bytes))
    });
    let selected = images.fold(Writer::new(), |selected, image| selected.message(1, image));
    let message = Writer::new().string(1, &user_text(&user.content)).string(2, message_id).message(3, selected);
    Writer::new().message(1, Writer::new().message(1, message))
}

/// A request that ends with a user message sends it as the action; otherwise the run resumes
/// from the history, whose tool results carry what pi ran.
pub(crate) fn build(
    model: &Model,
    context: &Context,
    level: Option<ThinkingLevel>,
    fast: bool,
    conversation_id: &str,
    message_id: &str,
) -> RunRequest {
    let messages: Vec<&Message> =
        context.messages.iter().filter(|message| !matches!(message, Message::Custom(_))).collect();
    let active = match messages.last() {
        Some(Message::User(user)) => Some(user),
        _ => None,
    };
    let history: Vec<Message> =
        messages[..messages.len() - usize::from(active.is_some())].iter().map(|m| (*m).clone()).collect();
    let mut results: Results = HashMap::new();
    for message in &history {
        if let Message::ToolResult(result) = message {
            results.insert(sanitize_call_id(&result.tool_call_id), result);
        }
    }
    let system = context.system_prompt.as_deref().map(str::trim).filter(|prompt| !prompt.is_empty());
    let mut records = vec![json!({"role": "system", "content": system.unwrap_or(DEFAULT_SYSTEM)})];
    let history_records = history::records(&history, &results);
    // Without this record a request that carries history loses the system prompt.
    if let (Some(system), false) = (system, history_records.is_empty()) {
        records.push(json!({"role": "user", "content": format!("<rules>\n{system}\n</rules>")}));
    }
    records.extend(history_records);
    let mut blobs = Blobs::default();
    let root: Vec<Vec<u8>> = records.iter().map(|record| blobs.put(record.to_string().into_bytes())).collect();
    let turns = history::turns(&history, &results, &mut blobs);
    let state = root.iter().fold(Writer::new(), |state, id| state.bytes(1, id));
    let state = turns.iter().fold(state, |state, id| state.bytes(8, id));
    let (details_id, requested_id, params, max_mode) = route(model, level, fast);
    let mut details = Writer::new().string(1, &details_id).string(3, &model.id).string(4, &model.name);
    let mut requested = Writer::new().string(1, &requested_id);
    if max_mode {
        details = details.bool(7, true);
        requested = requested.bool(2, true);
    }
    let requested = params
        .iter()
        .fold(requested, |requested, (id, value)| requested.message(3, Writer::new().string(1, id).string(2, value)));
    let run = Writer::new()
        .message(1, state)
        .message(2, action(active, message_id))
        .message(3, details)
        .string(5, conversation_id)
        .message(9, requested);
    let tools = tool_definitions(context);
    let rule = system.map(|system| {
        let global = Writer::new().message(1, Writer::new());
        Writer::new().string(1, "/pi/system-prompt/0.mdc").string(2, system).message(3, global).varint(4, 2)
    });
    let request_context = rule.into_iter().fold(Writer::new(), |request, rule| request.message(2, rule));
    let request_context = tools.iter().cloned().fold(request_context, |request, tool| request.message(7, tool));
    RunRequest { message: Writer::new().message(1, run).finish(), blobs: blobs.0, request_context, tools }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::{Value as Json, json};

    use super::*;
    use crate::providers::cursor::proto::{Value, field, fields};
    use crate::types::{AssistantMessage, ContentBlock, ToolCall, ToolResultMessage};

    fn nested<'a>(data: &'a [u8], path: &[u32]) -> &'a [u8] {
        path.iter().fold(data, |data, number| field(&fields(data).unwrap(), *number).unwrap().as_bytes().unwrap())
    }

    #[test]
    fn gpt_ids_request_their_base_with_a_reasoning_parameter() {
        assert_eq!(
            model_ids("gpt-5.5-high"),
            ("gpt-5.5-high".into(), "gpt-5.5".into(), vec![("reasoning", "high".into())], false)
        );
        assert_eq!(model_ids("gpt-5.2-xhigh-fast").1, "gpt-5.2-fast");
        assert!(model_ids("gpt-5.2-xhigh-fast").3);
        assert_eq!(
            model_ids("claude-opus-5-high"),
            ("claude-opus-5-high".into(), "claude-opus-5-high".into(), vec![], false)
        );
        assert_eq!(model_ids("composer-2.5").2, vec![("fast", "false".to_string())]);
    }

    #[test]
    fn tool_results_resume_the_run_with_rules_and_sanitized_ids() {
        let call = ToolCall {
            id: "call|1".into(),
            name: "read".into(),
            arguments: json!({"path": "a"}).as_object().cloned().unwrap(),
            ..ToolCall::default()
        };
        let assistant = AssistantMessage { content: vec![ContentBlock::ToolCall(call)], ..AssistantMessage::default() };
        let result = ToolResultMessage {
            tool_call_id: "call|1".into(),
            tool_name: "read".into(),
            content: vec![ContentBlock::text("file body")],
            ..ToolResultMessage::default()
        };
        let context = Context {
            system_prompt: Some("Be brief".into()),
            messages: vec![Message::user("read a"), Message::Assistant(assistant), Message::ToolResult(result)],
            tools: Vec::new(),
        };
        let model = Model { id: "claude-opus-5-high".into(), name: "Claude Opus 5".into(), ..Model::default() };
        let built = build(&model, &context, None, false, "conv-1", "msg-1");
        let run = nested(&built.message, &[1]);
        assert_eq!(nested(run, &[2]), [0x12, 0x00]);
        let state = fields(nested(run, &[1])).unwrap();
        let records: Vec<Json> = state
            .iter()
            .filter(|(number, _)| *number == 1)
            .map(|(_, id)| serde_json::from_slice(&built.blobs[id.as_bytes().unwrap()]).unwrap())
            .collect();
        assert_eq!(records[1], json!({"role": "user", "content": "<rules>\nBe brief\n</rules>"}));
        assert_eq!(records[3]["content"][0]["toolCallId"], "call_1");
        assert_eq!(
            records[4]["content"][0],
            json!({"type": "tool-result", "toolName": "read", "toolCallId": "call_1", "result": "file body"})
        );
        let turns: Vec<&[u8]> =
            state.iter().filter(|(number, _)| *number == 8).filter_map(|(_, id)| id.as_bytes()).collect();
        assert_eq!(turns.len(), 1);
        assert_eq!(field(&fields(run).unwrap(), 5).and_then(Value::as_str), Some("conv-1"));
    }

    #[test]
    fn discovered_models_use_their_compat_route() {
        let compat = json!({"cursorModel": "claude-opus-5", "cursorParameters": [{"id": "thinking", "value": "true"}], "cursorMaxMode": true});
        let model = Model { id: "claude-opus-5-high-max".into(), compat: Some(compat), ..Model::default() };
        let route = route(&model, None, false);
        assert_eq!(
            route,
            ("claude-opus-5-high-max".into(), "claude-opus-5".into(), vec![("thinking".into(), "true".into())], true)
        );
    }

    #[test]
    fn the_thinking_level_picks_the_variant_route() {
        let effort =
            |effort: &str| json!([{"id": "reasoning_effort", "value": effort}, {"id": "fast", "value": "false"}]);
        let compat = json!({"cursorModel": "grok-4.7", "cursorSlug": "grok-4.7-high", "cursorParameters": effort("high"),
            "cursorMaxMode": false, "cursorLevels": {
                "low": {"slug": "grok-4.7-low", "parameters": effort("low")},
                "high": {"slug": "grok-4.7-high", "parameters": effort("high")}}});
        let map = ThinkingLevel::ALL
            .into_iter()
            .map(|level| {
                let offered = matches!(level, ThinkingLevel::Low | ThinkingLevel::High);
                (level.as_str().to_string(), offered.then(|| level.as_str().to_string()))
            })
            .collect();
        let model = Model {
            id: "grok-4.7".into(),
            reasoning: true,
            thinking_level_map: Some(map),
            compat: Some(compat),
            ..Model::default()
        };
        let pick = |level| {
            let (slug, requested, params, _) = route(&model, level, false);
            (slug, requested, params[0].1.clone())
        };
        assert_eq!(pick(Some(ThinkingLevel::High)), ("grok-4.7-high".into(), "grok-4.7".into(), "high".into()));
        assert_eq!(pick(Some(ThinkingLevel::Medium)).0, "grok-4.7-high");
        assert_eq!(pick(Some(ThinkingLevel::Off)).0, "grok-4.7-low");
        assert_eq!(pick(None).0, "grok-4.7-low");
        assert_eq!(route(&model, Some(ThinkingLevel::High), true).0, "grok-4.7-high", "no cursorFast");
    }

    #[test]
    fn fast_picks_the_fast_route() {
        let params =
            |effort: &str| json!([{"id": "reasoning_effort", "value": effort}, {"id": "fast", "value": "true"}]);
        let fast = |levels: bool| {
            let mut set = json!({"slug": "grok-4.7-high-fast", "parameters": params("high")});
            if levels {
                set["levels"] = json!({"low": {"slug": "grok-4.7-low-fast", "parameters": params("low")},
                    "high": {"slug": "grok-4.7-high-fast", "parameters": params("high")}});
            }
            set
        };
        let map: BTreeMap<String, Option<String>> = ThinkingLevel::ALL
            .into_iter()
            .map(|level| {
                let offered = matches!(level, ThinkingLevel::Low | ThinkingLevel::High);
                (level.as_str().to_string(), offered.then(|| level.as_str().to_string()))
            })
            .collect();
        let model = |levels: bool| Model {
            id: "grok-4.7".into(),
            reasoning: true,
            thinking_level_map: Some(map.clone()),
            compat: Some(json!({"cursorModel": "grok-4.7", "cursorSlug": "grok-4.7-high", "cursorFast": fast(levels),
                "cursorLevels": {"low": {"slug": "grok-4.7-low", "parameters": []}}})),
            ..Model::default()
        };
        let with_levels = model(true);
        let (slug, requested, params, _) = route(&with_levels, Some(ThinkingLevel::Low), true);
        assert_eq!((slug.as_str(), requested.as_str()), ("grok-4.7-low-fast", "grok-4.7"));
        assert_eq!(params[1], ("fast".to_string(), "true".to_string()));
        assert_eq!(route(&with_levels, Some(ThinkingLevel::Low), false).0, "grok-4.7-low");
        assert_eq!(route(&model(false), Some(ThinkingLevel::Low), true).0, "grok-4.7-high-fast");
    }
}
