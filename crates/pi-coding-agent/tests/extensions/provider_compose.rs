//! Provider composition for extensions: `refreshModels`, `oauth.modifyModels`, and the
//! provider's `images` and `classifiers` (upstream `provider-composer.ts`).

use pi_ai::auth::{Credential, OAuthCredential};
use pi_ai::classify::{ClassifierContext, ClassifierOptions, ClassifierStopReason};
use pi_ai::images::{ImagesContext, ImagesOptions, ImagesStopReason};
use serde_json::{Map, json};

use crate::harness::{Setup, harness};

const SOURCE: &str = r#"const cost = { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 };
export default function (pi) {
	pi.registerProvider("compose-demo", {
		baseUrl: "http://compose.invalid",
		api: "openai-completions",
		apiKey: "demo-key",
		models: [
			{ id: "first", name: "First", reasoning: false, input: ["text"], cost, contextWindow: 1000, maxTokens: 100 },
			{ id: "painter", name: "Painter", type: "image", api: "compose-images", input: ["text"], output: ["image"], cost },
			{ id: "judge", name: "Judge", type: "classifier", api: "compose-judge", input: ["text"], cost, contextWindow: 10 },
		],
		async refreshModels(context) {
			globalThis.refreshSeen = `${context.allowNetwork}/${context.credential?.type}`;
			return [
				{ id: "refreshed", name: "Refreshed", reasoning: false, input: ["text"], cost, contextWindow: 2000, maxTokens: 200 },
				{ id: "painter", name: "Painter", type: "image", api: "compose-images", input: ["text"], output: ["image"], cost },
				{ id: "other-painter", name: "Other", type: "image", api: "missing-images", input: ["text"], output: ["image"], cost },
				{ id: "judge", name: "Judge", type: "classifier", api: "compose-judge", input: ["text"], cost, contextWindow: 10 },
			];
		},
		oauth: {
			name: "Compose",
			async login() { return { refresh: "r", access: "a", expires: 0 }; },
			async refreshToken(credentials) { return credentials; },
			getApiKey(credentials) { return credentials.access; },
			modifyModels(models, credentials) {
				return models.map((model) => ({ ...model, name: `${model.name} for ${credentials.access} (${globalThis.refreshSeen})` }));
			},
		},
		images: {
			"compose-images": {
				async generateImages(model, context, options) {
					return { api: model.api, provider: model.provider, model: model.id, stopReason: "stop", timestamp: 1,
						output: [{ type: "text", text: `painted ${context.input[0].text} with ${options.apiKey}` }] };
				},
			},
		},
		classifiers: {
			"compose-judge": {
				async classify(model) {
					return { api: model.api, provider: model.provider, model: model.id, stopReason: "stop", timestamp: 1,
						answers: { ok: { type: "bool", probability: 0.75 } } };
				},
			},
		},
	});
}
"#;

