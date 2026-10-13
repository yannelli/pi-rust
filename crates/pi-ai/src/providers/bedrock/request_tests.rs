use std::collections::BTreeMap;

use serde_json::{Value, json};

use super::bedrock_model;
use crate::providers::bedrock::request::build_body;
use crate::providers::bedrock::resolve_target;
use crate::registry::{CacheRetention, StreamOptions};
use crate::types::{
    AssistantMessage, ContentBlock, Context, Message, Model, StopReason, ThinkingLevel, Tool, ToolCall,
    ToolResultMessage, UserContent, UserMessage,
};

const BINDING_BETA: &str = "thinking-binding-controls-2026-08-01";

fn hello() -> Context {
    Context { messages: vec![Message::user("Hello")], ..Context::default() }
}

fn fields(model: &Model, level: ThinkingLevel, region: Option<&str>) -> Value {
    let options =
        StreamOptions { reasoning: Some(level), cache_retention: Some(CacheRetention::None), ..Default::default() };
    build_body(model, &hello(), &options, region).unwrap()["additionalModelRequestFields"].clone()
}

#[test]
fn adaptive_claude_thinking_uses_effort_and_block_binding() {
    let adaptive = json!({"type": "adaptive", "display": "summarized", "block_binding": {"prefix_mismatch_behavior": "drop_block"}});
    for id in ["us.anthropic.claude-opus-4-8", "global.anthropic.claude-fable-5", "global.anthropic.claude-sonnet-5"] {
        let fields = fields(&bedrock_model(id, ""), ThinkingLevel::High, None);
        assert_eq!(fields["thinking"], adaptive, "{id}");
        assert_eq!(fields["output_config"], json!({"effort": "high"}));
        assert_eq!(fields["anthropic_beta"], json!([BINDING_BETA]));
    }
    let xhigh = fields(&bedrock_model("global.anthropic.claude-opus-5", ""), ThinkingLevel::Xhigh, None);
    assert_eq!(xhigh["output_config"], json!({"effort": "xhigh"}));
    for id in ["global.anthropic.claude-opus-4-6-v1", "global.anthropic.claude-sonnet-4-6"] {
        let fields = fields(&bedrock_model(id, ""), ThinkingLevel::High, None);
        assert_eq!(fields["thinking"], json!({"type": "adaptive", "display": "summarized"}), "{id}");
        assert!(fields.get("anthropic_beta").is_none());
    }
    let profile = Model {
        name: "Claude Opus 4.8".into(),
        ..bedrock_model("arn:aws:bedrock:us-east-1:1:application-inference-profile/x", "")
    };
    assert_eq!(fields(&profile, ThinkingLevel::High, None)["output_config"], json!({"effort": "high"}));
}

#[test]
fn gov_cloud_omits_display_and_budget_models_get_interleaved_thinking() {
    let gov = Model {
        name: "Claude Sonnet 4.5 (GovCloud)".into(),
        ..bedrock_model("us-gov.anthropic.claude-sonnet-4-5-20250929-v1:0", "")
    };
    let fields = fields(&gov, ThinkingLevel::High, None);
    assert_eq!(fields["thinking"], json!({"type": "enabled", "budget_tokens": 16384}));
    assert_eq!(fields["anthropic_beta"], json!(["interleaved-thinking-2025-05-14"]));
    let adaptive = fields_for_region("global.anthropic.claude-opus-4-8-v1", "us-gov-west-1");
    assert_eq!(adaptive["thinking"], json!({"type": "adaptive"}));
    assert!(adaptive.get("anthropic_beta").is_none());

    let sonnet = bedrock_model("us.anthropic.claude-sonnet-4-5-20250929-v1:0", "");
    let options =
        StreamOptions { reasoning: Some(ThinkingLevel::Medium), max_tokens: Some(4000), ..Default::default() };
    let body = build_body(&sonnet, &hello(), &options, None).unwrap();
    assert_eq!(body["inferenceConfig"]["maxTokens"], 12_192);
    assert_eq!(body["additionalModelRequestFields"]["thinking"]["budget_tokens"], 8192);
    let tight = StreamOptions { max_tokens: Some(64_000), ..options };
    let body = build_body(&sonnet, &hello(), &tight, None).unwrap();
    assert_eq!(body["inferenceConfig"]["maxTokens"], 64_000);
}

fn fields_for_region(id: &str, region: &str) -> Value {
    fields(&bedrock_model(id, ""), ThinkingLevel::High, Some(region))
}

