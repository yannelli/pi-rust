//! Upstream `Provider` objects from `pi.registerProvider(provider)`: registration, API key and
//! OAuth auth with stored credentials, streaming through the object, `refreshModels` with the
//! models store, `filterModels`, re-registration, and unregistration.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use pi_ai::auth::{
    ApiKeyCredential, AuthEvent, AuthInteraction, AuthPrompt, AuthResult, AuthType, Credential, CredentialStore,
    InMemoryCredentialStore, LoginOptions,
};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::extensions::harness::{Harness, Setup, harness};

/// Model and stream helpers the extensions of these tests share.
const COMMON: &str = r#"import { createAssistantMessageEventStream } from "@earendil-works/pi-ai";

const usage = { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, totalTokens: 0,
	cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 } };
const chat = (id, extra) => ({ id, name: id, api: "native-api", provider: "native", baseUrl: "http://127.0.0.1:9/v1",
	reasoning: false, input: ["text"], cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 }, contextWindow: 4096,
	maxTokens: 256, ...extra });
const noStream = () => {
	throw new Error("stream is not used");
};

function reply(model, text) {
	const stream = createAssistantMessageEventStream();
	const message = { role: "assistant", content: [{ type: "text", text }], api: model.api, provider: model.provider,
		model: model.id, usage, stopReason: "stop", timestamp: Date.now() };
	queueMicrotask(() => {
		stream.push({ type: "start", partial: message });
		stream.push({ type: "done", reason: "stop", message });
		stream.end(message);
	});
	return stream;
}
"#;

async fn native_harness(body: &str) -> Harness {
    let source = format!("{COMMON}\n{body}");
    let h = harness(Setup { files: vec![("native.ts", source)], load: vec!["native.ts"], ..Setup::default() }).await;
    assert_eq!(h.errors(), Vec::<String>::new());
    h.session.model_runtime().wait_for_offline_refreshes().await;
    h
}

/// Answers prompts in order and records prompts and notifications.
#[derive(Default)]
struct Interaction {
    answers: Mutex<Vec<String>>,
    events: Mutex<Vec<String>>,
}

impl Interaction {
    fn answering(answer: &str) -> Arc<Self> {
        Arc::new(Self { answers: Mutex::new(vec![answer.into()]), ..Self::default() })
    }

    fn events(&self) -> Vec<String> {
        self.events.lock().unwrap().clone()
    }
}

#[async_trait]
impl AuthInteraction for Interaction {
    async fn prompt(&self, prompt: AuthPrompt, _cancel: CancellationToken) -> AuthResult<String> {
        self.events.lock().unwrap().push(format!("prompt: {prompt:?}"));
        Ok(self.answers.lock().unwrap().remove(0))
    }

    fn notify(&self, event: AuthEvent) {
        self.events.lock().unwrap().push(format!("notify: {event:?}"));
    }
}

async fn store(credentials: &InMemoryCredentialStore, provider: &str, credential: Credential) {
    credentials.update(provider, Box::new(move |_| Box::pin(async move { Ok(Some(credential)) }))).await.unwrap();
}

fn api_key(key: &str) -> Credential {
    Credential::ApiKey(ApiKeyCredential { key: Some(key.into()), env: None })
}

async fn stored_json(credentials: &InMemoryCredentialStore, provider: &str) -> Value {
    json!(credentials.read(provider).await.unwrap())
}

fn entry_data(h: &Harness, custom_type: &str) -> Value {
    let entries = h.entries();
    let entry = entries.iter().rev().find(|entry| entry["customType"] == custom_type);
    entry.map(|entry| entry["data"].clone()).unwrap_or(Value::Null)
}

async fn ask(h: &Harness, provider: &str, model: &str) -> Option<String> {
    let model = h.session.model_runtime().get_model(provider, model).expect("registered model");
    h.session.set_model(model, false).await.unwrap();
    h.session.prompt("hi", Vec::new()).await.unwrap();
    h.session.last_assistant_text()
}

