use std::collections::HashSet;

use super::*;

fn model(provider: &str, id: &str, name: &str, reasoning: bool) -> Model {
    Model {
        id: id.into(),
        name: name.into(),
        api: "anthropic-messages".into(),
        provider: provider.into(),
        base_url: "https://example.invalid".into(),
        reasoning,
        context_window: 128_000,
        max_tokens: 8192,
        ..Model::default()
    }
}

fn all_models() -> Vec<Model> {
    vec![
        model("anthropic", "claude-sonnet-4-5", "Claude Sonnet 4.5", true),
        model("openai", "gpt-4o", "GPT-4o", false),
        model("openrouter", "qwen/qwen3-coder:exacto", "Qwen3 Coder Exacto", true),
        model("openrouter", "openai/gpt-4o:extended", "GPT-4o Extended", false),
    ]
}

struct Catalog {
    models: Vec<Model>,
    authenticated: Option<HashSet<String>>,
}

impl Catalog {
    fn new(models: Vec<Model>) -> Self {
        Self { models, authenticated: None }
    }

    fn with_auth(models: Vec<Model>, providers: &[&str]) -> Self {
        Self { models, authenticated: Some(providers.iter().map(|p| p.to_string()).collect()) }
    }
}

impl ModelCatalog for Catalog {
    fn all_chat_models(&self) -> Vec<Model> {
        self.models.clone()
    }

    fn has_configured_auth(&self, provider: &str) -> bool {
        self.authenticated.as_ref().is_some_and(|set| set.contains(provider))
    }
}

fn parse(pattern: &str) -> ParsedModelResult {
    parse_model_pattern(pattern, &all_models(), true)
}

fn id(result: &ParsedModelResult) -> Option<&str> {
    result.model.as_ref().map(|model| model.id.as_str())
}

#[test]
fn parse_model_pattern_matches_upstream_cases() {
    let exact = parse("claude-sonnet-4-5");
    assert_eq!((id(&exact), exact.thinking_level, exact.warning.as_deref()), (Some("claude-sonnet-4-5"), None, None));
    assert_eq!(id(&parse("sonnet")), Some("claude-sonnet-4-5"));
    assert_eq!(parse("nonexistent"), ParsedModelResult::default());
    for level in ThinkingLevel::ALL {
        let result = parse(&format!("sonnet:{}", level.as_str()));
        assert_eq!((id(&result), result.thinking_level), (Some("claude-sonnet-4-5"), Some(level)));
    }
    assert_eq!(parse("gpt-4o:medium").thinking_level, Some(ThinkingLevel::Medium));
    let invalid = parse("sonnet:random");
    assert_eq!((id(&invalid), invalid.thinking_level), (Some("claude-sonnet-4-5"), None));
    assert!(invalid.warning.unwrap().contains("Invalid thinking level \"random\""));
    assert_eq!(id(&parse("qwen/qwen3-coder:exacto")), Some("qwen/qwen3-coder:exacto"));
    let prefixed = parse("openrouter/qwen/qwen3-coder:exacto:high");
    assert_eq!((id(&prefixed), prefixed.thinking_level), (Some("qwen/qwen3-coder:exacto"), Some(ThinkingLevel::High)));
    assert_eq!(id(&parse("openai/gpt-4o:extended")), Some("openai/gpt-4o:extended"));
    let nested = parse("qwen/qwen3-coder:exacto:high:random");
    assert_eq!((id(&nested), nested.thinking_level), (Some("qwen/qwen3-coder:exacto"), None));
    assert!(nested.warning.is_some());
    assert!(parse("").model.is_some());
    assert!(parse("sonnet:").warning.unwrap().contains("Invalid thinking level"));
}

#[test]
fn fuzzy_matches_prefer_aliases_then_latest_dates() {
    let models = vec![
        model("anthropic", "claude-3-5-sonnet-20240620", "Sonnet old", false),
        model("anthropic", "claude-3-5-sonnet-20241022", "Sonnet new", false),
    ];
    assert_eq!(id(&parse_model_pattern("3-5-sonnet", &models, true)), Some("claude-3-5-sonnet-20241022"));
    let mut with_alias = models.clone();
    with_alias.push(model("anthropic", "claude-3-5-sonnet-latest", "Sonnet latest", false));
    assert_eq!(id(&parse_model_pattern("3-5-sonnet", &with_alias, true)), Some("claude-3-5-sonnet-latest"));
}

