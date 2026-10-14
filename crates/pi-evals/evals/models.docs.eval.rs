//! Documentation eval: add a model to a built-in provider without replacing its models.

use std::sync::Arc;

use serde_json::json;

use super::configured_runtime::inspect_added_model;
use super::{load_agent_runtime, run_local};
use crate::framework::{EvalCase, EvalSuite, Judge, Project, StructuredOutputJudge, describe_eval};
use crate::harness::{HarnessOptions, OutputContext, Step, create_pi_documentation_eval_harness, process_env};

const PROVIDER_ID: &str = "openai";
const MODEL_ID: &str = "fixture-chat";
const MODEL_NAME: &str = "Fixture Chat";

pub fn suite() -> EvalSuite {
    let expected = json!({
        "result": {
            "model": {
                "id": MODEL_ID, "name": MODEL_NAME, "provider": PROVIDER_ID, "reasoning": true, "input": ["text"],
                "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 },
                "contextWindow": 32768, "maxTokens": 4096,
            },
            "existingModelsPreserved": true,
        },
    });
    let harness = Arc::new(|| {
        let output = Arc::new(|context: OutputContext| {
            Box::pin(run_local(move || async move {
                let runtime = load_agent_runtime(&context.agent_dir, &context.workspace).await?;
                let session = runtime.session();
                Ok(inspect_added_model(session.model_runtime(), PROVIDER_ID, MODEL_ID).await)
            })) as _
        });
        create_pi_documentation_eval_harness(
            HarnessOptions { output: Some(output), ..HarnessOptions::default() },
            &process_env(),
        )
    });
    let judges: Vec<Arc<dyn Judge>> = vec![Arc::new(StructuredOutputJudge { expected })];
    let prompt = format!(
        "Configure this running Pi installation with a new `{PROVIDER_ID}/{MODEL_ID}` model. Do not create project-local configuration. Show it as “{MODEL_NAME}”. It accepts text, supports reasoning, has a 32,768-token context window and a 4,096-token maximum output, and has no usage cost."
    );
    EvalSuite {
        file: "evals/models.docs.eval.rs",
        project: Project::Docs,
        name: "Add model to existing provider".into(),
        cases: vec![EvalCase {
            name: "adds the model without replacing existing models".into(),
            run: describe_eval(harness, judges, None, vec![Step::Prompt(prompt), Step::Reload]),
        }],
    }
}