const API_KEY_PROVIDER: &str = r#"export default function (pi) {
	const provider = {
		id: "native", name: "Native", baseUrl: "http://127.0.0.1:9/v1",
		auth: { apiKey: {
			name: "Native API key",
			async login(interaction) {
				interaction.signal.throwIfAborted();
				interaction.notify({ type: "info", message: "Get a key", links: [{ url: "https://example.test/keys" }] });
				const key = await interaction.prompt({ type: "secret", message: "Enter Native key", placeholder: "nk-" });
				return { type: "api_key", key: key.trim() };
			},
			async resolve({ ctx, credential, signal }) {
				if (!(signal instanceof AbortSignal) || typeof ctx.env !== "function") throw new Error("bad resolve input");
				return credential?.key ? { auth: { apiKey: credential.key, headers: { "x-native": "yes" } }, source: "stored key" } : undefined;
			},
		} },
		getModels: () => [chat("alpha", { marker: "listed" })],
		stream: noStream,
		streamSimple: (model, context, options) => reply(model,
			`key=${options.apiKey} header=${options.headers?.["x-native"]} marker=${model.marker} ` +
				`last=${context.messages.at(-1)?.role} hooked=${options.headers === hooked}`),
	};
	let hooked;
	pi.on("before_provider_headers", (event) => {
		event.headers["x-hook"] = "1";
		hooked = event.headers;
	});
	pi.registerProvider(provider);
	pi.registerCommand("registry", { handler: async (_args, ctx) => {
		const registry = ctx.modelRegistry;
		pi.appendEntry("registry", {
			same: registry.getRegisteredNativeProvider("native") === provider,
			effective: registry.getProvider("native") === provider,
			config: registry.getRegisteredProviderConfig("native") ?? null,
			ids: registry.getRegisteredProviderIds(),
			key: (await registry.getApiKeyForProvider("native")) ?? null,
			model: registry.find("native", "alpha")?.id ?? null,
		});
	} });
}
"#;

#[tokio::test(flavor = "multi_thread")]
async fn api_key_login_resolves_through_the_object_and_streams_with_its_stream_function() {
    let h = native_harness(API_KEY_PROVIDER).await;
    let runtime = h.session.model_runtime().clone();
    assert_eq!(runtime.get_model("native", "alpha").map(|model| model.name), Some("alpha".into()));
    assert_eq!(runtime.registry().provider_name("native"), Some("Native"));
    runtime.refresh_configured_providers().await;
    assert!(!runtime.has_configured_auth("native"));
    let options = runtime.find_login_providers("native").await;
    assert_eq!(
        options.iter().map(|option| (option.auth_type, option.ambient)).collect::<Vec<_>>(),
        [(AuthType::ApiKey, false)]
    );

    let interaction = Interaction::answering("  sk-native  ");
    let options = LoginOptions::default();
    let login = runtime.login("native", AuthType::ApiKey, interaction.as_ref(), &options, CancellationToken::new());
    let credential = login.await.unwrap();
    assert_eq!(json!(credential), json!({"type": "api_key", "key": "sk-native"}));
    assert_eq!(stored_json(&h.credentials, "native").await, json!({"type": "api_key", "key": "sk-native"}));
    let events = interaction.events();
    assert!(events[0].starts_with("notify: Info { message: \"Get a key\""), "{events:?}");
    assert!(events[1].contains("Secret { message: \"Enter Native key\", placeholder: Some(\"nk-\") }"), "{events:?}");
    assert!(runtime.has_configured_auth("native"));
    assert!(runtime.available_models().iter().any(|model| model.provider == "native" && model.id == "alpha"));

    let text = ask(&h, "native", "alpha").await;
    assert_eq!(text.as_deref(), Some("key=sk-native header=yes marker=listed last=user hooked=true"));
    h.session.prompt("/registry", Vec::new()).await.unwrap();
    let data = entry_data(&h, "registry");
    assert_eq!(
        data,
        json!({"same": true, "effective": true, "config": null, "ids": ["native"], "key": "sk-native", "model": "alpha"})
    );
    assert_eq!(h.errors(), Vec::<String>::new());
}

