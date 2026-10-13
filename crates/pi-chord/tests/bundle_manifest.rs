//! Manifests, artifacts, and bundle loaders with a stand-in evaluator (no `js` feature).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use indexmap::IndexMap;
use pi_chord::facets::define_facet;
use pi_chord::node::{
    ExportedFacet, FacetBundleArtifactLoaderOptions, FacetBundleEntry, FacetBundleLoaderOptions, FacetBundleManifest,
    FacetBundlePlugin, FacetModule, FacetModuleEvaluator, ModuleExports, create_facet_bundle_artifact_loader,
    create_facet_bundle_loader, integrity_of, read_facet_bundle_artifact, read_facet_bundle_manifest,
};
use pi_chord::{ChordResult, Json, json};

/// The module path, external imports, and resolved `@example/host` target of one evaluation.
type Seen = (PathBuf, Vec<String>, Option<String>);

/// Reads facet IDs from a `facets:` line; `none` exports nothing and `!id` lacks setup.
#[derive(Default)]
struct FakeEvaluator {
    seen: Mutex<Vec<Seen>>,
}

#[async_trait]
impl FacetModuleEvaluator for FakeEvaluator {
    async fn evaluate(&self, module: FacetModule) -> ChordResult<ModuleExports> {
        let resolved = module.resolve_external.as_ref().and_then(|resolve| resolve("@example/host"));
        self.seen.lock().unwrap().push((module.module_path.clone(), module.external_imports.clone(), resolved));
        let Some(list) = module.source.trim().strip_prefix("facets:") else { return Ok(ModuleExports::NotAModule) };
        let candidates = list
            .split(',')
            .map(str::trim)
            .filter(|item| *item != "none")
            .map(|item| match item {
                "" => ExportedFacet::InvalidId,
                id if id.starts_with('!') => ExportedFacet::MissingSetup(id[1..].to_string()),
                id => ExportedFacet::Facet(define_facet(id, |_| Ok(()))),
            })
            .collect();
        Ok(ModuleExports::Facets(candidates))
    }
}

fn write_bundle(directory: &Path, source: &str, source_map: Option<&str>) -> PathBuf {
    let file = "facet-000000000000-AAAAAAAA.cjs".to_string();
    std::fs::write(directory.join(&file), source).unwrap();
    if let Some(contents) = source_map {
        std::fs::write(directory.join(format!("{file}.map")), contents).unwrap();
    }
    let entry = FacetBundleEntry {
        integrity: integrity_of(source.as_bytes()),
        source_map: source_map.map(|_| format!("{file}.map")),
        external_imports: vec!["@example/host".into()],
        file,
    };
    let plugin = FacetBundlePlugin { id: "test-plugin".into(), version: Some("1".into()) };
    let manifest = FacetBundleManifest { plugin, entries: IndexMap::from([("worker".to_string(), entry)]) };
    let path = directory.join("chord-facets.json");
    std::fs::write(&path, manifest.to_file_text()).unwrap();
    path
}

#[test]
fn manifests_serialize_like_json_stringify_with_two_space_indent() {
    let entry = FacetBundleEntry {
        file: "facet-a.cjs".into(),
        integrity: integrity_of(b"x"),
        external_imports: vec!["@earendil-works/chord".into()],
        source_map: None,
    };
    let plugin = FacetBundlePlugin { id: "p".into(), version: None };
    let manifest = FacetBundleManifest { plugin, entries: IndexMap::from([("worker".to_string(), entry)]) };
    let expected = "{\n  \"format\": \"chord.facet-bundle\",\n  \"formatVersion\": 2,\n  \"plugin\": {\n    \"id\": \"p\"\n  },\n  \
        \"entries\": {\n    \"worker\": {\n      \"file\": \"facet-a.cjs\",\n      \"integrity\": \"sha256-LXEWQrcmsEQBYnyp+6wy9chTD7GQPMTbAiWHF5IaSIE=\",\n      \
        \"externalImports\": [\n        \"@earendil-works/chord\"\n      ]\n    }\n  }\n}\n";
    assert_eq!(manifest.to_file_text(), expected);
    assert_eq!(FacetBundleManifest::parse(&manifest.to_json(), "m").unwrap(), manifest);
}

#[test]
fn manifests_reject_invalid_shapes() {
    let message = |value: Json| FacetBundleManifest::parse(&value, "m.json").unwrap_err().to_string();
    assert_eq!(message(json!({"format": "other"})), "Invalid facet bundle manifest format in m.json");
    let base = json!({"format": "chord.facet-bundle", "formatVersion": 1});
    assert_eq!(message(base), "Unsupported facet bundle manifest version in m.json: 1");
    let empty = json!({"format": "chord.facet-bundle", "formatVersion": 2, "plugin": {"id": "p"}, "entries": {}});
    assert_eq!(message(empty), "Facet bundle manifest has no entries in m.json");
}

