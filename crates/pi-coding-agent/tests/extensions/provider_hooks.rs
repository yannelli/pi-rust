//! Provider request hooks against a local OpenAI-compatible server: `before_provider_request`,
//! `before_provider_headers`, `after_provider_response`, and `provider_stream_event`.

use serde_json::json;

use crate::extensions::harness::{Setup, harness};
use crate::extensions::http_mock::{Reply, Server};

const STREAM: &str = concat!(
    "data: {\"id\":\"r1\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hel\"},\"finish_reason\":null}]}\n\n",
    "data: {\"id\":\"r1\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"lo\"},\"finish_reason\":null}]}\n\n",
    "data: {\"id\":\"r1\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],",
    "\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":2,\"total_tokens\":5}}\n\n",
    "data: [DONE]\n\n",
);

#[tokio::test(flavor = "multi_thread")]
async fn request_hooks_edit_the_payload_and_headers_and_see_the_response() {
    let server =
        Server::start(|_| Reply { status: 200, content_type: "text/event-stream", body: STREAM.as_bytes().to_vec() })
            .await;
    let source = r#"export default function (pi) {
	const seen = [];
	pi.on("before_provider_request", async (event) => {
		await new Promise((resolve) => setTimeout(resolve, 20));
		return { ...event.payload, temperature: 0.25 };
	});
	pi.on("before_provider_headers", (event) => { event.headers["x-extension"] = "yes"; });
	pi.on("after_provider_response", (event) => { seen.push(`status ${event.status}`); });
	pi.on("provider_stream_event", (event) => { seen.push(`${event.provider}/${event.model}`); });
	pi.registerCommand("seen", { handler: async (_args, ctx) => ctx.ui.notify([...new Set(seen)].join(" ")) });
}
"#;
    let models = json!({"providers": {"mock": {"baseUrl": format!("{}/v1", server.url), "api": "openai-completions",
        "apiKey": "test-key", "models": [{"id": "m1", "contextWindow": 1000, "maxTokens": 100}]}}});
    let h = harness(Setup {
        files: vec![("hooks.ts", source.into())],
        load: vec!["hooks.ts"],
        models: Some(models),
        settings: json!({"defaultProvider": "mock", "defaultModel": "m1"}),
        ..Setup::default()
    })
    .await;
    h.session.prompt("hi", Vec::new()).await.unwrap();
    assert_eq!(h.transcript().last().map(String::as_str), Some("assistant: hello"));
    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].json()["temperature"], 0.25);
    assert_eq!(requests[0].header("x-extension"), Some("yes"));
    h.session.prompt("/seen", Vec::new()).await.unwrap();
    assert_eq!(h.ui.calls(), vec!["notify(info): status 200 mock/m1"]);
    assert_eq!(h.errors(), Vec::<String>::new());
}

#[tokio::test(flavor = "multi_thread")]
async fn before_provider_auth_sends_a_request_with_another_login_of_the_provider() {
    use pi_ai::auth::{Credential, CredentialStore, OAuthAuth, OAuthCredential};
    use std::sync::Arc;

    let server =
        Server::start(|_| Reply { status: 200, content_type: "text/event-stream", body: STREAM.as_bytes().to_vec() })
            .await;
    // `mock-2` reuses the core login of `mock`, so it is an account of `mock` with no models.
    let source = r#"export default function (pi) {
	let account = "";
	const seen = [];
	pi.registerProvider("mock-2", { oauth: { builtin: "mock", name: "Mock #2" }, models: [] });
	pi.on("before_provider_auth", (event) => {
		seen.push(`${event.provider}/${event.model.id}`);
		return account ? { authProvider: account } : undefined;
	});
	pi.registerCommand("account", { handler: async (args) => { account = args.trim(); } });
	pi.registerCommand("seen", { handler: async (_args, ctx) => ctx.ui.notify([...new Set(seen)].join(" ")) });
}
"#;
    let models = json!({"providers": {"mock": {"baseUrl": format!("{}/v1", server.url), "api": "openai-completions",
        "apiKey": "test-key", "models": [{"id": "m1", "contextWindow": 1000, "maxTokens": 100}]}}});
    let flow = Arc::new(crate::extensions::native_contracts::FakeAnthropic::default());
    let oauth: pi_ai::auth::OAuthLookup =
        Arc::new(move |id: &str| (id == "mock").then(|| flow.clone() as Arc<dyn OAuthAuth>));
    let h = harness(Setup {
        files: vec![("accounts.ts", source.into())],
        load: vec!["accounts.ts"],
        models: Some(models),
        settings: json!({"defaultProvider": "mock", "defaultModel": "m1"}),
        oauth: Some(oauth),
        ..Setup::default()
    })
    .await;
    let credential = OAuthCredential {
        refresh: "r".into(),
        access: "second".into(),
        expires: pi_ai::now_ms() + 3_600_000,
        extra: Default::default(),
    };
    let stored = Credential::OAuth(credential);
    h.credentials.update("mock-2", Box::new(move |_| Box::pin(async move { Ok(Some(stored)) }))).await.unwrap();
    let listed: Vec<String> = h
        .session
        .model_runtime()
        .available_models()
        .iter()
        .map(|model| format!("{}/{}", model.provider, model.id))
        .collect();
    assert_eq!(listed, vec!["mock/m1"], "an account adds no models");

    h.session.prompt("hi", Vec::new()).await.unwrap();
    h.session.prompt("/account mock-2", Vec::new()).await.unwrap();
    h.session.prompt("again", Vec::new()).await.unwrap();
    h.session.prompt("/account elsewhere", Vec::new()).await.unwrap();
    h.session.prompt("refused", Vec::new()).await.unwrap();

    let requests = server.requests();
    let keys: Vec<Option<&str>> = requests.iter().map(|request| request.header("authorization")).collect();
    assert_eq!(keys, vec![Some("Bearer test-key"), Some("Bearer second")]);
    let last = serde_json::to_value(h.session.messages().last().unwrap()).unwrap();
    assert_eq!(last["errorMessage"], "elsewhere is not an account of provider mock", "{last}");
    assert_eq!(h.session.model().map(|model| model.provider), Some("mock".into()), "the model stays the same");
    h.session.prompt("/seen", Vec::new()).await.unwrap();
    assert_eq!(h.ui.calls(), vec!["notify(info): mock/m1"]);
    assert_eq!(h.errors(), Vec::<String>::new());
}