const OAUTH_PROVIDER: &str = r#"export default function (pi) {
	pi.registerProvider({
		id: "native-oauth", name: "Native OAuth", baseUrl: "http://127.0.0.1:9/v1",
		auth: { oauth: {
			name: "Native account", isSubscription: true, loginLabel: "Sign in with Native",
			async login(interaction, options) {
				interaction.notify({ type: "auth_url", url: "https://example.test/authorize" });
				const code = await interaction.prompt({ type: "manual_code", message: "Paste the code" });
				return { type: "oauth", refresh: `r-${code}`, access: `a-${code}`, expires: Date.now() - 1000,
					account: "acct-1", agent: options?.agentName ?? null };
			},
			async refresh(credential, signal) {
				if (!(signal instanceof AbortSignal)) throw new Error("no refresh signal");
				return { ...credential, refresh: `${credential.refresh}+`, access: "a-fresh", expires: Date.now() + 3600000 };
			},
			async toAuth(credential) {
				return { apiKey: `tok:${credential.access}`, baseUrl: "http://127.0.0.1:9/v2" };
			},
		} },
		getModels: () => [chat("omega", { provider: "native-oauth" })],
		stream: noStream,
		streamSimple: (model, _context, options) => reply(model, `${options.apiKey} ${model.baseUrl}`),
	});
}
"#;

#[tokio::test(flavor = "multi_thread")]
async fn oauth_login_refresh_and_to_auth_run_in_the_object_and_persist_the_credential() {
    let h = native_harness(OAUTH_PROVIDER).await;
    let runtime = h.session.model_runtime().clone();
    let oauth = runtime.login_providers(Some(AuthType::OAuth)).await;
    let option = oauth.iter().find(|option| option.id == "native-oauth").expect("an OAuth login option");
    assert_eq!((option.name.as_str(), option.login_label.as_deref()), ("Native OAuth", Some("Sign in with Native")));
    assert!(option.subscription);
    let api_keys = runtime.login_providers(Some(AuthType::ApiKey)).await;
    assert!(!api_keys.iter().any(|option| option.id == "native-oauth"), "{api_keys:?}");

    let interaction = Interaction::answering("c1");
    let options = LoginOptions { agent_name: Some("tester".into()), ..LoginOptions::default() };
    let credential =
        runtime.login("native-oauth", AuthType::OAuth, interaction.as_ref(), &options, CancellationToken::new()).await;
    let stored = stored_json(&h.credentials, "native-oauth").await;
    assert_eq!(json!(credential.unwrap()), stored);
    assert_eq!(stored["type"], "oauth");
    assert_eq!((&stored["access"], &stored["refresh"]), (&json!("a-c1"), &json!("r-c1")));
    assert_eq!((&stored["account"], &stored["agent"]), (&json!("acct-1"), &json!("tester")));
    let events = interaction.events();
    assert!(events[0].contains("AuthUrl { url: \"https://example.test/authorize\""), "{events:?}");
    assert!(events[1].contains("ManualCode { message: \"Paste the code\""), "{events:?}");
    assert!(runtime.has_configured_auth("native-oauth"));
    assert!(runtime.is_using_oauth("native-oauth").await);

    // The stored token has expired: the request refreshes it under the store lock first.
    let text = ask(&h, "native-oauth", "omega").await;
    assert_eq!(text.as_deref(), Some("tok:a-fresh http://127.0.0.1:9/v2"));
    let stored = stored_json(&h.credentials, "native-oauth").await;
    assert_eq!((&stored["access"], &stored["refresh"]), (&json!("a-fresh"), &json!("r-c1+")));
    assert_eq!((&stored["type"], &stored["account"]), (&json!("oauth"), &json!("acct-1")));
    assert_eq!(h.errors(), Vec::<String>::new());
}

