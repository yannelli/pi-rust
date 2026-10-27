//! Catalog entries that upstream `generate-models.ts` rules produce, checked in the bundled
//! `models.json` (ported from upstream `supports-xhigh`, `cache-retention`, `max-thinking`,
//! and `classifier-models` tests).

use pi_ai::Model;
use pi_ai::ThinkingLevel::{Max, Xhigh};
use pi_ai::models::builtin_models;
use pi_ai::simple_options::supported_thinking_levels;
use serde_json::json;

fn find(provider: &str, id: &str, kind: &str) -> &'static Model {
    builtin_models()
        .iter()
        .find(|model| model.provider == provider && model.id == id && model.model_type.as_deref() == Some(kind))
        .unwrap_or_else(|| panic!("{kind} {provider}/{id} is not in the bundled catalog"))
}

/// The provider value a thinking level maps to: `Some(None)` for a level the model rejects.
fn level(model: &Model, name: &str) -> Option<Option<String>> {
    model.thinking_level_map.as_ref()?.get(name).cloned()
}

fn mapped(value: &str) -> Option<Option<String>> {
    Some(Some(value.to_string()))
}

#[test]
fn gpt_6_1_terra_supports_off_but_not_minimal_with_official_metadata() {
    for provider in ["openai", "azure", "openai-codex"] {
        let terra = find(provider, "gpt-6.1-terra", "chat");
        assert_eq!(level(terra, "off"), mapped("none"), "{provider}");
        let minimal = if provider == "openai-codex" { mapped("low") } else { Some(None) };
        assert_eq!(level(terra, "minimal"), minimal, "{provider}");
        assert!(supported_thinking_levels(terra).contains(&Xhigh), "{provider}");
    }
    for (provider, context_window) in [("openai", 272_000), ("openai-codex", 500_000)] {
        let terra = find(provider, "gpt-6.1-terra", "chat");
        let cost = &terra.cost;
        assert_eq!((cost.input, cost.output, cost.cache_read, cost.cache_write), (1.0, 5.0, 0.05, 1.25));
        assert_eq!(terra.context_window, context_window, "{provider}");
        assert_eq!(level(terra, "max"), mapped("max"), "{provider} sends max");
    }
    let openai = find("openai", "gpt-6.1-terra", "chat");
    assert!(supported_thinking_levels(openai).contains(&Max));
    assert_eq!(openai.prompt_cache, None, "no cache warming from the documented TTL alone");
}

#[test]
fn kimi_k3_1_pro_models_use_kimi_k3_settings() {
    for (provider, ids) in [
        ("kimi-coding", ["k3.1-pro", "k3.1-pro-highspeed"]),
        ("moonshotai", ["kimi-k3.1-pro", "kimi-k3.1-pro-highspeed"]),
    ] {
        for id in ids {
            let model = find(provider, id, "chat");
            let compat = model.compat.as_ref().unwrap();
            assert_eq!(
                (compat["thinkingFormat"].clone(), compat["supportsReasoningEffort"].clone()),
                (json!("openai"), json!(true))
            );
            assert_eq!((level(model, "low"), level(model, "max")), (mapped("low"), mapped("max")), "{id}");
            assert_eq!(model.cost.cache_write, 0.0, "{provider}/{id}");
        }
    }
    // Kimi Coding reports Moonshot rates for K3.1 Pro and K3 rates for k3-256k.
    let pro = &find("kimi-coding", "k3.1-pro", "chat").cost;
    assert_eq!((pro.input, pro.output, pro.cache_read), (2.0, 10.0, 0.2));
    let k3 = &find("kimi-coding", "k3-256k", "chat").cost;
    assert_eq!((k3.input, k3.output, k3.cache_read), (3.0, 15.0, 0.3));
}

#[test]
fn system_one_classifiers_set_their_image_form() {
    let openrouter = find("openrouter", "typesafe/jev-1.14", "classifier");
    assert_eq!(openrouter.api, "typesafe-system-one");
    // Explicit, so images keep OpenRouter's form when the baseUrl is overridden.
    assert_eq!(openrouter.compat, Some(json!({"imageInput": "state-image-urls", "sendImageOptInHeader": false})));
    let typesafe = find("typesafe", "jev-latest", "classifier");
    assert_eq!(typesafe.input, ["text", "image"]);
    assert_eq!(typesafe.compat, Some(json!({"imageInput": "assets", "sendImageOptInHeader": true})));
    let workers = [
        ("@cf/cloudflare/clef", 0.24, 65_536, "images"),
        ("@cf/cloudflare/clef-flash", 0.038, 24_576, "images"),
        ("@cf/cloudflare/clef-omni", 0.15, 64_000, "images"),
        ("typesafe/jev", 0.042, 32_000, "assets"),
        ("typesafe/jev-1.14.0", 0.042, 32_000, "assets"),
    ];
    for (id, price, context_window, image_input) in workers {
        let model = find("cloudflare-workers-ai", id, "classifier");
        assert_eq!((model.cost.input, model.context_window), (price, context_window), "{id}");
        assert_eq!(model.input, ["text", "image"], "{id}");
        assert_eq!(model.compat, Some(json!({"imageInput": image_input})), "{id}");
    }
}

#[test]
fn opencode_deepseek_v4_1_flash_does_not_support_long_cache_retention() {
    let model = find("opencode", "deepseek-v4.1-flash", "chat");
    assert_eq!(model.compat.as_ref().unwrap()["supportsLongCacheRetention"], json!(false));
}
