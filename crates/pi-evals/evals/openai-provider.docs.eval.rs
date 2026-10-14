//! Documentation eval: configure an OpenAI-compatible provider that works through pi.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::json;

use super::acme_server::{
    AcmeServer, OPENAI_MODEL_ID, OPENAI_PROBE_PROMPT, OPENAI_PROBE_RESPONSE, OPENAI_PROVIDER_ID, ServerMode,
};
use super::configured_runtime::{ProviderScenario, inspect_provider};
use super::{load_agent_runtime, run_local};
use crate::framework::{CaseReport, EvalCase, EvalSuite, Judge, Project, StructuredOutputJudge, evaluate};
use crate::harness::{HarnessOptions, OutputContext, Step, create_pi_documentation_eval_harness, process_env};

/// Harness output: the configured provider probed through the reloaded runtime.
pub fn provider_output(scenario: ProviderScenario) -> crate::harness::OutputFn {
    Arc::new(move |context: OutputContext| {
        let scenario = scenario.clone();
        Box::pin(run_local(move || async move {
            let runtime = load_agent_runtime(&context.agent_dir, &context.workspace).await?;
            let session = runtime.session();
            Ok(inspect_provider(session.model_runtime(), &scenario).await)
        }))
    })
}

async fn configures_a_provider() -> CaseReport {
    let server = AcmeServer::new(ServerMode::OpenAi);
    if let Err(error) = server.start().await {
        return CaseReport { failures: vec![error.to_string()], ..CaseReport::default() };
    }
    server.reset();
    let expected = json!({
        "result": {
            "validRequestReceived": true,
            "model": {
                "id": OPENAI_MODEL_ID, "name": "Acme Chat", "provider": OPENAI_PROVIDER_ID, "reasoning": false,
                "input": ["text"], "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 },
                "contextWindow": 32768, "maxTokens": 4096,
            },
            "response": { "text": OPENAI_PROBE_RESPONSE, "stopReason": "stop", "inputTokens": 3, "outputTokens": 2 },
        },
    });
    let scenario = ProviderScenario {
        provider_id: OPENAI_PROVIDER_ID.into(),
        model_id: OPENAI_MODEL_ID.into(),
        prompt: OPENAI_PROBE_PROMPT.into(),
        env: BTreeMap::from([("ACME_API_KEY".to_string(), "resolved-acme-key".to_string())]),
        max_tokens: 32,
        valid_request_received: server.probe(),
    };
    let options = HarnessOptions { output: Some(provider_output(scenario)), ..HarnessOptions::default() };
    let harness = create_pi_documentation_eval_harness(options, &process_env());
    let judges: Vec<Arc<dyn Judge>> = vec![Arc::new(StructuredOutputJudge { expected })];
    let prompt = format!(
        "Configure this running Pi installation with Acme as a provider. Do not create project-local configuration. Its provider ID is {OPENAI_PROVIDER_ID}, its API is at {}, and it uses OpenAI Chat Completions. Read its API key from the ACME_API_KEY environment variable.\n\nThe provider offers one model, {OPENAI_MODEL_ID}, shown as “Acme Chat”. It accepts text, does not support reasoning, has a 32,768-token context window and a 4,096-token maximum output, and has no usage cost.",
        server.base_url()
    );
    let report = evaluate(harness, &judges, None, &[Step::Prompt(prompt), Step::Reload]).await;
    server.stop();
    report
}

pub fn suite() -> EvalSuite {
    EvalSuite {
        file: "evals/openai-provider.docs.eval.rs",
        project: Project::Docs,
        name: "Add OpenAI-compatible provider".into(),
        cases: vec![EvalCase {
            name: "configures a provider that works through Pi".into(),
            run: Arc::new(|| Box::pin(configures_a_provider())),
        }],
    }
}
