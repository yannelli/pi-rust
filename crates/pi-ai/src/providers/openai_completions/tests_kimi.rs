use serde_json::{Value, json};

use super::OpenAICompletions;
use super::test_support::{API, capture, ok_frames, options, system, user_context};
use crate::providers::mock_server::{MockResponse, MockServer, test_model};
use crate::registry::ApiProvider;
use crate::types::{Context, Message, StopReason, Tool};

fn parameters() -> Value {
    json!({"type": "object", "properties": {
        "mode": {"enum": ["auto", "manual"], "description": "Execution mode"},
        "options": {"properties": {"enabled": {"const": true}}},
        "choices": {"items": {"enum": [1, 2.5]}},
        "optional": {"anyOf": [{"const": "none"}, {"type": "null"}]}
    }, "required": ["mode"], "additionalProperties": false})
}

#[tokio::test]
async fn kimi_enum_only_mode_is_accepted_by_the_mock_provider() {
    let mut server = MockServer::routed(|request| {
        let schema = &request.body["tools"][0]["function"]["parameters"];
        if schema["properties"]["mode"]["type"] != "string" {
            return MockResponse::Status(400, json!({"error": {"message":
                "tools.function.parameters is not a valid moonshot flavored json schema: At path 'properties.mode': type is not defined"
            }}).to_string());
        }
        MockResponse::Sse(ok_frames())
    }).await;
    let model = test_model(API, "kimi-coding", &server.base_url);
    let tool = Tool::new("choose_mode", "Choose a mode", parameters());
    let context = Context { tools: vec![tool], ..user_context("Choose auto") };
    let result = OpenAICompletions.stream(&model, &context, options()).result(&model).await;
    assert_eq!(result.stop_reason, StopReason::Stop, "{:?}", result.error_message);
    let request = server.request().await;
    let schema = &request.body["tools"][0]["function"]["parameters"];
    assert_eq!(
        schema["properties"]["mode"],
        json!({"type": "string", "enum": ["auto", "manual"], "description": "Execution mode"})
    );
    assert_eq!(schema["properties"]["options"]["type"], "object");
    assert_eq!(schema["properties"]["options"]["properties"]["enabled"]["type"], "boolean");
    assert_eq!(schema["properties"]["choices"]["type"], "array");
    assert_eq!(schema["properties"]["choices"]["items"]["type"], "number");
    assert_eq!(schema["properties"]["optional"]["anyOf"][0]["type"], "string");
    assert!(schema["properties"]["optional"].get("type").is_none());
    assert_eq!(schema["required"], json!(["mode"]));
    assert_eq!(schema["additionalProperties"], false);
    assert_eq!(context.tools[0].parameters, parameters());
}

#[tokio::test]
async fn kimi_schema_normalization_covers_regions_moonshot_and_late_tools_only() {
    let schema = parameters();
    for (provider, url, normalize) in [
        ("kimi-coding", "http://127.0.0.1:9/v1", true),
        ("moonshotai", "http://127.0.0.1:9/v1", true),
        ("moonshotai-cn", "http://127.0.0.1:9/v1", true),
        ("custom", "https://api.kimi.ai/coding/v1", true),
        ("custom", "https://api.kimi.com/coding/v1", true),
        ("openai", "http://127.0.0.1:9/v1", false),
        ("custom", "https://api.kimi.ai.example/v1", false),
    ] {
        let mut model = test_model(API, provider, url);
        model.compat = Some(json!({"supportsMidConvoSystemMessages": true, "supportsMidConvoToolAdditions": true}));
        let tool = Tool::new("choose_mode", "Choose a mode", schema.clone());
        let context = Context {
            messages: vec![
                system(json!({"role": "system", "content": "Base", "toolsAdded": [tool.clone()], "timestamp": 0})),
                Message::user("before"),
                system(
                    json!({"role": "system", "content": "Update", "toolsAdded": [Tool::new("late", "Late tool", schema.clone())], "timestamp": 1}),
                ),
            ],
            ..Context::default()
        };
        let body = capture(&model, &context, options()).await;
        let late = body["messages"].as_array().unwrap().iter().find_map(|message| message.get("tools")).unwrap();
        for tools in [&body["tools"], late] {
            let observed = &tools[0]["function"]["parameters"];
            if normalize {
                assert_eq!(observed["properties"]["mode"]["type"], "string", "{provider} {url}");
            } else {
                assert_eq!(observed, &schema, "{provider} {url}");
            }
        }
    }
}

#[test]
fn kimi_schema_walks_schema_slots_without_changing_data_or_constraints() {
    let mut schema = json!({"type": "object", "$defs": {"Mode": {"enum": ["a", "b"]}},
        "properties": {"fixed": {"type": "number", "minimum": 0}, "ref": {"$ref": "#/$defs/Mode"},
            "free": {}, "mixed": {"enum": ["one", 2]}, "mixedKeywords": {"minimum": 1, "maxLength": 5},
            "literal": {"default": {"enum": ["data"]}}},
        "default": {"properties": {"data": {"enum": ["untouched"]}}}});
    let before = schema.clone();
    super::kimi_schema::normalize(&mut schema);
    assert_eq!(schema["$defs"]["Mode"]["type"], "string");
    assert_eq!(schema["properties"], before["properties"]);
    assert_eq!(schema["default"], before["default"]);
}
