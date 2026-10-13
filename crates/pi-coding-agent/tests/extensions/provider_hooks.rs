//! Provider request hooks against a local OpenAI-compatible server: `before_provider_request`,
//! `before_provider_headers`, `after_provider_response`, and `provider_stream_event`.

use serde_json::json;

use crate::harness::{Setup, harness};
use crate::http_mock::{Reply, Server};

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
	pi.on("before_provider_request", (event) => ({ ...event.payload, temperature: 0.25 }));
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