#[test]
fn scope_patterns_resolve_globs_levels_and_diagnostics() {
    let patterns: Vec<String> = ["sonnet:high", "gpt-4o:invalid", "missing"].iter().map(|p| p.to_string()).collect();
    let (scoped, diagnostics) = resolve_model_scope(&patterns, &all_models());
    let ids: Vec<&str> = scoped.iter().map(|scoped| scoped.model.id.as_str()).collect();
    assert_eq!(ids, ["claude-sonnet-4-5", "gpt-4o"]);
    assert_eq!((scoped[0].thinking_level, scoped[1].thinking_level), (Some(ThinkingLevel::High), None));
    assert_eq!(diagnostics.len(), 2);
    assert_eq!(diagnostics[0].code, ModelScopeCode::InvalidThinkingLevel);
    assert_eq!(
        diagnostics[0].message,
        "Invalid thinking level \"invalid\" in pattern \"gpt-4o:invalid\". Using default instead."
    );
    assert_eq!(
        (diagnostics[1].code, diagnostics[1].message.as_str()),
        (ModelScopeCode::NoMatch, "No models match pattern \"missing\"")
    );

    let globs: Vec<String> = ["openrouter/*/*:low", "*SONNET*"].iter().map(|p| p.to_string()).collect();
    let (scoped, diagnostics) = resolve_model_scope(&globs, &all_models());
    assert!(diagnostics.is_empty());
    let ids: Vec<(&str, Option<ThinkingLevel>)> =
        scoped.iter().map(|s| (s.model.id.as_str(), s.thinking_level)).collect();
    assert_eq!(ids[0], ("qwen/qwen3-coder:exacto", Some(ThinkingLevel::Low)));
    assert_eq!(ids[2], ("claude-sonnet-4-5", None));

    let mut models = all_models();
    models.push(model("custom", "bracketed-model[1m]", "Bracketed", true));
    let bracketed: Vec<String> = vec!["custom/bracketed-model[1m]:high".into()];
    let (scoped, diagnostics) = resolve_model_scope(&bracketed, &models);
    assert_eq!(
        (scoped[0].model.id.as_str(), scoped[0].thinking_level),
        ("bracketed-model[1m]", Some(ThinkingLevel::High))
    );
    assert!(diagnostics.is_empty());
}

fn cli(
    provider: Option<&str>,
    pattern: &str,
    thinking: Option<ThinkingLevel>,
    catalog: &Catalog,
) -> ResolveCliModelResult {
    resolve_cli_model(provider, Some(pattern), thinking, catalog)
}

fn pair(result: &ResolveCliModelResult) -> (String, String) {
    let model = result.model.as_ref().unwrap_or_else(|| panic!("no model: {:?}", result.error));
    (model.provider.clone(), model.id.clone())
}

#[test]
fn cli_model_resolution_matches_upstream_cases() {
    let catalog = Catalog::new(all_models());
    let as_pair = |p: &str, i: &str| (p.to_string(), i.to_string());
    assert_eq!(pair(&cli(None, "openai/gpt-4o", None, &catalog)), as_pair("openai", "gpt-4o"));
    assert_eq!(pair(&cli(Some("openai"), "4o", None, &catalog)), as_pair("openai", "gpt-4o"));
    let with_level = cli(None, "sonnet:high", None, &catalog);
    assert_eq!(
        (pair(&with_level).1, with_level.thinking_level),
        ("claude-sonnet-4-5".into(), Some(ThinkingLevel::High))
    );
    assert_eq!(
        pair(&cli(None, "openai/gpt-4o:extended", None, &catalog)),
        as_pair("openrouter", "openai/gpt-4o:extended")
    );
    assert_eq!(pair(&cli(Some("openai"), "gpt-4o:extended", None, &catalog)), as_pair("openai", "gpt-4o:extended"));
    assert_eq!(
        pair(&cli(Some("openrouter"), "openrouter/openai/ghost-model", None, &catalog)),
        as_pair("openrouter", "openai/ghost-model")
    );
    assert_eq!(pair(&cli(None, "openrouter/qwen", None, &catalog)), as_pair("openrouter", "qwen/qwen3-coder:exacto"));
    let unknown = cli(Some("nope"), "x", None, &catalog);
    assert!(unknown.error.unwrap().contains("Unknown provider \"nope\""));
    let empty = Catalog::new(Vec::new());
    assert!(cli(Some("openai"), "gpt-4o", None, &empty).error.unwrap().contains("No models available"));
    assert!(cli(None, "nothing-like-this", None, &catalog).error.unwrap().contains("not found"));
}

#[test]
fn ambiguous_and_authenticated_provider_preferences() {
    let azure = model("azure", "gpt-5.6-sol", "GPT 5.6 Sol", false);
    let codex = model("openai-codex", "gpt-5.6-sol", "GPT 5.6 Sol", false);
    let catalog = Catalog::with_auth(vec![azure.clone(), codex.clone()], &["openai-codex"]);
    assert_eq!(pair(&cli(None, "gpt-5.6-sol", None, &catalog)).0, "openai-codex");
    let catalog = Catalog::with_auth(vec![azure, codex], &[]);
    let error = cli(None, "gpt-5.6-sol", None, &catalog).error.unwrap();
    assert!(error.contains("ambiguous across providers: azure/gpt-5.6-sol, openai-codex/gpt-5.6-sol"));
    assert!(error.contains("Use --provider or provider/model"));

    let mut models = all_models();
    models.push(model("zai", "glm-5", "GLM-5", true));
    models.push(model("vercel-ai-gateway", "zai/glm-5", "GLM-5", true));
    let catalog = Catalog::with_auth(models.clone(), &["zai", "vercel-ai-gateway"]);
    assert_eq!(pair(&cli(None, "zai/glm-5", None, &catalog)), ("zai".into(), "glm-5".into()));

    models.push(model("commandcode", "xiaomi/mimo-v2.5-pro", "MiMo via Commandcode", false));
    models.push(model("xiaomi", "mimo-v2.5-pro", "MiMo", false));
    let catalog = Catalog::with_auth(models, &["commandcode"]);
    assert_eq!(pair(&cli(None, "xiaomi/mimo-v2.5-pro", None, &catalog)).0, "commandcode");
}

