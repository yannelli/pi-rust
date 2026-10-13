use super::*;

fn registry_with_overrides() -> ModelRegistry {
    let mut registry = ModelRegistry::default();
    let models = json!({"providers": {"demo": {"baseUrl": "http://models-json", "api": "openai-completions",
        "models": [{"id": "base", "contextWindow": 1000, "maxTokens": 100}],
        "modelOverrides": {"ext": {"name": "Overridden", "maxTokens": 77}}}}});
    registry.apply_json(&models.to_string()).unwrap();
    registry
}

#[test]
fn extension_models_replace_the_provider_models() {
    let mut registry = ModelRegistry::builtin();
    let config = json!({
        "baseUrl": "http://localhost:1",
        "api": "custom-api",
        "apiKey": "key",
        "models": [{"id": "only", "name": "Only", "contextWindow": 1000, "maxTokens": 100}],
    });
    apply_extension_provider(&mut registry, "anthropic", &config, None).unwrap();
    let models: Vec<&str> = registry
        .all_models()
        .iter()
        .filter(|model| model.provider == "anthropic")
        .map(|model| model.id.as_str())
        .collect();
    assert_eq!(models, vec!["only"]);
    let model = registry.find("anthropic", "only").unwrap();
    assert_eq!((model.api.as_str(), model.base_url.as_str()), ("custom-api", "http://localhost:1"));
}

#[test]
fn extension_definitions_are_taken_as_given_with_defaults_by_type() {
    let mut registry = registry_with_overrides();
    let image = json!({"providers": {"demo": {"models": [
        {"id": "painter", "type": "image", "api": "demo-images", "baseUrl": "http://images", "output": ["image"]}]}}});
    registry.apply_json(&image.to_string()).unwrap();
    let config = json!({"models": [
        {"id": "ext", "name": "Ext", "reasoning": true, "headers": {"x-model": "1"}},
        {"id": "pic", "name": "Pic", "type": "image", "output": ["image"]},
    ]});
    apply_extension_provider(&mut registry, "demo", &config, None).unwrap();
    let chat = registry.find("demo", "ext").unwrap();
    assert_eq!((chat.api.as_str(), chat.base_url.as_str()), ("openai-completions", "http://models-json"));
    assert_eq!((chat.context_window, chat.max_tokens), (0, 0), "no chat defaults are invented");
    assert!(chat.headers.is_empty());
    assert_eq!(registry.auth_configs()["demo"].model_headers["ext"]["x-model"], "1");
    let image = registry.all_models().iter().find(|model| model.id == "pic").unwrap().clone();
    assert_eq!((image.api.as_str(), image.base_url.as_str()), ("demo-images", "http://images"));

    let missing = json!({"models": [{"id": "cls", "type": "classifier"}]});
    let error = apply_extension_provider(&mut registry_with_overrides(), "demo", &missing, None).unwrap_err();
    assert_eq!(error, "Provider demo, model cls: no \"api\" specified. Set it at model level.");
}

#[test]
fn model_overrides_apply_after_extension_models() {
    let mut registry = registry_with_overrides();
    let config = json!({"models": [{"id": "ext", "name": "Ext", "contextWindow": 5000, "maxTokens": 500}]});
    apply_extension_provider(&mut registry, "demo", &config, None).unwrap();
    assert_eq!(registry.find("demo", "ext").unwrap().name, "Ext");
    registry.apply_model_overrides("demo").unwrap();
    let model = registry.find("demo", "ext").unwrap();
    assert_eq!((model.name.as_str(), model.max_tokens, model.context_window), ("Overridden", 77, 5000));
}

#[test]
fn extension_auth_layers_over_models_json() {
    let mut registry = ModelRegistry::default();
    let models = json!({"providers": {"demo": {"baseUrl": "http://x", "api": "openai-completions", "apiKey": "FILE_KEY",
        "headers": {"a": "1", "b": "1"}, "authHeader": true, "models": [{"id": "m"}]}}});
    registry.apply_json(&models.to_string()).unwrap();
    apply_extension_provider(&mut registry, "demo", &json!({"headers": {"b": "2"}, "name": "Demo"}), None).unwrap();
    let auth = &registry.auth_configs()["demo"];
    assert_eq!(auth.api_key.as_deref(), Some("FILE_KEY"));
    assert_eq!((auth.headers["a"].as_str(), auth.headers["b"].as_str(), auth.auth_header), ("1", "2", true));
    assert_eq!(registry.provider_name("demo"), Some("Demo"));
}
