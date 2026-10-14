//! Documentation eval: implement a custom streaming provider from an API fixture.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::json;

use super::acme_server::{
    AcmeServer, STREAM_MODEL_ID, STREAM_PROBE_PROMPT, STREAM_PROBE_RESPONSE, STREAM_PROVIDER_ID, ServerMode,
    stream_api_documentation,
};
use super::configured_runtime::ProviderScenario;
use super::openai_provider_docs::provider_output;
use crate::framework::{CaseReport, EvalCase, EvalSuite, Judge, Project, StructuredOutputJudge, evaluate};
use crate::harness::{HarnessOptions, Step, create_pi_documentation_eval_harness, process_env};

const DOCUMENTATION_PATH: &str = "fixtures/acme-stream-api.json";
const STREAM_API_KEY: &str = "resolved-stream-key";

async fn implements_a_provider() -> CaseReport {
    let server = AcmeServer::new(ServerMode::Stream);
    if let Err(error) = server.start().await {
        return CaseReport { failures: vec![error.to_string()], ..CaseReport::default() };
    }
    server.reset();
    let expected = json!({
        "result": {
            "validRequestReceived": true,
            "model": {
                "id": STREAM_MODEL_ID, "name": "Acme Stream Chat", "provider": STREAM_PROVIDER_ID, "reasoning": false,
                "input": ["text"], "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 },
                "contextWindow": 16384, "maxTokens": 2048,
            },
            "response": { "text": STREAM_PROBE_RESPONSE, "stopReason": "stop", "inputTokens": 4, "outputTokens": 3 },
        },
    });
    let key = ("ACME_STREAM_API_KEY".to_string(), STREAM_API_KEY.to_string());
    let scenario = ProviderScenario {
        provider_id: STREAM_PROVIDER_ID.into(),
        model_id: STREAM_MODEL_ID.into(),
        prompt: STREAM_PROBE_PROMPT.into(),
        env: BTreeMap::from([key.clone()]),
        max_tokens: 32,
        valid_request_received: server.probe(),
    };
    // Upstream sets the key in the process environment; here the agent process gets it.
    let options = HarnessOptions {
        workspace_files: vec![(DOCUMENTATION_PATH.into(), stream_api_documentation())],
        env: vec![key],
        output: Some(provider_output(scenario)),
        ..HarnessOptions::default()
    };
    let harness = create_pi_documentation_eval_harness(options, &process_env());
    let judges: Vec<Arc<dyn Judge>> = vec![Arc::new(StructuredOutputJudge { expected })];
    let prompt = format!(
        "Configure this running Pi installation with Acme Stream as a provider. Do not create a project package or modify project source. Its provider ID is {STREAM_PROVIDER_ID}, its API is at {}, and its API documentation is in ./{DOCUMENTATION_PATH}. Read its credential from the ACME_STREAM_API_KEY environment variable.\n\nIt offers one model, {STREAM_MODEL_ID}, shown as “Acme Stream Chat”. The model accepts text, does not support reasoning, has a 16,384-token context window and a 2,048-token maximum output, and has no usage cost.",
        server.origin()
    );
    let report = evaluate(harness, &judges, None, &[Step::Prompt(prompt), Step::Reload]).await;
    server.stop();
    report
}

pub fn suite() -> EvalSuite {
    EvalSuite {
        file: "evals/custom-provider.docs.eval.rs",
        project: Project::Docs,
        name: "Add custom streaming provider".into(),
        cases: vec![EvalCase {
            name: "implements a provider from an API fixture".into(),
            run: Arc::new(|| Box::pin(implements_a_provider())),
        }],
    }
}