const REFRESH_PROVIDER: &str = r#"export default function (pi) {
	const contexts = [];
	let late;
	const register = (label) => {
		let models = [chat("alpha")];
		pi.registerProvider({
			id: "native", name: "Native", baseUrl: "http://127.0.0.1:9/v1",
			auth: { apiKey: { name: "Native key", resolve: async ({ credential }) => (credential?.key ? { auth: { apiKey: credential.key } } : undefined) } },
			getModels: () => models.filter((model) => (model.type ?? "chat") === "chat"),
			getAllModels: () => models,
			async refreshModels(context) {
				contexts.push({ label, allowNetwork: context.allowNetwork, credential: context.credential ?? null,
					stored: context.stored?.models.map((model) => model.id) ?? null, force: context.force ?? null,
					baseUrl: context.baseUrl ?? null, signal: context.signal instanceof AbortSignal });
				if (context.stored) models = context.stored.models;
				if (!context.allowNetwork) return;
				const next = [chat("beta", { marker: "fetched" }), chat("pic", { type: "image", api: "native-images" }),
					chat("clip", { type: "video" })];
				late = context.publish;
				const published = await context.publish({ persist: { models: next, checkedAt: 42 }, update: () => { models = next; } });
				contexts.push({ label, published });
			},
			stream: noStream,
			streamSimple: (model) => reply(model, model.id),
		});
	};
	register("first");
	pi.registerCommand("reregister", { handler: async () => register("second") });
	pi.registerCommand("contexts", { handler: async () => {
		const stale = await late({ update: () => { throw new Error("a stale update ran"); } });
		pi.appendEntry("contexts", { contexts, stale });
	} });
}
"#;

#[tokio::test(flavor = "multi_thread")]
async fn refresh_models_restores_publishes_and_persists_the_catalog() {
    let h = native_harness(REFRESH_PROVIDER).await;
    let runtime = h.session.model_runtime().clone();
    store(&h.credentials, "native", api_key("sk")).await;
    let result = runtime.refresh_catalogs(true, Some(vec!["native".into()]), true).await;
    assert!(result.errors.is_empty(), "{:?}", result.errors);
    assert!(runtime.get_model("native", "alpha").is_none());
    assert!(runtime.get_model("native", "beta").is_some());
    let kinds: Vec<(String, Option<String>)> =
        runtime.registry().provider_models("native").into_iter().map(|model| (model.id, model.model_type)).collect();
    assert!(kinds.contains(&("pic".into(), Some("image".into()))), "{kinds:?}");

    // The entry is stored as the provider published it, in `models-store.json`.
    let text = std::fs::read_to_string(h.agent_dir.join("models-store.json")).unwrap();
    let document: Value = serde_json::from_str(&text).unwrap();
    let entry = &document["native"];
    assert_eq!(entry["checkedAt"], 42);
    assert_eq!(entry["models"][0]["marker"], "fetched");
    let ids: Vec<&str> = entry["models"].as_array().unwrap().iter().filter_map(|model| model["id"].as_str()).collect();
    assert_eq!(ids, ["beta", "pic", "clip"]);

    // A new object restores the stored catalog offline, without models of unknown types.
    h.session.prompt("/reregister", Vec::new()).await.unwrap();
    runtime.wait_for_offline_refreshes().await;
    assert!(runtime.get_model("native", "beta").is_some());
    h.session.prompt("/contexts", Vec::new()).await.unwrap();
    let data = entry_data(&h, "contexts");
    let contexts = data["contexts"].as_array().unwrap();
    let network: Vec<&Value> = contexts.iter().filter(|context| context["allowNetwork"] == true).collect();
    assert_eq!(
        network,
        [&json!({"label": "first", "allowNetwork": true, "credential": {"type": "api_key", "key": "sk"},
        "stored": null, "force": true, "baseUrl": "http://127.0.0.1:9/v1", "signal": true})]
    );
    let published: Vec<&Value> = contexts.iter().filter(|context| context.get("published").is_some()).collect();
    assert_eq!(published, [&json!({"label": "first", "published": true})]);
    let second: Vec<&Value> = contexts.iter().filter(|context| context["label"] == "second").collect();
    assert!(!second.is_empty());
    for context in second {
        assert_eq!(context["allowNetwork"], false, "{context}");
        assert_eq!(context["stored"], json!(["beta", "pic"]), "{context}");
        assert_eq!(context["credential"], json!({"type": "api_key", "key": "sk"}), "{context}");
        assert_eq!(context["force"], Value::Null, "{context}");
    }
    assert_eq!(data["stale"], false, "a publication after its refresh ended is dropped");
    assert_eq!(h.errors(), Vec::<String>::new());
}

