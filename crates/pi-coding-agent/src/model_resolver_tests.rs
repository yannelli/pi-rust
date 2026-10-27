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

fn reference_catalog() -> Vec<Model> {
    [
        ("anthropic", "claude-sonnet-4-5"),
        ("anthropic", "claude-sonnet-4-5-20250929"),
        ("anthropic", "claude-opus-4-8"),
        ("anthropic", "exact"),
        // Fuzzy-matches "shared"; an ambiguous exact "shared" must not fall through to it.
        ("anthropic", "shared-pro"),
        ("openai", "gpt-4o"),
        ("openrouter", "qwen/qwen3-coder:exacto"),
        ("openrouter", "openai/gpt-4o:extended"),
        ("gw-a", "shared"),
        ("gw-a", "anthropic/future"),
        ("gw-a", "anthropic/exact"),
        ("gw-a", "anthropic/claude-opus-4.8"),
        ("gw-a", "anthropic/claude-opus"),
        ("gw-b", "shared"),
        ("gw-b", "anthropic/future"),
        ("gw-b", "claude-opus-4-8"),
        ("gw-b", "gw-b/free"),
        ("gw-b", "zz:free"),
        // URL-derived provider id containing "/" (upstream #11394).
        ("llama=http://host:10800", "unsloth/qwen-gguf:BF16"),
    ]
    .into_iter()
    .map(|(provider, id)| model(provider, id, id, true))
    .collect()
}

struct Case {
    text: &'static str,
    usable: &'static [&'static str],
    provider: Option<&'static str>,
    suffix: ThinkingSuffix,
    fuzzy: bool,
    expected: &'static str,
}

const fn case(text: &'static str, expected: &'static str) -> Case {
    Case { text, usable: &[], provider: None, suffix: ThinkingSuffix::Strict, fuzzy: true, expected }
}

fn describe(result: ModelReferenceResult) -> String {
    match result {
        ModelReferenceResult::Resolved { model, thinking_level, warning } => {
            let level = thinking_level.map(|level| format!(" :{}", level.as_str())).unwrap_or_default();
            let warning = warning.map(|warning| format!(" ! {warning}")).unwrap_or_default();
            format!("{}/{}{level}{warning}", model.provider, model.id)
        }
        ModelReferenceResult::Ambiguous { candidates, usable_count, .. } => {
            let mut names: Vec<String> = candidates.iter().map(|m| format!("{}/{}", m.provider, m.id)).collect();
            names.sort();
            format!("ambiguous {} ({usable_count} usable)", names.join(", "))
        }
        ModelReferenceResult::NotFound { .. } => "not-found".into(),
    }
}

#[test]
fn model_references_match_upstream_cases() {
    let lenient = ThinkingSuffix::Lenient;
    let cases = [
        case("claude-sonnet-4-5", "anthropic/claude-sonnet-4-5"),
        case("GPT-4O", "openai/gpt-4o"),
        case("sonnet", "anthropic/claude-sonnet-4-5"),
        case("sonnet:high", "anthropic/claude-sonnet-4-5 :high"),
        case("qwen/qwen3-coder:exacto", "openrouter/qwen/qwen3-coder:exacto"),
        case("openrouter/qwen/qwen3-coder:exacto:high", "openrouter/qwen/qwen3-coder:exacto :high"),
        case("llama=http://host:10800/unsloth/qwen-gguf:BF16", "llama=http://host:10800/unsloth/qwen-gguf:BF16"),
        case(
            "llama=http://host:10800/unsloth/qwen-gguf:BF16:high",
            "llama=http://host:10800/unsloth/qwen-gguf:BF16 :high",
        ),
        case("openai/gpt-4o:extended", "openrouter/openai/gpt-4o:extended"),
        case("openrouter/qwen", "openrouter/qwen/qwen3-coder:exacto"),
        case("gpt-4o:random", "not-found"),
        Case {
            suffix: lenient,
            ..case(
                "gpt-4o:random",
                "openai/gpt-4o ! Invalid thinking level \"random\" in pattern \"gpt-4o:random\". Using default instead.",
            )
        },
        Case {
            suffix: lenient,
            ..case(
                "qwen/qwen3-coder:exacto:high:random",
                "openrouter/qwen/qwen3-coder:exacto ! Invalid thinking level \"random\" in pattern \"qwen/qwen3-coder:exacto:high:random\". Using default instead.",
            )
        },
        Case { suffix: ThinkingSuffix::None, ..case("gpt-4o:high", "not-found") },
        Case { fuzzy: false, ..case("sonnet", "not-found") },
        case("  ", "not-found"),
        Case { usable: &["gw-b"], ..case("shared", "gw-b/shared") },
        case("shared", "ambiguous gw-a/shared, gw-b/shared (0 usable)"),
        Case { usable: &["gw-a", "gw-b"], ..case("shared", "ambiguous gw-a/shared, gw-b/shared (2 usable)") },
        Case { usable: &["gw-b"], ..case("shared:high", "gw-b/shared :high") },
        Case { usable: &["gw-a", "gw-b"], ..case("shared:high", "ambiguous gw-a/shared, gw-b/shared (2 usable)") },
        Case { usable: &["gw-b"], ..case("anthropic/future", "gw-b/anthropic/future") },
        Case { usable: &["anthropic", "gw-a"], ..case("anthropic/exact", "anthropic/exact") },
        Case { usable: &["gw-a"], ..case("anthropic/exact", "gw-a/anthropic/exact") },
        case("anthropic/exact", "anthropic/exact"),
        Case { usable: &["anthropic"], ..case("anthropic/claude-opus", "anthropic/claude-opus-4-8") },
        Case { usable: &["gw-a"], ..case("anthropic/claude-opus", "gw-a/anthropic/claude-opus") },
        case("anthropic/claude-opus", "anthropic/claude-opus-4-8"),
        case("gw-b/free", "gw-b/gw-b/free"),
        Case { usable: &["gw-a"], ..case("opus", "gw-a/anthropic/claude-opus-4.8") },
        Case {
            usable: &["anthropic", "gw-a", "gw-b"],
            ..case("opus", "ambiguous anthropic/claude-opus-4-8, gw-b/claude-opus-4-8 (2 usable)")
        },
        Case { usable: &["anthropic"], ..case("opus", "anthropic/claude-opus-4-8") },
        Case { provider: Some("gw-a"), usable: &["gw-b"], ..case("shared", "gw-a/shared") },
        Case { provider: Some("gw-b"), ..case("GW-B/shared", "gw-b/shared") },
        Case { provider: Some("openai"), ..case("4o", "openai/gpt-4o") },
        Case { provider: Some("gw-a"), ..case("gpt-4o", "not-found") },
    ];
    let catalog = reference_catalog();
    let reversed: Vec<Model> = catalog.iter().rev().cloned().collect();
    for case in cases {
        let request = ModelReferenceRequest {
            text: case.text,
            provider: case.provider,
            thinking_suffix: case.suffix,
            fuzzy: case.fuzzy,
        };
        // Catalog order must never decide the result.
        for models in [&catalog, &reversed] {
            let result = resolve_model_reference(&request, models, &|provider| case.usable.contains(&provider));
            assert_eq!(describe(result), case.expected, "{}", case.text);
        }
    }
}