#[tokio::test(flavor = "multi_thread")]
async fn extension_providers_refresh_modify_and_run_their_operations() {
    let h = harness(Setup { files: vec![("compose.ts", SOURCE.into())], load: vec!["compose.ts"], ..Setup::default() })
        .await;
    assert_eq!(h.errors(), Vec::<String>::new());
    let runtime = h.session.model_runtime().clone();
    // Registration ran `refreshModels` offline; without a credential nothing modifies its models.
    assert!(runtime.get_model("compose-demo", "first").is_none());
    assert_eq!(runtime.get_model("compose-demo", "refreshed").unwrap().name, "Refreshed");

    let oauth = OAuthCredential { refresh: "r".into(), access: "token".into(), expires: 0, extra: Map::new() };
    let credential = Credential::OAuth(oauth);
    runtime
        .credential_store()
        .update("compose-demo", Box::new(move |_| Box::pin(async move { Ok(Some(credential)) })))
        .await
        .unwrap();
    let result = runtime.refresh_catalogs(true, Some(vec!["compose-demo".into()]), false).await;
    assert!(result.errors.is_empty(), "{:?}", result.errors);
    assert!(runtime.get_model("compose-demo", "first").is_none());
    let refreshed = runtime.get_model("compose-demo", "refreshed").expect("refreshed model");
    assert_eq!(refreshed.name, "Refreshed for token (true/oauth)");

    let all = runtime.registry().all_models().to_vec();
    let find = |id: &str| all.iter().find(|model| model.provider == "compose-demo" && model.id == id).unwrap().clone();
    let context = ImagesContext { input: vec![pi_ai::ContentBlock::text("a cat")] };
    let mut options = ImagesOptions::default();
    options.request.api_key = Some("image-key".into());
    let images = runtime.generate_images(&find("painter"), &context, options).await;
    assert_eq!(images.stop_reason, ImagesStopReason::Stop, "{images:?}");
    assert_eq!(serde_json::to_value(&images.output).unwrap()[0]["text"], json!("painted a cat with image-key"));
    let missing = runtime.generate_images(&find("other-painter"), &context, ImagesOptions::default()).await;
    assert_eq!(missing.stop_reason, ImagesStopReason::Error);
    let expected = "Provider compose-demo has no image implementation for \"missing-images\"";
    assert_eq!(missing.error_message.as_deref(), Some(expected));

    let context: ClassifierContext =
        serde_json::from_value(json!({"state": {}, "questions": {"ok": {"type": "bool", "instructions": "ok?",
            "criteria": {"true": "yes", "false": "no"}}}}))
        .unwrap();
    let judged = runtime.classify(&find("judge"), &context, ClassifierOptions::default()).await;
    assert_eq!(judged.stop_reason, ClassifierStopReason::Stop, "{judged:?}");
    let mut wrong = find("judge");
    wrong.api = "missing-judge".into();
    let failed = runtime.classify(&wrong, &context, ClassifierOptions::default()).await;
    let expected = "Provider compose-demo has no classifier implementation for \"missing-judge\"";
    assert_eq!(failed.error_message.as_deref(), Some(expected));
}

const OFFLINE: &str = r#"const cost = { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 };
let refreshes = 0;
export default function (pi) {
	pi.registerProvider("offline-demo", {
		baseUrl: "http://offline.invalid",
		api: "openai-completions",
		apiKey: "demo-key",
		async refreshModels(context) {
			if (context.allowNetwork) throw new Error("a test refresh asked for the network");
			refreshes += 1;
			return [{ id: "cached", name: `Cached ${refreshes}`, reasoning: false, input: ["text"], cost, contextWindow: 1000, maxTokens: 100 }];
		},
	});
	pi.registerCommand("add-virtual", { handler: async () => pi.registerVirtualModel({
		provider: "router", id: "auto", name: "Auto", route: () => ({ model: { provider: "offline-demo", id: "cached" } }),
	}) });
	pi.registerCommand("drop-provider", { handler: async () => pi.unregisterProvider("offline-demo") });
}
"#;

#[tokio::test(flavor = "multi_thread")]
async fn models_from_refresh_models_are_listed_offline_as_providers_and_virtual_models_change() {
    let h =
        harness(Setup { files: vec![("offline.ts", OFFLINE.into())], load: vec!["offline.ts"], ..Setup::default() })
            .await;
    assert_eq!(h.errors(), Vec::<String>::new());
    let runtime = h.session.model_runtime().clone();
    let cached = runtime.get_model("offline-demo", "cached").expect("listed by the offline refresh");
    assert_eq!(cached.base_url, "http://offline.invalid");
    assert!(runtime.has_configured_auth("offline-demo"));

    h.session.prompt("/add-virtual", Vec::new()).await.unwrap();
    runtime.wait_for_offline_refreshes().await;
    assert!(runtime.get_model("router", "auto").is_some());
    let renamed = runtime.get_model("offline-demo", "cached").unwrap().name;
    assert_ne!(renamed, cached.name, "registering a virtual model refreshes offline");

    h.session.prompt("/drop-provider", Vec::new()).await.unwrap();
    assert!(runtime.get_model("offline-demo", "cached").is_none());
    runtime.wait_for_offline_refreshes().await;
    assert!(runtime.get_model("offline-demo", "cached").is_none(), "the refresh does not bring them back");
    assert!(runtime.get_model("router", "auto").is_some());
    assert_eq!(h.errors(), Vec::<String>::new());
}