#[test]
fn openai_models_get_reasoning_effort_and_off_sends_nothing() {
    let cases = [
        (ThinkingLevel::Minimal, "low"),
        (ThinkingLevel::Medium, "medium"),
        (ThinkingLevel::Xhigh, "xhigh"),
        (ThinkingLevel::Max, "max"),
    ];
    for (level, effort) in cases {
        let fields = fields(&bedrock_model("global.openai.gpt-6-sol", ""), level, None);
        assert_eq!(fields, json!({"reasoning": {"effort": effort}}));
    }
    let oss = bedrock_model("openai.gpt-oss-120b-1:0", "");
    assert_eq!(fields(&oss, ThinkingLevel::Minimal, None), json!({"reasoning_effort": "low"}));
    assert_eq!(fields(&oss, ThinkingLevel::Xhigh, None), json!({"reasoning_effort": "high"}));
    assert_eq!(fields(&bedrock_model("us.anthropic.claude-opus-4-8", ""), ThinkingLevel::Off, None), Value::Null);
}

fn assistant(content: Vec<ContentBlock>, model: &str) -> Message {
    Message::Assistant(AssistantMessage {
        content,
        api: "bedrock-converse-stream".into(),
        provider: "amazon-bedrock".into(),
        model: model.into(),
        stop_reason: StopReason::ToolUse,
        ..Default::default()
    })
}

fn result(id: &str, content: Vec<ContentBlock>, is_error: bool) -> Message {
    Message::ToolResult(ToolResultMessage {
        tool_call_id: id.into(),
        tool_name: "edit".into(),
        content,
        is_error,
        ..Default::default()
    })
}

#[test]
fn converts_messages_with_placeholders_grouped_results_and_replay() {
    let id = "us.anthropic.claude-opus-4-8";
    let mut arguments = serde_json::Map::new();
    arguments.insert("path".into(), json!("a.js"));
    arguments.insert("edits".into(), json!([{"oldText": "x", "": ""}]));
    let call = |id: &str| {
        ContentBlock::ToolCall(ToolCall {
            id: id.into(),
            name: "edit".into(),
            arguments: arguments.clone(),
            ..Default::default()
        })
    };
    let messages = vec![
        Message::User(UserMessage { content: UserContent::Text("  ".into()), timestamp: 0 }),
        assistant(
            vec![
                ContentBlock::Thinking {
                    thinking: "secret".into(),
                    thinking_signature: Some("AAEC".into()),
                    redacted: true,
                },
                ContentBlock::Thinking {
                    thinking: "plan".into(),
                    thinking_signature: Some("sig".into()),
                    redacted: false,
                },
                ContentBlock::Thinking { thinking: "unsigned".into(), thinking_signature: None, redacted: false },
                ContentBlock::text(" "),
                call("t1"),
                call("t2"),
            ],
            id,
        ),
        result("t1", vec![ContentBlock::text("")], false),
        result(
            "t2",
            vec![ContentBlock::text("bad"), ContentBlock::Image { data: "AA==".into(), mime_type: "image/png".into() }],
            true,
        ),
        assistant(Vec::new(), id),
    ];
    let options = StreamOptions { cache_retention: Some(CacheRetention::Long), ..Default::default() };
    let context = Context { messages, ..Context::default() };
    let body = build_body(&bedrock_model(id, ""), &context, &options, None).unwrap();
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 3);
    assert_eq!(messages[0]["content"], json!([{"text": "<empty>"}]));
    let content = &messages[1]["content"];
    assert_eq!(content[0], json!({"reasoningContent": {"redactedContent": "AAEC"}}));
    assert_eq!(content[1], json!({"reasoningContent": {"reasoningText": {"text": "plan", "signature": "sig"}}}));
    assert_eq!(content[2], json!({"text": "unsigned"}));
    assert_eq!(content[3]["toolUse"]["input"], json!({"path": "a.js", "edits": [{"oldText": "x"}]}));
    let results = messages[2]["content"].as_array().unwrap();
    assert_eq!(
        results[0]["toolResult"],
        json!({"toolUseId": "t1", "content": [{"text": "<empty>"}], "status": "success"})
    );
    assert_eq!(results[1]["toolResult"]["status"], "error");
    assert_eq!(
        results[1]["toolResult"]["content"][1],
        json!({"image": {"format": "png", "source": {"bytes": "AA=="}}})
    );
    assert_eq!(results[2], json!({"cachePoint": {"type": "default", "ttl": "1h"}}));
}

