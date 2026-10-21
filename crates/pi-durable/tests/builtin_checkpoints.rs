//! Port of the first case of upstream `test/codemode-checkpoints.test.ts`: checkpoint policies of built-in documents.
//! The code mode store case has no counterpart because pi-durable has no code mode documents.

use pi_chord::Json;
use pi_durable::documents::CheckpointInfo;
use pi_durable::harness::AGENT_DOC;
use pi_durable::harness::tool::NESTED_CALL_DOC;
use pi_durable::harness::usage::PROVIDER_DOC;
use serde_json::json;

fn info() -> CheckpointInfo {
    CheckpointInfo { deltas_since_base: 1000 }
}

#[test]
fn keeps_task_scoped_documents_as_deltas_until_their_task_retires() {
    let empty = Json::from(json!({}));
    assert!(!NESTED_CALL_DOC.definition().checkpoint_when(&empty, &[], info()).unwrap());
}

#[test]
fn stores_every_agent_and_provider_change_as_a_complete_base() {
    let empty = Json::from(json!({}));
    assert!(AGENT_DOC.definition().checkpoint_when(&empty, &[], info()).unwrap());
    assert!(PROVIDER_DOC.definition().checkpoint_when(&empty, &[], info()).unwrap());
}
