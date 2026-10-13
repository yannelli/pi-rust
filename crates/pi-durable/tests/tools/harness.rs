//! Not upstream: the coding tools run once through a Harness, which gives them a real `ToolExecutionApi`.

use std::sync::Arc;

use pi_ai::{ContentBlock, Message, StopReason};
use pi_durable::entries::TOOL_RESULT_ENTRY;
use pi_durable::env::{LocalEnvOptions, LocalExecutionEnv, SharedEnv};
use pi_durable::harness::*;
use pi_durable::storage::MemoryStorage;
use pi_durable::testing::faux::{
    FauxMessageOptions, FauxResponseStep, RegisterFauxProviderOptions, faux_assistant_message, faux_provider,
    faux_text, faux_tool_call_with_id,
};
use pi_durable::tools::coding_tools;
use serde_json::{Value, json};

use super::support::ctx;

fn call(name: &str, args: Value, id: &str) -> FauxResponseStep {
    let blocks = vec![faux_tool_call_with_id(name, args, id)];
    faux_assistant_message(blocks, FauxMessageOptions::stop_reason(StopReason::ToolUse)).into()
}

#[tokio::test]
async fn coding_tools_write_and_run_commands_through_the_harness() {
    let dir = tempfile::tempdir().unwrap();
    let env: SharedEnv = Arc::new(LocalExecutionEnv::new(LocalEnvOptions::new(dir.path().to_str().unwrap())));
    let faux = faux_provider(RegisterFauxProviderOptions::default());
    faux.set_responses(vec![
        call("write", json!({ "path": "a.txt", "content": "hello" }), "c1"),
        call("bash", json!({ "command": "cat a.txt; exit 2" }), "c2"),
        faux_assistant_message(vec![faux_text("done")], FauxMessageOptions::default()).into(),
    ]);
    let registry = Arc::new(create_registry());
    registry.install(coding_tools()).unwrap();
    let options =
        HarnessOptions::new(faux.provider(), registry as Arc<dyn RegistryReader>).with_shared_env(env.clone());
    let harness = Harness::open(Arc::new(MemoryStorage::new()), options, &ctx()).await.unwrap();
    let model = ModelRef { provider: "faux".into(), model_id: "faux-1".into() };
    let root = harness.root(&ctx(), CreateOptions::default().with_agent(AgentChange::model(model))).await.unwrap();
    root.submit(SubmissionDraft::input("go"), &ctx()).await.unwrap().wait(&ctx()).await.unwrap();

    assert_eq!(env.read_text_file("a.txt", &ctx()).await.unwrap(), "hello");
    let page = root.entries(EntryRange::default(), 1000, None, &ctx()).await.unwrap();
    let results: Vec<(String, bool)> = page
        .items
        .iter()
        .rev()
        .filter(|entry| TOOL_RESULT_ENTRY.is(Some(entry)))
        .filter_map(|entry| match entry.model.as_ref()?.first()? {
            Message::ToolResult(result) => {
                let text = result.content.iter().filter_map(ContentBlock::as_text).collect::<String>();
                Some((text, result.is_error))
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        results,
        [
            ("Successfully wrote to a.txt".to_string(), false),
            ("hello<harness>\n[error] Command exited with code 2\n</harness>".to_string(), true)
        ]
    );
    harness.close(&ctx()).await.unwrap();
}