#[test]
fn tools_honor_strict_mode_tool_choice_and_cache_support() {
    let tool = Tool {
        constrained_sampling: Some(json!({"type": "json_schema", "strict": "require"})),
        ..Tool::new(
            "lookup",
            "Look up",
            json!({"type": "object", "properties": {"value": {"type": "string"}}, "required": ["value"]}),
        )
    };
    let context = Context { tools: vec![tool], ..hello() };
    let strict_model = Model {
        compat: Some(json!({"supportsStrictMode": true})),
        ..bedrock_model("us.anthropic.claude-opus-4-8", "")
    };
    let options = StreamOptions { tool_choice: Some("auto".into()), ..Default::default() };
    let body = build_body(&strict_model, &context, &options, None).unwrap();
    assert_eq!(body["toolConfig"]["tools"][0]["toolSpec"]["strict"], true);
    assert_eq!(body["toolConfig"]["tools"][0]["toolSpec"]["inputSchema"]["json"]["additionalProperties"], false);
    assert_eq!(body["toolConfig"]["toolChoice"], json!({"auto": {}}));
    let nova = bedrock_model("amazon.nova-pro-v1:0", "");
    let error = build_body(&nova, &context, &options, None).unwrap_err();
    assert!(error.contains("strict tools are unsupported"), "{error}");
    let none = StreamOptions { tool_choice: Some("none".into()), ..Default::default() };
    let mut prefer = context.clone();
    prefer.tools[0].constrained_sampling = Some(json!({"type": "json_schema", "strict": "prefer"}));
    assert!(build_body(&nova, &prefer, &none, None).unwrap().get("toolConfig").is_none());
    let body = build_body(&nova, &prefer, &StreamOptions::default(), None).unwrap();
    assert!(body["toolConfig"]["tools"][0]["toolSpec"].get("strict").is_none());
    assert_eq!(body["messages"][0]["content"].as_array().unwrap().len(), 1);
    let forced =
        StreamOptions { env: BTreeMap::from([("AWS_BEDROCK_FORCE_CACHE".into(), "1".into())]), ..Default::default() };
    assert_eq!(
        build_body(&nova, &hello(), &forced, None).unwrap()["messages"][0]["content"][1]["cachePoint"]["type"],
        "default"
    );
}

#[test]
fn resolves_regions_and_endpoints_like_the_sdk_config() {
    let eu = bedrock_model(
        "eu.anthropic.claude-sonnet-4-5-20250929-v1:0",
        "https://bedrock-runtime.eu-central-1.amazonaws.com",
    );
    let target = resolve_target(&eu, &BTreeMap::new()).unwrap();
    assert_eq!(
        (target.endpoint.as_str(), target.region.as_str()),
        ("https://bedrock-runtime.eu-central-1.amazonaws.com", "eu-central-1")
    );
    let scoped = BTreeMap::from([("AWS_PROFILE".to_string(), "scoped".to_string())]);
    let target = resolve_target(&eu, &scoped).unwrap();
    assert_eq!((target.region.as_str(), target.profile.as_deref()), ("eu-central-1", Some("scoped")));

    let us = bedrock_model("us.anthropic.claude-opus-4-8", "https://bedrock-runtime.us-east-1.amazonaws.com");
    let env = BTreeMap::from([("AWS_REGION".to_string(), "us-east-2".to_string())]);
    let target = resolve_target(&us, &env).unwrap();
    assert_eq!(
        (target.endpoint.as_str(), target.region.as_str()),
        ("https://bedrock-runtime.us-east-2.amazonaws.com", "us-east-2")
    );
    let custom = Model { base_url: "https://bedrock-vpc.example.com".into(), ..us.clone() };
    let env = BTreeMap::from([("AWS_REGION".to_string(), "us-west-2".to_string())]);
    let target = resolve_target(&custom, &env).unwrap();
    assert_eq!((target.endpoint.as_str(), target.region.as_str()), ("https://bedrock-vpc.example.com", "us-west-2"));
    let arn = Model { id: "arn:aws-us-gov:bedrock:us-gov-west-1:1:application-inference-profile/abc".into(), ..us };
    let env = BTreeMap::from([("AWS_REGION".to_string(), "us-east-1".to_string())]);
    assert_eq!(resolve_target(&arn, &env).unwrap().region, "us-gov-west-1");
}
