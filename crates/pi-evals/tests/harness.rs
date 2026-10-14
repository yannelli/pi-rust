//! Port of upstream `test/harness.test.ts`.

use std::path::Path;

use pi_coding_agent::system_prompt::{BuildSystemPromptOptions, render_system_prompt};
use pi_evals::harness::{
    DOCUMENTATION_EVAL_TOOLS, EnvMap, HarnessOptions, ModelSelection, create_pi_documentation_eval_harness,
    exclude_pi_documentation, isolated_environment, process_env, resolve_documentation_variant,
    resolve_model_selection, verify_system_prompt,
};
use pi_evals::plan::DocumentationVariant;

fn env(pairs: &[(&str, &str)]) -> EnvMap {
    pairs.iter().map(|(name, value)| (name.to_string(), value.to_string())).collect()
}

#[test]
fn prefers_an_explicit_harness_model() {
    let explicit = ModelSelection { provider: "anthropic".into(), id: "claude-opus-4-6".into() };
    let defaults = env(&[("PI_PROVIDER", "openai-codex"), ("PI_MODEL", "gpt-5.6-sol")]);
    assert_eq!(resolve_model_selection(Some(&explicit), &defaults).unwrap(), explicit);
}

#[test]
fn uses_trimmed_environment_defaults() {
    let defaults = env(&[("PI_PROVIDER", " openai-codex "), ("PI_MODEL", " gpt-5.6-sol ")]);
    let expected = ModelSelection { provider: "openai-codex".into(), id: "gpt-5.6-sol".into() };
    assert_eq!(resolve_model_selection(None, &defaults).unwrap(), expected);
}

#[test]
fn rejects_incomplete_model_selection() {
    for defaults in [env(&[]), env(&[("PI_PROVIDER", "openai-codex")]), env(&[("PI_MODEL", "gpt-5.6-sol")])] {
        let error = resolve_model_selection(None, &defaults).unwrap_err().to_string();
        assert!(error.contains("Select a harness model explicitly"), "{error}");
    }
}

// Upstream mutates `process.env` and restores it. Rust edition 2024 makes that unsafe, so
// the harness applies an environment plan to the agent process and leaves its own alone.
#[test]
fn removes_runner_metadata_and_restores_the_process_environment() {
    let before = process_env();
    let host = env(&[("PI_EVAL_VARIANT", "with_docs"), ("PI_EVAL_ARTIFACT_DIR", "/tmp/artifacts"), ("HOME", "/root")]);
    let plan = isolated_environment("/tmp/eval-home", "/tmp/eval-agent", &host);
    let isolated = plan.applied_to(&host);
    assert_eq!(isolated.get("HOME").map(String::as_str), Some("/tmp/eval-home"));
    assert_eq!(isolated.get("USERPROFILE").map(String::as_str), Some("/tmp/eval-home"));
    assert_eq!(isolated.get("PI_CODING_AGENT_DIR").map(String::as_str), Some("/tmp/eval-agent"));
    assert!(!isolated.contains_key("PI_EVAL_VARIANT"));
    assert!(!isolated.contains_key("PI_EVAL_ARTIFACT_DIR"));
    assert_eq!(host.get("PI_EVAL_VARIANT").map(String::as_str), Some("with_docs"));
    assert_eq!(process_env(), before);
}

#[test]
fn accepts_both_variants() {
    assert_eq!(resolve_documentation_variant(Some("without_docs")).unwrap(), DocumentationVariant::WithoutDocs);
    assert_eq!(resolve_documentation_variant(Some("with_docs")).unwrap(), DocumentationVariant::WithDocs);
}

#[test]
fn rejects_invalid_variants() {
    for variant in [None, Some(""), Some("other")] {
        assert!(resolve_documentation_variant(variant).unwrap_err().to_string().contains("PI_EVAL_VARIANT"));
    }
}

fn default_prompt() -> String {
    let mut options = BuildSystemPromptOptions::new(Path::new("/workspace"));
    options.selected_tools = DOCUMENTATION_EVAL_TOOLS.iter().map(|tool| tool.to_string()).collect();
    render_system_prompt(&options.with_builtin_tool_info()).unwrap()
}

// pi-rust's docs section links the project site instead of local `docs/models.md`,
// README, and examples paths, so the checks for those paths use the link.
#[test]
fn strips_only_the_documentation_routing_section_from_the_default_prompt() {
    let prompt = default_prompt();
    assert!(prompt.contains("\n<docs>\nPi documentation (read only"));
    assert!(prompt.contains("\n<rules>\n"));
    assert!(prompt.contains("\n<cwd>\n/workspace\n</cwd>"));
    assert!(prompt.contains("github.com/yannelli/pi-rust"));

    let stripped = exclude_pi_documentation(&prompt).unwrap();
    assert!(stripped.contains("\n<rules>\n"));
    assert!(stripped.contains("\n<cwd>\n/workspace\n</cwd>"));
    assert!(!stripped.contains("<docs>"));
    assert!(!stripped.contains("Pi documentation"));
    assert!(!stripped.contains("github.com/yannelli/pi-rust"));
}

#[test]
fn verifies_the_prompt_that_was_sent() {
    let prompt = default_prompt();
    let stripped = exclude_pi_documentation(&prompt).unwrap();
    assert_eq!(verify_system_prompt(&stripped, "without_docs", Some(false)).unwrap(), stripped);
    let error = verify_system_prompt(&prompt, "without_docs", Some(false)).unwrap_err().to_string();
    assert!(error.contains("does not match"), "{error}");
}

#[test]
fn fails_closed_when_prompt_markers_are_missing() {
    let missing = exclude_pi_documentation("Instructions").unwrap_err().to_string();
    assert!(missing.contains("no Pi documentation section"), "{missing}");
    let no_cwd = exclude_pi_documentation("\n<docs>\nPi documentation\n</docs>").unwrap_err().to_string();
    assert!(no_cwd.contains("no working-directory section"), "{no_cwd}");
}

#[test]
fn rejects_documentation_harnesses_outside_the_container_sandbox() {
    let result = create_pi_documentation_eval_harness(HarnessOptions::default(), &env(&[]));
    assert!(result.err().unwrap().to_string().contains("isolated container sandbox"));
}