#[test]
fn custom_model_fallback_handles_thinking_suffixes() {
    let mut models = all_models();
    models.push(model("neuralwatt", "some-base-model", "Some Base Model", false));
    let catalog = Catalog::new(models);
    let result = cli(None, "neuralwatt/zai-org/GLM-5.1-FP8:high", None, &catalog);
    let resolved = result.model.clone().unwrap();
    assert_eq!(
        (resolved.provider.as_str(), resolved.id.as_str(), resolved.reasoning),
        ("neuralwatt", "zai-org/GLM-5.1-FP8", true)
    );
    assert_eq!(result.thinking_level, Some(ThinkingLevel::High));
    assert!(result.warning.unwrap().contains("Using custom model id"));
    let plain = cli(None, "neuralwatt/zai-org/GLM-5.1-FP8", None, &catalog);
    assert_eq!((pair(&plain).1, plain.thinking_level), ("zai-org/GLM-5.1-FP8".into(), None));
    for level in ThinkingLevel::ALL {
        let result = cli(None, &format!("neuralwatt/zai-org/GLM-5.1-FP8:{}", level.as_str()), None, &catalog);
        assert_eq!((pair(&result).1, result.thinking_level), ("zai-org/GLM-5.1-FP8".into(), Some(level)));
    }
    assert_eq!(
        pair(&cli(None, "neuralwatt/zai-org/GLM-5.1-FP8:banana", None, &catalog)).1,
        "zai-org/GLM-5.1-FP8:banana"
    );
    let explicit = cli(Some("neuralwatt"), "zai-org/GLM-5.1-FP8:high", None, &catalog);
    assert_eq!((pair(&explicit).1, explicit.thinking_level), ("zai-org/GLM-5.1-FP8".into(), Some(ThinkingLevel::High)));
    let with_flag = cli(None, "neuralwatt/zai-org/GLM-5.1-FP8:high", Some(ThinkingLevel::Medium), &catalog);
    assert_eq!((pair(&with_flag).1, with_flag.thinking_level), ("zai-org/GLM-5.1-FP8:high".into(), None));
}

#[test]
fn default_model_selection_follows_upstream_priority() {
    assert_eq!(default_model_for("openai"), Some("gpt-5.5"));
    assert_eq!(default_model_for("openai-codex"), Some("gpt-6.1-sol"));
    assert_eq!(default_model_for("vercel-ai-gateway"), Some("zai/glm-5.1"));
    assert_eq!(default_model_for("ant-ling"), Some("Ring-2.6-1T"));
    for provider in pi_ai::models::builtin_models().iter().map(|model| model.provider.as_str()) {
        let default = default_model_for(provider).unwrap_or_else(|| panic!("{provider} has no default"));
        assert!(
            pi_ai::models::builtin_models().iter().any(|m| m.provider == provider && m.id == default),
            "{provider}/{default}"
        );
    }

    let catalog = Catalog::new(all_models());
    let options = InitialModelOptions {
        cli_provider: Some("openrouter"),
        cli_model: Some("openrouter/openai/ghost-model"),
        ..Default::default()
    };
    assert_eq!(find_initial_model(&options, &catalog).unwrap().model.unwrap().id, "openai/ghost-model");

    let gateway = model("vercel-ai-gateway", "anthropic/claude-opus-4-6", "Opus", true);
    let catalog =
        Catalog::with_auth(vec![model("zzz", "first", "First", false), gateway], &["vercel-ai-gateway", "zzz"]);
    let found = find_initial_model(&InitialModelOptions::default(), &catalog).unwrap();
    assert_eq!(found.model.unwrap().provider, "zzz");
    assert_eq!(found.thinking_level, ThinkingLevel::Medium);

    let saved = model("deepseek", "deepseek-v4-flash", "Flash", true);
    let local = model("spark-two", "deepseek-v4-flash", "Flash", true);
    let catalog = Catalog::with_auth(vec![saved, local], &["spark-two"]);
    let options = InitialModelOptions {
        default_provider: Some("deepseek"),
        default_model_id: Some("deepseek-v4-flash"),
        ..Default::default()
    };
    assert_eq!(find_initial_model(&options, &catalog).unwrap().model.unwrap().provider, "spark-two");

    let scoped = [ScopedModel { model: model("openai", "gpt-4o", "GPT-4o", false), thinking_level: None }];
    let levels = std::collections::BTreeMap::from([("openai/gpt-4o".to_string(), ThinkingLevel::Low)]);
    let options =
        InitialModelOptions { scoped_models: &scoped, model_thinking_levels: Some(&levels), ..Default::default() };
    assert_eq!(find_initial_model(&options, &Catalog::new(all_models())).unwrap().thinking_level, ThinkingLevel::Low);
}