const FILTER_PROVIDER: &str = r#"export default function (pi) {
	pi.registerProvider({
		id: "native", name: "Native", baseUrl: "http://127.0.0.1:9/v1",
		auth: { apiKey: { name: "Native key", resolve: async ({ credential }) => (credential?.key ? { auth: { apiKey: credential.key } } : undefined) } },
		getModels: () => [chat("alpha"), chat("reserve")],
		filterModels: (models, credential) => models.filter((model) => model.id !== "reserve" || credential?.key === "all"),
		stream: noStream,
		streamSimple: (model) => reply(model, model.id),
	});
	pi.registerCommand("available", { handler: async (_args, ctx) => {
		const available = ctx.modelRegistry.getAvailable().filter((model) => model.provider === "native");
		pi.appendEntry("available", available.map((model) => model.id));
	} });
}
"#;

#[tokio::test(flavor = "multi_thread")]
async fn filter_models_decides_which_models_of_a_configured_provider_are_available() {
    let h = native_harness(FILTER_PROVIDER).await;
    let runtime = h.session.model_runtime().clone();
    let native = |models: Vec<pi_ai::Model>| -> Vec<String> {
        models.into_iter().filter(|model| model.provider == "native").map(|model| model.id).collect()
    };
    store(&h.credentials, "native", api_key("sk")).await;
    runtime.refresh_configured_providers().await;
    assert_eq!(native(runtime.all_models()), ["alpha", "reserve"]);
    assert_eq!(native(runtime.available_models()), ["alpha"]);
    h.session.prompt("/available", Vec::new()).await.unwrap();
    assert_eq!(entry_data(&h, "available"), json!(["alpha"]));

    store(&h.credentials, "native", api_key("all")).await;
    runtime.refresh_configured_providers().await;
    assert_eq!(native(runtime.available_models()), ["alpha", "reserve"]);
    assert_eq!(h.errors(), Vec::<String>::new());
}

/// Run `command`, let the offline refresh it starts finish, then record the registry state.
async fn run_then_state(h: &Harness, command: &str) {
    h.session.prompt(command, Vec::new()).await.unwrap();
    h.session.model_runtime().wait_for_offline_refreshes().await;
    h.session.prompt("/state", Vec::new()).await.unwrap();
}

const SWAPPED_PROVIDER: &str = r#"export default function (pi) {
	const make = (tag, ids) => ({
		id: "native", name: `Native ${tag}`, baseUrl: "http://127.0.0.1:9/v1",
		auth: { apiKey: { name: "Native key", resolve: async () => ({ auth: {}, source: "keyless" }) } },
		getModels: () => ids.map((id) => chat(id)),
		stream: noStream,
		streamSimple: (model, _context, options) => reply(model, `${tag}:${model.id}:${options.apiKey ?? "none"}`),
	});
	let current = make("one", ["alpha"]);
	pi.registerProvider(current);
	pi.registerCommand("swap", { handler: async () => {
		current = make("two", ["gamma"]);
		pi.registerProvider(current);
	} });
	pi.registerCommand("config", { handler: async () => pi.registerProvider("native", { baseUrl: "http://127.0.0.1:9/v1",
		api: "openai-completions", apiKey: "configured", models: [{ id: "delta", name: "Delta", reasoning: false,
			input: ["text"], cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 }, contextWindow: 1000, maxTokens: 100 }] }) });
	pi.registerCommand("drop", { handler: async () => pi.unregisterProvider("native") });
	pi.registerCommand("state", { handler: async (_args, ctx) => {
		const native = ctx.modelRegistry.getRegisteredNativeProvider("native");
		pi.appendEntry("state", { current: native === current, native: native !== undefined,
			config: ctx.modelRegistry.getRegisteredProviderConfig("native") !== undefined,
			ids: ctx.modelRegistry.getRegisteredProviderIds() });
	} });
}
"#;

