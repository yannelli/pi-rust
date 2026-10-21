mod personalized;
mod refresh;
mod remote;

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use pi_ai::Model;
use pi_ai::auth::Credential;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::*;

/// Accepts every publication and keeps the persisted entry, like upstream's test `publish`.
pub(crate) struct RecordingPublisher {
    pub entry: Mutex<Option<ModelsStoreEntry>>,
    pub accept: AtomicBool,
}

impl RecordingPublisher {
    pub fn new(entry: Option<ModelsStoreEntry>) -> Self {
        Self { entry: Mutex::new(entry), accept: AtomicBool::new(true) }
    }

    pub fn stored(&self) -> Option<ModelsStoreEntry> {
        self.entry.lock().unwrap().clone()
    }
}

#[async_trait]
impl CatalogPublisher for RecordingPublisher {
    async fn publish(&self, persist: Persist) -> bool {
        if !self.accept.load(Ordering::SeqCst) {
            return false;
        }
        match persist {
            Persist::Unchanged => {}
            Persist::Write(entry) => *self.entry.lock().unwrap() = Some(entry),
            Persist::Delete => *self.entry.lock().unwrap() = None,
        }
        true
    }
}

/// Run one refresh phase of `catalog` against the publisher's stored entry.
pub(crate) async fn refresh_with(
    catalog: &dyn DynamicCatalog,
    publisher: &RecordingPublisher,
    credential: Option<&Credential>,
    allow_network: bool,
    force: bool,
) -> anyhow::Result<()> {
    let stored = publisher.stored().map(ModelsStoreEntry::with_known_model_types);
    let cancel = CancellationToken::new();
    let context =
        RefreshContext { credential, stored: stored.as_ref(), allow_network, force, cancel: &cancel, publisher };
    catalog.refresh(&context).await
}

pub(crate) fn model(provider: &str, id: &str) -> Model {
    serde_json::from_value(model_json(provider, id)).unwrap()
}

pub(crate) fn model_json(provider: &str, id: &str) -> Value {
    json!({"id": id, "name": id, "api": "openai-completions", "provider": provider,
        "baseUrl": "https://example.test/v1", "reasoning": false, "input": ["text"],
        "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0}, "contextWindow": 1000, "maxTokens": 100})
}

pub(crate) fn ids(models: &[Model]) -> Vec<&str> {
    models.iter().map(|model| model.id.as_str()).collect()
}

struct FixedCatalog {
    id: &'static str,
    models: Vec<Model>,
}

#[async_trait]
impl DynamicCatalog for FixedCatalog {
    fn provider_id(&self) -> &str {
        self.id
    }

    fn compose(&self, builtin: Vec<Model>) -> Vec<Model> {
        merge_models(builtin, &self.models)
    }

    async fn refresh(&self, _context: &RefreshContext<'_>) -> anyhow::Result<()> {
        Ok(())
    }
}

#[test]
fn compose_applies_catalogs_in_bundled_order_and_appends_new_providers() {
    let builtin = vec![model("a", "a1"), model("b", "b1"), model("a", "a2"), model("c", "c1")];
    let mut replaced = model("a", "a1");
    replaced.name = "A1 remote".into();
    let catalogs: Vec<std::sync::Arc<dyn DynamicCatalog>> = vec![
        std::sync::Arc::new(FixedCatalog { id: "a", models: vec![replaced, model("a", "a3")] }),
        std::sync::Arc::new(FixedCatalog { id: "llama.cpp", models: vec![model("llama.cpp", "local")] }),
    ];
    let composed = compose_catalog(&builtin, &catalogs);
    assert_eq!(ids(&composed), ["a1", "a2", "a3", "b1", "c1", "local"]);
    assert_eq!(composed[0].name, "A1 remote");
}

#[test]
fn registry_applies_models_json_over_the_composed_catalog() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("models.json");
    std::fs::write(&path, r#"{"providers": {"a": {"modelOverrides": {"a1": {"name": "Renamed"}}}}}"#).unwrap();
    let catalogs: Vec<std::sync::Arc<dyn DynamicCatalog>> =
        vec![std::sync::Arc::new(FixedCatalog { id: "a", models: vec![model("a", "a9")] })];
    let (registry, error) = build_registry(&[model("a", "a1")], &catalogs, Some(&path));
    assert!(error.is_none(), "{error:?}");
    assert_eq!(registry.find("a", "a1").unwrap().name, "Renamed");
    assert!(registry.find("a", "a9").is_some());

    std::fs::write(&path, "{ not json").unwrap();
    let (registry, error) = build_registry(&[model("a", "a1")], &catalogs, Some(&path));
    assert!(matches!(error, Some(pi_ai::models::ModelsFileError::Syntax { .. })));
    assert_eq!(registry.all_models().len(), 2);
}

#[test]
fn builtin_catalogs_cover_bundled_radius_custom_radius_and_llama() {
    let builtin = vec![model("openai", "gpt"), model("radius", "r1"), model("work", "w1")];
    let config = CatalogConfig {
        models_json: Some(
            r#"{
                // a Radius gateway of the company
                "providers": {"work": {"oauth": "radius", "baseUrl": "https://gw.example/v1/", "name": "Work"},
                              "plain": {"baseUrl": "https://x/v1"}}
            }"#
            .into(),
        ),
        ..CatalogConfig::default()
    };
    let catalogs = builtin_catalogs(&builtin, &config);
    let providers: Vec<&str> = catalogs.iter().map(|catalog| catalog.provider_id()).collect();
    assert_eq!(providers, ["openai", "radius", "work", "llama.cpp"]);
    let radius = radius_providers_in_models_json(config.models_json.as_deref().unwrap());
    assert_eq!(
        radius,
        [RadiusProviderConfig { id: "work".into(), name: "Work".into(), gateway: "https://gw.example".into() }]
    );
}

#[test]
fn update_outcomes_match_upstream_messages() {
    let mut result = RefreshResult::default();
    assert_eq!(update_models_outcome(&result).unwrap(), "Model catalogs refreshed");
    result.errors.insert("a".into(), "boom".into());
    result.errors.insert("b".into(), "bad".into());
    assert_eq!(update_models_outcome(&result).unwrap_err(), "Could not refresh model catalogs: a: boom; b: bad");
    result.aborted = true;
    assert_eq!(update_models_outcome(&result).unwrap_err(), "Model catalog refresh timed out.");
}

#[test]
fn radius_baseline_uses_bundled_models_for_the_default_gateway_only() {
    let builtin_count = pi_ai::models::builtin_models().iter().filter(|model| model.provider == "radius").count();
    let radius = RadiusCatalog::new("radius", None);
    assert_eq!(radius.models().len(), builtin_count);
    let custom = RadiusCatalog::new("team", Some("https://radius.pi.dev/"));
    assert!(custom.models().iter().all(|model| model.provider == "team"));
    assert!(RadiusCatalog::new("work", Some("gw.example")).models().is_empty());
    assert_eq!(RadiusCatalog::new("work", Some("gw.example")).gateway(), "https://gw.example");
}
