//! The harness fails fast instead of falling back to a built-in model that would reach a real
//! provider with credentials from the environment.

use std::collections::BTreeMap;

use pi_ai::auth::{ApiKeyCredential, Credential};
use pi_ai::env::credential_env_vars;
use pi_ai::providers::faux::FauxResponse;
use pi_coding_agent::agent_session::auth::request_auth;
use pi_coding_agent::agent_session::{ModelRuntime, ModelRuntimeOptions};
use pi_coding_agent::session::SessionManager;
use serde_json::json;

use crate::harness::{Options, harness};
use crate::offline_guard::{app_runtime, assert_offline_model, is_offline};

const FAKE_OPENAI_KEY: &str = "sk-pi-rust-test-fake-openai-key";
/// Set in the child process that runs a test with [`FAKE_OPENAI_KEY`] in its environment.
const FAKE_KEY_CHILD: &str = "PI_RUST_TEST_FAKE_KEY_CHILD";

#[tokio::test]
#[should_panic(expected = "the test models.json did not load")]
async fn a_models_json_that_does_not_load_fails_the_harness() {
    // A cost without `cacheRead` and `cacheWrite` is invalid, so the whole file is ignored.
    let options = Options {
        tiny: json!({"cost": {"input": 3, "output": 15}}),
        responses: vec![FauxResponse::text("never sent")],
        ..Options::default()
    };
    harness(options).await;
}

#[tokio::test]
#[should_panic(expected = "instead of the faux test model")]
async fn a_session_on_a_builtin_model_fails_the_harness() {
    let key = Credential::ApiKey(ApiKeyCredential { key: Some("sk-test".into()), env: None });
    let options = Options {
        settings: json!({"defaultProvider": "anthropic", "defaultModel": "claude-sonnet-4-5"}),
        credentials: BTreeMap::from([("anthropic".to_string(), key)]),
        ..Options::default()
    };
    harness(options).await;
}

#[tokio::test]
async fn the_default_harness_runs_on_the_faux_model() {
    let h = harness(Options { responses: vec![FauxResponse::text("hi")], ..Options::default() }).await;
    let model = h.session.model().unwrap();
    assert!(model.api.starts_with("faux-session-") && is_offline(&model), "{model:?}");
    h.session.prompt("hello", Vec::new()).await.unwrap();
    assert_eq!(h.requests().len(), 1);
}

#[test]
fn builtin_models_are_not_offline_and_loopback_servers_are() {
    let mut model = pi_ai::models::builtin_models().iter().find(|model| model.provider == "openai").unwrap().clone();
    assert!(!is_offline(&model), "{}", model.base_url);
    for base_url in ["http://127.0.0.1:9/v1", "http://localhost:8080", "http://[::1]:1234/v1"] {
        model.base_url = base_url.into();
        assert!(is_offline(&model), "{base_url}");
    }
    model.base_url = "http://unused".into();
    assert!(!is_offline(&model));
    model.api = "faux-session-1".into();
    assert!(is_offline(&model));
}

#[test]
#[should_panic(expected = "is not served by a faux provider or a loopback server")]
fn a_builtin_model_fails_the_offline_check() {
    let model = pi_ai::models::builtin_models().iter().find(|model| model.provider == "anthropic").unwrap();
    assert_offline_model(Some(model));
}

/// Runs the test `name` of this binary in a child process whose environment holds a fake
/// `OPENAI_API_KEY` and no other credential, so the key never reaches the other tests.
fn run_with_fake_openai_key(name: &str) {
    let test = format!("{}::{name}", module_path!().split_once("::").unwrap().1);
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command.args(["--exact", test.as_str()]);
    for variable in credential_env_vars() {
        command.env_remove(variable);
    }
    let output = command.env("OPENAI_API_KEY", FAKE_OPENAI_KEY).env(FAKE_KEY_CHILD, "1").output().unwrap();
    let (stdout, stderr) = (String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
    assert!(output.status.success() && stdout.contains(" 1 passed"), "{stdout}\n{stderr}");
}

#[tokio::test]
async fn an_openai_key_in_the_process_environment_stays_hidden_from_harness_sessions() {
    if std::env::var_os(FAKE_KEY_CHILD).is_none() {
        return run_with_fake_openai_key("an_openai_key_in_the_process_environment_stays_hidden_from_harness_sessions");
    }
    assert_eq!(std::env::var("OPENAI_API_KEY").as_deref(), Ok(FAKE_OPENAI_KEY));
    // A runtime with default options reads the process environment, as `pi` does.
    let dir = tempfile::tempdir().unwrap();
    let default = ModelRuntime::create(ModelRuntimeOptions::for_agent_dir(dir.path())).await;
    assert!(default.has_configured_auth("openai") && default.check_auth("openai").await);
    assert_eq!(default.api_key_for("openai").await.as_deref(), Some(FAKE_OPENAI_KEY));

    let h = harness(Options { responses: vec![FauxResponse::text("hi")], ..Options::default() }).await;
    let runtime = &h.services.model_runtime;
    assert!(!runtime.has_configured_auth("openai") && !runtime.check_auth("openai").await);
    assert_eq!(runtime.api_key_for("openai").await, None);
    assert!(runtime.available_models().iter().all(|model| model.provider != "openai"));
    let gpt = runtime.all_models().into_iter().find(|model| model.provider == "openai").unwrap();
    let error = h.session.set_model(gpt.clone(), false).await.unwrap_err();
    assert!(error.to_string().contains("No API key for openai/"), "{error}");
    assert!(request_auth(runtime, &gpt).await.is_err());
    assert_eq!(h.session.model().unwrap().provider, "local");

    let app = app_runtime(h.app_options(), SessionManager::in_memory(&h.cwd)).await;
    let session = app.session();
    assert!(!app.services().model_runtime.check_auth("openai").await);
    assert!(session.set_model(gpt, false).await.is_err());
    session.prompt("hello", Vec::new()).await.unwrap();
    assert_eq!(h.requests().len(), 1);
}
