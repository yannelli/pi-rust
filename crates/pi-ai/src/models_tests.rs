use super::*;
use crate::types::ThinkingLevel;

#[test]
fn bundled_catalog_parses() {
    assert!(!builtin_models().is_empty());
    assert!(builtin_models().iter().all(|model| model.provider == "azure" || !model.base_url.is_empty()));
}

#[test]
fn models_json_adds_providers_and_overrides() {
    let mut registry = ModelRegistry::builtin();
    let anthropic = registry.models().iter().find(|model| model.provider == "anthropic").unwrap().id.clone();
    registry
        .apply_json(&format!(
            r#"{{"providers":{{"ollama":{{"baseUrl":"http://localhost:11434/v1","api":"openai-completions","apiKey":"ollama","models":[{{"id":"qwen2.5-coder:7b"}}]}},
            "anthropic":{{"modelOverrides":{{"{anthropic}":{{"maxTokens":1234}}}}}}}}}}"#
        ))
        .unwrap();
    let ollama = registry.find("ollama", "qwen2.5-coder:7b").unwrap();
    assert_eq!(ollama.base_url, "http://localhost:11434/v1");
    assert_eq!((ollama.context_window, ollama.max_tokens), (128_000, 16_384));
    assert_eq!(registry.api_key("ollama").as_deref(), Some("ollama"));
    assert_eq!(registry.find("anthropic", &anthropic).unwrap().max_tokens, 1234);
    assert_eq!(registry.resolve("ollama/qwen2.5-coder:7b", None).unwrap().provider, "ollama");
    assert_eq!(registry.resolve("QWEN", Some("ollama")).unwrap().id, "qwen2.5-coder:7b");
}

#[test]
fn models_json_extras_parse_into_model_fields() {
    let mut registry = ModelRegistry::builtin();
    let text = r#"{
      // comments are allowed, as upstream strips them
      "providers": {
        "local": {
          "baseUrl": "http://localhost:8080/v1", "api": "openai-completions", "apiKey": "$LOCAL_KEY",
          "headers": {"X-Team": "${TEAM}"}, "authHeader": true, "compat": {"supportsStore": false},
          "models": [
            {"id": "chat", "reasoning": true, "contextWindow": 32000,
             "thinkingLevelMap": {"minimal": null, "xhigh": "x-high", "max": "maximum"},
             "inputLimits": {"maxRequestBytes": 1000, "images": {"maxPerMessage": 2, "resize": {"maxWidth": 512}}},
             "promptCache": {"short": 300}, "samplingParams": {"temperature": 0.2},
             "samplingParamsByThinkingLevel": {"high": {"top_p": 0.9}},
             "headers": {"X-Model": "chat"}, "compat": {"thinkingFormat": "qwen"}},
            {"id": "painter", "type": "image", "api": "openrouter-images", "input": ["text", "image"], "output": ["image"]},
            {"id": "judge", "type": "classifier", "api": "openai-decisions"}
          ],
          "modelOverrides": {"chat": {"samplingParams": {"top_k": 40}, "inputLimits": {"images": {"resize": {"maxHeight": 256}}},
            "samplingParamsByThinkingLevel": {"high": {"min_p": 0.1}}, "headers": {"X-Override": "yes"}}}
        }
      }
    }"#;
    registry.apply_json(text).unwrap();
    let chat = registry.find("local", "chat").unwrap();
    assert_eq!(chat.context_window, 32_000);
    assert_eq!(chat.prompt_cache.as_ref().unwrap()["short"], 300.0);
    let sampling = chat.sampling_params.as_ref().unwrap();
    assert_eq!((sampling["temperature"].as_f64(), sampling["top_k"].as_i64()), (Some(0.2), Some(40)));
    let high = &chat.sampling_params_by_thinking_level.as_ref().unwrap()["high"];
    assert_eq!((high["top_p"].as_f64(), high["min_p"].as_f64()), (Some(0.9), Some(0.1)));
    let limits = chat.input_limits.unwrap();
    let resize = limits.images.unwrap().resize.unwrap();
    assert_eq!((limits.max_request_bytes, resize.max_width, resize.max_height), (Some(1000), Some(512), Some(256)));
    let compat = chat.compat.as_ref().unwrap();
    assert_eq!((compat["supportsStore"].as_bool(), compat["thinkingFormat"].as_str()), (Some(false), Some("qwen")));
    assert_eq!(
        supported_thinking_levels(chat),
        [
            ThinkingLevel::Off,
            ThinkingLevel::Low,
            ThinkingLevel::Medium,
            ThinkingLevel::High,
            ThinkingLevel::Xhigh,
            ThinkingLevel::Max
        ]
    );
    assert!(registry.find("local", "painter").is_none());
    let painter = registry.all_models().iter().find(|model| model.id == "painter").unwrap();
    assert_eq!(
        (painter.model_type.as_deref(), painter.output.as_deref()),
        (Some("image"), Some(&["image".to_string()][..]))
    );
    assert!(registry.models().iter().all(|model| model.id != "judge"));
    let auth = &registry.auth_configs()["local"];
    assert_eq!(
        (auth.api_key.as_deref(), auth.auth_header, auth.headers["X-Team"].as_str()),
        (Some("$LOCAL_KEY"), true, "${TEAM}")
    );
    assert_eq!(auth.model_headers["chat"]["X-Model"], "chat");
    assert_eq!(auth.model_headers["chat"]["X-Override"], "yes");
}

