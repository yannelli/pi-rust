//! Guards that keep in-process test sessions off real model providers. When a test
//! `models.json` fails to load, a session falls back to a built-in model and resolves its key
//! from the developer's environment; these checks fail the test before any request is sent.
#![allow(dead_code)]

use std::path::Path;

use pi_ai::{Model, ModelRegistry};
use pi_coding_agent::agent_session::{AgentSessionRuntime, AgentSessionServices};
use pi_coding_agent::app::{AppOptions, create_app_runtime};
use pi_coding_agent::session::SessionManager;
use pi_coding_agent::virtual_models::VIRTUAL_MODEL_API;

const FALLBACK: &str = "sessions would fall back to a built-in model and send requests to a real provider";

/// Panics when the `models.json` at `path` is invalid.
pub fn assert_models_file_loads(path: &Path) {
    if let Err(error) = ModelRegistry::load(path) {
        panic!("the test models.json does not load, so {FALLBACK}:\n{error}");
    }
}

/// Panics when the services recorded a `models.json` error.
pub fn assert_models_loaded(services: &AgentSessionServices) {
    if let Some(error) = services.model_runtime.load_error() {
        panic!("the test models.json did not load, so {FALLBACK}:\n{error}");
    }
    let errors: Vec<&str> = services
        .diagnostics
        .iter()
        .map(|diagnostic| diagnostic.message.as_str())
        .filter(|message| message.contains("models.json"))
        .collect();
    assert!(errors.is_empty(), "models.json diagnostics mean {FALLBACK}: {errors:?}");
}

/// Whether requests for `model` stay on this machine: a faux API, or a loopback server. A
/// virtual model exists only when the test registers it; its router picks each physical model.
pub fn is_offline(model: &Model) -> bool {
    if model.api.starts_with("faux") || model.api == VIRTUAL_MODEL_API {
        return true;
    }
    let Ok(url) = url::Url::parse(&model.base_url) else { return false };
    match url.host() {
        Some(url::Host::Domain(domain)) => domain.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(address)) => address.is_loopback(),
        Some(url::Host::Ipv6(address)) => address.is_loopback(),
        None => false,
    }
}

fn describe(model: &Model) -> String {
    format!("{}/{} (api {}, base URL {})", model.provider, model.id, model.api, model.base_url)
}

/// Panics when `model` would reach a real provider. Tests that choose a model other than the
/// faux one must serve it from a loopback mock server.
pub fn assert_offline_model(model: Option<&Model>) {
    if let Some(model) = model.filter(|model| !is_offline(model)) {
        panic!("the session model {} is not served by a faux provider or a loopback server", describe(model));
    }
}

/// Panics unless `model` is the faux test model on `api`, the API the harness registered.
pub fn assert_faux_model(model: Option<&Model>, api: &str) {
    match model {
        Some(model) if model.api == api => {}
        Some(model) => panic!("the session resolved {} instead of the faux test model on {api}", describe(model)),
        None => panic!("the session has no model; expected the faux test model on {api}"),
    }
}

/// `create_app_runtime` with the checks above: models loaded and an offline session model.
pub async fn app_runtime(options: AppOptions, manager: SessionManager) -> AgentSessionRuntime {
    let runtime = create_app_runtime(options, manager).await.unwrap();
    assert_models_loaded(&runtime.services());
    assert_offline_model(runtime.session().model().as_ref());
    runtime
}

/// Turn off startup network work, such as catalog refreshes, for in-process sessions.
pub fn offline_process() {
    pi_ai::env::set_child_process_env("PI_OFFLINE", "1");
}