#[tokio::test]
async fn loads_entries_and_reports_module_shape_failures() {
    let temporary = tempfile::tempdir().unwrap();
    let path = write_bundle(temporary.path(), "facets: one, two", None);
    let evaluator = Arc::new(FakeEvaluator::default());
    let mut options = FacetBundleLoaderOptions::new(&path, "worker", evaluator.clone());
    options.resolve_external = Some(Arc::new(|specifier: &str| Some(format!("/host/{specifier}"))));
    let loader = create_facet_bundle_loader(options.clone()).unwrap();
    let loaded = loader.load().await.unwrap();
    assert_eq!(loaded.facets.iter().map(|facet| facet.id()).collect::<Vec<_>>(), ["one", "two"]);
    let seen = evaluator.seen.lock().unwrap()[0].clone();
    assert_eq!(seen.0, temporary.path().join("facet-000000000000-AAAAAAAA.cjs"));
    assert_eq!(seen.1, ["@example/host"]);
    assert_eq!(seen.2.as_deref(), Some("/host/@example/host"));
    loaded.dispose().await.unwrap();

    let failure = |source: &'static str| {
        let (directory, options) = (temporary.path().to_path_buf(), options.clone());
        async move {
            write_bundle(&directory, source, None);
            let error = create_facet_bundle_loader(options).unwrap().load().await.err().unwrap();
            error.to_string()
        }
    };
    let prefix = "Could not load facet bundle entry test-plugin/worker: Facet bundle entry test-plugin/worker";
    assert_eq!(failure("module.exports = 1").await, format!("{prefix} did not export a module"));
    assert_eq!(failure("facets: none").await, format!("{prefix} exported no facets"));
    assert_eq!(failure("facets: one,").await, format!("{prefix} has a facet with an invalid ID"));
    assert_eq!(failure("facets: !lonely").await, format!("{prefix} facet lonely has no setup function"));
    assert_eq!(failure("facets: one, one").await, format!("{prefix} exports duplicate facet IDs"));

    std::fs::write(temporary.path().join("facet-000000000000-AAAAAAAA.cjs"), "facets: tampered").unwrap();
    let error = create_facet_bundle_loader(options.clone()).unwrap().load().await.err().unwrap();
    assert!(error.to_string().contains("integrity check failed"), "{error}");
    options.verify_integrity = false;
    let loaded = create_facet_bundle_loader(options).unwrap().load().await.unwrap();
    assert_eq!(loaded.facets[0].id(), "tampered");

    let missing = FacetBundleLoaderOptions::new(&path, "absent", evaluator.clone());
    let error = create_facet_bundle_loader(missing).unwrap().load().await.err().unwrap();
    assert_eq!(error.to_string(), "Facet bundle test-plugin has no entry named absent");
    let empty = FacetBundleLoaderOptions::new(&path, "", evaluator);
    assert!(create_facet_bundle_loader(empty).is_err());
}

#[tokio::test]
async fn artifacts_round_trip_and_materialize_fresh_generations() {
    let temporary = tempfile::tempdir().unwrap();
    let path = write_bundle(temporary.path(), "facets: shipped", Some("{\"version\":3}"));
    let manifest = read_facet_bundle_manifest(&path).await.unwrap();
    assert_eq!(manifest.plugin.id, "test-plugin");
    let artifact = read_facet_bundle_artifact(&path, "worker").await.unwrap();
    assert_eq!(artifact.source_map_contents.as_deref(), Some("{\"version\":3}"));

    let materialized = temporary.path().join("materialized");
    let evaluator = Arc::new(FakeEvaluator::default());
    let loader = create_facet_bundle_artifact_loader(FacetBundleArtifactLoaderOptions {
        artifact: artifact.to_json(),
        resolve_external: None,
        temporary_directory: Some(materialized.clone()),
        evaluator: evaluator.clone(),
    })
    .unwrap();
    let first = loader.load().await.unwrap();
    let second = loader.load().await.unwrap();
    assert_eq!(first.facets[0].id(), "shipped");
    assert_eq!(std::fs::read_dir(&materialized).unwrap().count(), 2);
    first.dispose().await.unwrap();
    second.dispose().await.unwrap();
    assert_eq!(std::fs::read_dir(&materialized).unwrap().count(), 0);

    let mut tampered = artifact.to_json();
    tampered.set("source", "facets: other").unwrap();
    let options = FacetBundleArtifactLoaderOptions {
        artifact: tampered,
        resolve_external: None,
        temporary_directory: Some(materialized),
        evaluator,
    };
    let error = create_facet_bundle_artifact_loader(options).err().unwrap();
    assert!(error.to_string().contains("integrity check failed"), "{error}");
    let error = create_facet_bundle_artifact_loader(FacetBundleArtifactLoaderOptions {
        artifact: json!({"format": "chord.facet-bundle-artifact"}),
        resolve_external: None,
        temporary_directory: None,
        evaluator: Arc::new(FakeEvaluator::default()),
    })
    .err()
    .unwrap();
    assert_eq!(error.to_string(), "Invalid facet bundle artifact");
}