#[test]
fn fuzzy_matches_prefer_aliases_then_latest_dates() {
    let fuzzy = |models: &[Model]| {
        let request = ModelReferenceRequest {
            text: "3-5-sonnet",
            provider: None,
            thinking_suffix: ThinkingSuffix::Strict,
            fuzzy: true,
        };
        match resolve_model_reference(&request, models, &|_| true) {
            ModelReferenceResult::Resolved { model, .. } => model.id,
            other => panic!("{other:?}"),
        }
    };
    let models = vec![
        model("anthropic", "claude-3-5-sonnet-20240620", "Sonnet old", false),
        model("anthropic", "claude-3-5-sonnet-20241022", "Sonnet new", false),
    ];
    assert_eq!(fuzzy(&models), "claude-3-5-sonnet-20241022");
    let mut with_alias = models.clone();
    with_alias.push(model("anthropic", "claude-3-5-sonnet-latest", "Sonnet latest", false));
    assert_eq!(fuzzy(&with_alias), "claude-3-5-sonnet-latest");
}

#[test]
fn scope_patterns_add_every_provider_of_an_ambiguous_match_with_a_warning() {
    let mut models = all_models();
    models.push(model("azure", "gpt-4o", "GPT-4o", false));
    models.push(model("openai-codex", "gpt-4o-mini", "GPT-4o", false));
    for (pattern, level) in [("gpt-4o:high", Some(ThinkingLevel::High)), ("GPT-4o", None)] {
        let (scoped, diagnostics) = resolve_model_scope(&[pattern.to_string()], &models);
        let mut found: Vec<(String, Option<ThinkingLevel>)> =
            scoped.iter().map(|s| (format!("{}/{}", s.model.provider, s.model.id), s.thinking_level)).collect();
        found.sort();
        assert_eq!(found, [("azure/gpt-4o".to_string(), level), ("openai/gpt-4o".to_string(), level)]);
        assert_eq!(
            diagnostics,
            [ModelScopeDiagnostic {
                code: ModelScopeCode::Ambiguous,
                message: format!(
                    "Pattern \"{pattern}\" matches several providers: azure, openai. Use provider/id to choose one."
                ),
                pattern: pattern.to_string(),
            }]
        );
    }
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
    resolve_cli_model(provider, Some(pattern), thinking, false, catalog)
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

    let direct = model("anthropic", "claude-sonnet-4-5", "Sonnet", true);
    let openrouter = model("openrouter", "anthropic/claude-opus-5.5", "Opus", true);
    let vercel = model("vercel-ai-gateway", "anthropic/claude-opus-5.5", "Opus", true);
    let catalog = Catalog::with_auth(vec![direct, openrouter, vercel], &["vercel-ai-gateway"]);
    let with_level = cli(None, "anthropic/claude-opus-5.5:high", None, &catalog);
    assert_eq!(
        (pair(&with_level).0, with_level.thinking_level),
        ("vercel-ai-gateway".into(), Some(ThinkingLevel::High))
    );
    let inline = resolve_cli_model(None, Some("anthropic/claude-opus-5.5"), None, true, &catalog);
    assert!(inline.model.is_none());
    assert!(inline.error.unwrap().contains("--api-key does not choose a provider."));
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
    assert_eq!(default_model_for("github-copilot"), Some("gpt-6.1-sol"));
    // Every provider's default must be in the catalog.
    let chat = pi_ai::models::builtin_models().iter().filter(|model| pi_ai::models::is_chat_model(model));
    let mut absent = std::collections::BTreeSet::new();
    for provider in chat.map(|model| model.provider.as_str()) {
        let default = default_model_for(provider).unwrap_or_else(|| panic!("{provider} has no default"));
        if !pi_ai::models::builtin_models().iter().any(|m| m.provider == provider && m.id == default) {
            absent.insert(format!("{provider}/{default}"));
        }
    }
    assert!(absent.is_empty(), "defaults missing from the catalog: {absent:?}");

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