#[tokio::test(flavor = "multi_thread")]
async fn a_new_object_replaces_the_provider_and_unregistering_removes_it() {
    let h = native_harness(SWAPPED_PROVIDER).await;
    let runtime = h.session.model_runtime().clone();
    let state = |h: &Harness| entry_data(h, "state");
    assert!(runtime.has_configured_auth("native"), "a keyless resolve configures the provider");
    assert_eq!(ask(&h, "native", "alpha").await.as_deref(), Some("one:alpha:none"));

    run_then_state(&h, "/swap").await;
    assert!(runtime.get_model("native", "alpha").is_none());
    assert_eq!(runtime.registry().provider_name("native"), Some("Native two"));
    assert_eq!(ask(&h, "native", "gamma").await.as_deref(), Some("two:gamma:none"));
    assert_eq!(state(&h), json!({"current": true, "native": true, "config": false, "ids": ["native"]}));

    run_then_state(&h, "/config").await;
    assert!(runtime.get_model("native", "gamma").is_none());
    assert!(runtime.get_model("native", "delta").is_some());
    assert!(!runtime.is_native_provider("native"));
    assert_eq!(state(&h), json!({"current": false, "native": false, "config": true, "ids": ["native"]}));

    run_then_state(&h, "/swap").await;
    assert!(runtime.get_model("native", "delta").is_none());
    assert_eq!(state(&h), json!({"current": true, "native": true, "config": false, "ids": ["native"]}));

    run_then_state(&h, "/drop").await;
    assert!(runtime.registry().provider_models("native").is_empty());
    assert!(runtime.find_login_providers("native").await.is_empty());
    assert_eq!(state(&h), json!({"current": false, "native": false, "config": false, "ids": []}));
    assert_eq!(h.errors(), Vec::<String>::new());
}

const FAILING_PROVIDER: &str = r#"export default function (pi) {
	pi.registerProvider({
		id: "native", name: "Native", baseUrl: "http://127.0.0.1:9/v1",
		auth: { apiKey: { name: "Native key", resolve: async () => ({ auth: { apiKey: "k" } }) } },
		getModels: () => [chat("throws"), chat("silent")],
		stream: noStream,
		streamSimple(model) {
			if (model.id === "throws") throw new Error("no connection");
			const stream = createAssistantMessageEventStream();
			queueMicrotask(() => stream.end());
			return stream;
		},
	});
}
"#;

#[tokio::test(flavor = "multi_thread")]
async fn a_failing_or_unfinished_stream_ends_the_request_with_an_error() {
    let h = native_harness(FAILING_PROVIDER).await;
    let last_error = |h: &Harness| {
        h.session.messages().iter().rev().find_map(|message| match message {
            pi_ai::Message::Assistant(assistant) => Some((assistant.stop_reason, assistant.error_message.clone())),
            _ => None,
        })
    };
    ask(&h, "native", "throws").await;
    assert_eq!(last_error(&h), Some((pi_ai::StopReason::Error, Some("no connection".into()))));
    ask(&h, "native", "silent").await;
    let unfinished = Some("The provider stream ended without a done or error event".to_string());
    assert_eq!(last_error(&h), Some((pi_ai::StopReason::Error, unfinished)));
}
