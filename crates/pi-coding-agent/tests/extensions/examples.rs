//! Upstream example extensions, unchanged.

use pi_ai::providers::faux::FauxResponse;
use serde_json::json;

use crate::harness::{Setup, example, harness};

#[tokio::test(flavor = "multi_thread")]
async fn hello_registers_a_tool_the_model_calls() {
    let h = harness(Setup {
        files: vec![("hello.ts", example("hello.ts"))],
        load: vec!["hello.ts"],
        responses: vec![FauxResponse::tool_call("call-1", "hello", json!({"name": "Ada"})), FauxResponse::text("done")],
        ..Setup::default()
    })
    .await;
    assert_eq!(h.errors(), Vec::<String>::new());
    assert!(h.session.active_tool_names().contains(&"hello".to_string()), "{:?}", h.session.active_tool_names());
    h.session.prompt("greet", Vec::new()).await.unwrap();
    let transcript = h.transcript();
    assert!(transcript.iter().any(|line| line == "toolResult: Hello, Ada!"), "{transcript:?}");
}
