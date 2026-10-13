//! Providers from extensions: JavaScript streams, OAuth, and model registration.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use pi_ai::auth::{AuthEvent, AuthInteraction, AuthPrompt, AuthResult, AuthType, Credential, LoginOptions};
use tokio_util::sync::CancellationToken;

use crate::harness::{Setup, example, harness};

/// A stand-in for `@anthropic-ai/sdk` that streams one fixed text answer.
const FAKE_ANTHROPIC_SDK: &str = r#"
export default class Anthropic {
	constructor(options) {
		globalThis.anthropicOptions = options;
	}
	get messages() {
		return {
			stream(params) {
				globalThis.anthropicParams = params;
				const events = [
					{ type: "message_start", message: { usage: { input_tokens: 10, output_tokens: 0 } } },
					{ type: "content_block_start", index: 0, content_block: { type: "text", text: "" } },
					{ type: "content_block_delta", index: 0, delta: { type: "text_delta", text: "Hello from " } },
					{ type: "content_block_delta", index: 0, delta: { type: "text_delta", text: "fake Claude" } },
					{ type: "content_block_stop", index: 0 },
					{ type: "message_delta", delta: { stop_reason: "end_turn" }, usage: { input_tokens: 10, output_tokens: 5 } },
				];
				return {
					async *[Symbol.asyncIterator]() {
						for (const event of events) yield event;
					},
				};
			},
		};
	}
}
"#;

const SDK_PACKAGE: &str = r#"{"name": "@anthropic-ai/sdk", "type": "module", "main": "index.js"}"#;

#[tokio::test(flavor = "multi_thread")]
async fn custom_anthropic_provider_streams_through_its_sdk() {
    let h = harness(Setup {
        files: vec![
            ("custom-provider-anthropic/index.ts", example("custom-provider-anthropic/index.ts")),
            ("custom-provider-anthropic/package.json", example("custom-provider-anthropic/package.json")),
            ("custom-provider-anthropic/node_modules/@anthropic-ai/sdk/package.json", SDK_PACKAGE.into()),
            ("custom-provider-anthropic/node_modules/@anthropic-ai/sdk/index.js", FAKE_ANTHROPIC_SDK.into()),
        ],
        load: vec!["custom-provider-anthropic/index.ts"],
        ..Setup::default()
    })
    .await;
    assert_eq!(h.errors(), Vec::<String>::new());
    let runtime = h.session.model_runtime().clone();
    let model = runtime.get_model("custom-anthropic", "claude-sonnet-4-5").expect("registered model");
    assert_eq!(model.api, "custom-anthropic-api");
    runtime.set_runtime_api_key("custom-anthropic", "sk-test");
    h.session.set_model(model, false).await.unwrap();
    h.session.prompt("hi", Vec::new()).await.unwrap();
    assert_eq!(h.session.last_assistant_text().as_deref(), Some("Hello from fake Claude"));
    let usage = h.session.messages().iter().rev().find_map(|message| match message {
        pi_ai::Message::Assistant(assistant) => Some(assistant.usage.clone()),
        _ => None,
    });
    assert_eq!(usage.map(|usage| (usage.input, usage.output)), Some((10, 5)));
}

/// Answers prompts in order and records notifications.
#[derive(Default)]
struct Interaction {
    answers: Mutex<Vec<String>>,
    events: Mutex<Vec<String>>,
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

const OAUTH_PROVIDER: &str = r#"
import { createAssistantMessageEventStream } from "@earendil-works/pi-ai";

export default function (pi) {
	pi.registerProvider("echo-oauth", {
		baseUrl: "http://localhost:1",
		api: "echo-oauth-api",
		models: [{ id: "echo", name: "Echo", reasoning: false, input: ["text"], cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 }, contextWindow: 1000, maxTokens: 100 }],
		oauth: {
			name: "Echo OAuth",
			async login(callbacks) {
				callbacks.onAuth({ url: "https://example.test/auth" });
				const code = await callbacks.onPrompt({ message: "Code?" });
				return { refresh: "r-" + code, access: "a-" + code, expires: Date.now() + 3600000 };
			},
			async refreshToken(credentials) {
				return credentials;
			},
			getApiKey: (credentials) => "key:" + credentials.access,
		},
		streamSimple(model, context, options) {
			const stream = createAssistantMessageEventStream();
			const message = {
				role: "assistant", content: [{ type: "text", text: "auth=" + options.apiKey }], api: model.api,
				provider: model.provider, model: model.id, stopReason: "stop", timestamp: Date.now(),
				usage: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, totalTokens: 0, cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 } },
			};
			queueMicrotask(() => {
				stream.push({ type: "start", partial: message });
				stream.push({ type: "done", reason: "stop", message });
				stream.end(message);
			});
			return stream;
		},
	});
}
"#;

#[tokio::test(flavor = "multi_thread")]
async fn oauth_login_runs_the_extension_flow_and_its_key_reaches_requests() {
    let h = harness(Setup {
        files: vec![("oauth.js", OAUTH_PROVIDER.into())],
        load: vec!["oauth.js"],
        ..Setup::default()
    })
    .await;
    assert_eq!(h.errors(), Vec::<String>::new());
    let runtime = h.session.model_runtime().clone();
    let providers = runtime.login_providers(Some(AuthType::OAuth)).await;
    assert!(providers.iter().any(|provider| provider.id == "echo-oauth"), "{providers:?}");
    let interaction = Arc::new(Interaction { answers: Mutex::new(vec!["123".into()]), ..Interaction::default() });
    let credential = runtime
        .login("echo-oauth", AuthType::OAuth, interaction.as_ref(), &LoginOptions::default(), CancellationToken::new())
        .await
        .unwrap();
    let Credential::OAuth(oauth) = credential else { panic!("expected OAuth") };
    assert_eq!((oauth.access.as_str(), oauth.refresh.as_str()), ("a-123", "r-123"));
    let events = interaction.events.lock().unwrap().clone();
    assert!(events[0].contains("https://example.test/auth"), "{events:?}");
    assert!(events[1].contains("Code?"), "{events:?}");
    let model = runtime.get_model("echo-oauth", "echo").unwrap();
    h.session.set_model(model, false).await.unwrap();
    h.session.prompt("hi", Vec::new()).await.unwrap();
    assert_eq!(h.session.last_assistant_text().as_deref(), Some("auth=key:a-123"));
}
