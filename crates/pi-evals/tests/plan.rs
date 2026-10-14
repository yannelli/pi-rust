//! Port of upstream `test/plan.test.ts`.

use pi_evals::plan::{DiscoveredEvalCase, DocumentationVariant, create_task_plan, parse_discovered_cases};
use serde_json::{Value, json};

fn discovered() -> Value {
    json!([{ "name": "Add model > adds the model", "file": "evals/models.docs.eval.rs" }])
}

#[test]
fn derives_stable_case_identity_from_ordinary_names() {
    assert_eq!(
        parse_discovered_cases(&discovered()).unwrap(),
        vec![DiscoveredEvalCase {
            full_name: "Add model > adds the model".into(),
            eval_set: "Add model".into(),
            case_id: "adds the model".into(),
            file: "evals/models.docs.eval.rs".into(),
        }]
    );
}

#[test]
fn rejects_ambiguous_and_duplicate_identities() {
    let ambiguous = parse_discovered_cases(&json!([{ "name": "adds the model", "file": "model.rs" }]));
    assert!(ambiguous.unwrap_err().to_string().contains("<eval set> > <case>"));
    let item = discovered()[0].clone();
    let duplicate = parse_discovered_cases(&json!([item.clone(), item]));
    assert!(duplicate.unwrap_err().to_string().contains("Duplicate eval case identity"));
}

#[test]
fn creates_one_isolated_task_per_case_variant_model_and_repetition() {
    let cases = parse_discovered_cases(&discovered()).unwrap();
    let tasks = create_task_plan(&cases, "fixture/model", 2).unwrap();
    assert_eq!(tasks.len(), 4);
    let order: Vec<(DocumentationVariant, u32)> = tasks.iter().map(|task| (task.variant, task.run_number)).collect();
    use DocumentationVariant::{WithDocs, WithoutDocs};
    assert_eq!(order, vec![(WithoutDocs, 1), (WithDocs, 1), (WithDocs, 2), (WithoutDocs, 2)]);
}

#[test]
fn rejects_invalid_model_identities_and_repetitions() {
    let cases = parse_discovered_cases(&discovered()).unwrap();
    assert!(create_task_plan(&cases, "model", 1).unwrap_err().to_string().contains("provider and model"));
    assert!(create_task_plan(&cases, "fixture/model", 0).unwrap_err().to_string().contains("positive integer"));
}
