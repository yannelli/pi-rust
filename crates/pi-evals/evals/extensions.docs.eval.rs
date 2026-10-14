//! Documentation eval: create, reload, and invoke a tool extension.

use std::sync::Arc;

use pi_ai::Message;
use serde_json::{Value, json};

use super::{load_agent_runtime, run_local};
use crate::framework::{EvalCase, EvalSuite, Judge, Project, StructuredOutputJudge, ToolCallJudge, describe_eval};
use crate::harness::{
    DOCUMENTATION_EVAL_TOOLS, HarnessOptions, OutputContext, Step, create_pi_documentation_eval_harness, process_env,
};
use crate::session::content_text;

const TOOL_NAME: &str = "hello";
const TOOL_RESULT: &str = "Hello, Bob!";

fn tool_result(messages: &[Message]) -> Value {
    let found = messages.iter().rev().find_map(|message| match message {
        Message::ToolResult(result) if result.tool_name == TOOL_NAME && !result.is_error => Some(result),
        _ => None,
    });
    found.map_or(Value::Null, |result| json!(content_text(&result.content)))
}

async fn output(context: OutputContext) -> anyhow::Result<Value> {
    let OutputContext { response, messages, agent_dir, workspace, .. } = context;
    let (errors, loaded) = run_local(move || async move {
        let runtime = load_agent_runtime(&agent_dir, &workspace).await?;
        let runner = runtime.session().extension_runner();
        let errors: Vec<Value> = runner
            .as_ref()
            .map(|runner| runner.load_errors())
            .unwrap_or_default()
            .into_iter()
            .map(|error| json!({ "path": error.path, "error": error.error }))
            .collect();
        let loaded = runner.is_some_and(|runner| runner.registry().tools.iter().any(|tool| tool.name == TOOL_NAME));
        Ok(json!([errors, loaded]))
    })
    .await
    .map(|pair| (pair[0].clone(), pair[1].clone()))?;
    Ok(
        json!({ "response": response, "extensionErrors": errors, "toolLoaded": loaded, "toolResult": tool_result(&messages) }),
    )
}

pub fn suite() -> EvalSuite {
    let harness = Arc::new(|| {
        let mut tools: Vec<String> = DOCUMENTATION_EVAL_TOOLS.iter().map(|tool| tool.to_string()).collect();
        tools.push(TOOL_NAME.into());
        let output = Arc::new(|context| Box::pin(output(context)) as _);
        let options = HarnessOptions { tools: Some(tools), output: Some(output), ..HarnessOptions::default() };
        create_pi_documentation_eval_harness(options, &process_env())
    });
    let expected =
        json!({ "response": TOOL_RESULT, "extensionErrors": [], "toolLoaded": true, "toolResult": TOOL_RESULT });
    let judges: Vec<Arc<dyn Judge>> = vec![
        Arc::new(StructuredOutputJudge { expected }),
        Arc::new(ToolCallJudge { expected_tools: vec![(TOOL_NAME.into(), json!({ "name": "Bob" }))] }),
    ];
    let input = vec![
        Step::Prompt("Configure this running Pi installation with an extension containing a hello tool that takes a name and returns a greeting. Do not create project source. For example, passing Bob should return `Hello, Bob!`.".into()),
        Step::Reload,
        Step::Prompt("Use the hello tool to greet Bob. Respond with exactly the tool's greeting and nothing else.".into()),
    ];
    EvalSuite {
        file: "evals/extensions.docs.eval.rs",
        project: Project::Docs,
        name: "Create and use a tool extension".into(),
        cases: vec![EvalCase {
            name: "creates, reloads, and invokes the extension".into(),
            run: describe_eval(harness, judges, None, input),
        }],
    }
}