#[test]
fn provider_base_url_applies_to_builtin_models_and_errors_leave_the_catalog_unchanged() {
    let mut registry = ModelRegistry::builtin();
    registry.apply_json(r#"{"providers":{"openai":{"baseUrl":"https://proxy.example/v1"}}}"#).unwrap();
    assert!(
        registry
            .models()
            .iter()
            .filter(|model| model.provider == "openai")
            .all(|model| model.base_url == "https://proxy.example/v1")
    );
    let before = registry.all_models().len();
    let error = registry.apply_json(r#"{"providers":{"empty":{}}}"#).unwrap_err();
    assert!(error.contains("must specify"), "{error}");
    let error = registry.apply_json(r#"{"providers":{"x":{"baseUrl":"http://x","api":"openai-completions","models":[{"id":"m","maxTokens":0}]}}}"#).unwrap_err();
    assert!(error.contains("invalid maxTokens"), "{error}");
    assert_eq!(registry.all_models().len(), before);
}

#[test]
fn thinking_levels_clamp_to_model_support() {
    let mut model = Model { reasoning: false, ..Model::default() };
    assert_eq!(clamp_thinking_level(&model, ThinkingLevel::High), ThinkingLevel::Off);
    model.reasoning = true;
    assert_eq!(clamp_thinking_level(&model, ThinkingLevel::Max), ThinkingLevel::High);
    assert_eq!(clamp_thinking_level(&model, ThinkingLevel::Medium), ThinkingLevel::Medium);
    let map = [("minimal", None), ("low", None), ("max", Some("max".to_string()))];
    model.thinking_level_map = Some(map.into_iter().map(|(k, v)| (k.to_string(), v)).collect());
    assert_eq!(clamp_thinking_level(&model, ThinkingLevel::Minimal), ThinkingLevel::Medium);
    assert_eq!(clamp_thinking_level(&model, ThinkingLevel::Xhigh), ThinkingLevel::Max);
    model.thinking_level_map = Some([("off".to_string(), None)].into_iter().collect());
    assert_eq!(clamp_thinking_level(&model, ThinkingLevel::Off), ThinkingLevel::Minimal);
}

#[test]
fn config_values_read_environment_and_commands() {
    assert_eq!(resolve_config_value("literal").as_deref(), Some("literal"));
    #[cfg(unix)]
    assert_eq!(resolve_config_value("!printf secret").as_deref(), Some("secret"));
    assert_eq!(resolve_config_value("$PI_RUST_SURELY_UNSET_VARIABLE"), None);
}
