//! Port of upstream `test/bundle.test.ts`: bundling with oxc and loading through QuickJS.
#![cfg(feature = "js")]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use indexmap::IndexMap;
use pi_chord::facets::{FacetOptions, create_facet_host, define_facet};
use pi_chord::node::{
    BundleFacetPackageOptions, BundleFacetsOptions, FacetBundleArtifactLoaderOptions, FacetBundleLoaderOptions,
    FacetBundlePlugin, FacetModuleEvaluator, JsObject, QuickJsFacetEvaluator, bundle_facet_package, bundle_facets,
    create_facet_bundle_artifact_loader, create_facet_bundle_loader, read_facet_bundle_artifact,
    read_facet_bundle_manifest,
};
use pi_chord::services::{LocalHandle, define_local_service};
use pi_chord::{ChordResult, Json};

fn plugin(id: &str, version: Option<&str>) -> FacetBundlePlugin {
    FacetBundlePlugin { id: id.into(), version: version.map(str::to_string) }
}

fn entries(items: &[(&str, &Path)]) -> IndexMap<String, PathBuf> {
    items.iter().map(|(name, path)| (name.to_string(), path.to_path_buf())).collect()
}

fn write(path: &Path, text: &str) {
    std::fs::write(path, text).unwrap();
}

fn write_generation(path: &Path, generation: &str) {
    let text = format!(
        "import \"@earendil-works/chord\";\n\
         import {{ decorate }} from \"./helper.ts\";\n\
         const Value = {{ id: \"test.bundle.generation\", local: true }};\n\
         export default {{ id: \"bundle-provider\", setup(env) {{\n\
         \x20 env.provide(Value, {{ read() {{ return decorate({generation:?}); }} }});\n\
         }}}};\n"
    );
    write(path, &text);
}

fn listing(directory: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

fn ids(facets: &[Arc<dyn pi_chord::facets::Facet>]) -> Vec<&str> {
    facets.iter().map(|facet| facet.id()).collect()
}

async fn evaluator(directory: &Path) -> Arc<dyn FacetModuleEvaluator> {
    QuickJsFacetEvaluator::new(directory).await.unwrap()
}

fn is_entry_file(file: &str) -> bool {
    let Some(rest) = file.strip_prefix("facet-").and_then(|rest| rest.strip_suffix(".cjs")) else { return false };
    let Some((hash, content)) = rest.split_once('-') else { return false };
    hash.len() == 12
        && hash.chars().all(|char| char.is_ascii_hexdigit() && !char.is_ascii_uppercase())
        && !content.is_empty()
        && content.chars().all(|char| char.is_ascii_uppercase() || char.is_ascii_digit())
}

async fn read(handle: &LocalHandle<JsObject>) -> ChordResult<Option<Json>> {
    handle.get()?.call("read", Vec::new()).await
}

#[tokio::test(flavor = "multi_thread")]
async fn builds_independent_content_addressed_entries_and_loads_fresh_reloadable_generations() {
    let temporary = tempfile::tempdir().unwrap();
    let directory = temporary.path();
    let source = directory.join("src");
    let output = directory.join("bundle");
    std::fs::create_dir(&source).unwrap();
    write(&source.join("helper.ts"), "export const decorate = (value: string): string => \"generation:\" + value;\n");
    let entry_path = source.join("entry.ts");
    let presentation_path = source.join("presentation.ts");
    write_generation(&entry_path, "A");
    write(&presentation_path, "export default { id: \"bundle-presentation\", setup() {} };\n");
    let facet_entries = entries(&[("presentation", &presentation_path), ("worker", &entry_path)]);
    let options = |version: &str| {
        let mut options =
            BundleFacetsOptions::new(plugin("test-bundle", Some(version)), facet_entries.clone(), &output);
        options.source_map = true;
        options
    };

    let first = bundle_facets(options("1")).await.unwrap();
    let first_entry = first.manifest.entries["worker"].clone();
    assert!(is_entry_file(&first_entry.file), "{}", first_entry.file);
    assert_eq!(first_entry.source_map.as_deref(), Some(format!("{}.map", first_entry.file).as_str()));
    assert_eq!(first_entry.external_imports, vec!["@earendil-works/chord".to_string()]);
    assert_ne!(first.manifest.entries["presentation"].file, first_entry.file);
    assert_eq!(listing(&output).iter().filter(|name| name.ends_with(".cjs")).count(), 2);
    let first_source = std::fs::read_to_string(output.join(&first_entry.file)).unwrap();
    assert!(first_source.contains("require(\"@earendil-works/chord\")"));
    assert!(first_source.contains("module.exports"));
    assert!(std::fs::read_to_string(&first.manifest_path).unwrap().ends_with('\n'));

    let evaluator = evaluator(directory).await;
    let presentation = create_facet_bundle_loader(FacetBundleLoaderOptions::new(
        &first.manifest_path,
        "presentation",
        evaluator.clone(),
    ))
    .unwrap()
    .load()
    .await
    .unwrap();
    assert_eq!(ids(&presentation.facets), ["bundle-presentation"]);
    presentation.dispose().await.unwrap();

    let artifact = read_facet_bundle_artifact(&first.manifest_path, "worker").await.unwrap();
    let materialized = directory.join("materialized");
    let transported = create_facet_bundle_artifact_loader(FacetBundleArtifactLoaderOptions {
        artifact: artifact.to_json(),
        resolve_external: None,
        temporary_directory: Some(materialized.clone()),
        evaluator: evaluator.clone(),
    })
    .unwrap()
    .load()
    .await
    .unwrap();
    assert_eq!(ids(&transported.facets), ["bundle-provider"]);
    assert_eq!(listing(&materialized).len(), 1);
    transported.dispose().await.unwrap();
    assert!(listing(&materialized).is_empty());

    let second = bundle_facets(options("1")).await.unwrap();
    assert_eq!(second.manifest.entries["worker"], first_entry);

    let loader =
        create_facet_bundle_loader(FacetBundleLoaderOptions::new(&second.manifest_path, "worker", evaluator.clone()))
            .unwrap();
    let loaded_a = loader.load().await.unwrap();
    let loaded_a_copy = loader.load().await.unwrap();
    assert!(!Arc::ptr_eq(&loaded_a_copy.facets[0], &loaded_a.facets[0]));
    loaded_a_copy.dispose().await.unwrap();

    let retained: Arc<Mutex<Option<LocalHandle<JsObject>>>> = Arc::default();
    let slot = retained.clone();
    let consumer = define_facet("bundle-consumer", move |env| {
        let service = define_local_service::<JsObject>("test.bundle.generation")?;
        *slot.lock().unwrap() = Some(env.use_local(&service)?);
        Ok(())
    });
    let mut facets = vec![consumer];
    facets.extend(loaded_a.facets.iter().cloned());
    let host = create_facet_host(FacetOptions::new(facets)).await.unwrap();
    let handle = retained.lock().unwrap().clone().unwrap();
    assert_eq!(read(&handle).await.unwrap(), Some(Json::from("generation:A")));

    write_generation(&entry_path, "B");
    let third = bundle_facets(options("2")).await.unwrap();
    assert_ne!(third.manifest.entries["worker"].file, first_entry.file);
    let loaded_b = loader.load().await.unwrap();
    host.reload(loaded_b.facets.clone()).await.unwrap();
    loaded_a.dispose().await.unwrap();
    assert_eq!(read(&handle).await.unwrap(), Some(Json::from("generation:B")));

    host.dispose().await.unwrap();
    loaded_b.dispose().await.unwrap();
}

fn file_url(path: &Path) -> String {
    let text = path.display().to_string().replace('\\', "/");
    if text.starts_with('/') { format!("file://{text}") } else { format!("file:///{text}") }
}

#[tokio::test(flavor = "multi_thread")]
async fn loads_host_externals_through_the_controlled_commonjs_require() {
    let temporary = tempfile::tempdir().unwrap();
    let directory = temporary.path();
    let external_path = directory.join("host.mjs");
    let entry_path = directory.join("entry.ts");
    let output = directory.join("bundle");
    write(&external_path, "export const named = \"host-named\";\n");
    write(
        &entry_path,
        "import { named } from \"@example/host\";\n\
         export const loadDynamic = () => import(\"@example/dynamic\");\n\
         if (named !== \"host-named\") throw new Error(\"external mismatch\");\n\
         export default { id: \"external-facet\", setup() {} };\n",
    );
    let mut options =
        BundleFacetsOptions::new(plugin("external-bundle", None), entries(&[("worker", &entry_path)]), &output);
    options.external = vec!["@example/host".into(), "@example/dynamic".into()];
    let result = bundle_facets(options).await.unwrap();
    let entry = &result.manifest.entries["worker"];
    let source = std::fs::read_to_string(output.join(&entry.file)).unwrap();
    assert!(!source.contains("import("));
    assert!(source.contains("require(\"@example/dynamic\")"));

    let url = file_url(&external_path);
    let mut loader = FacetBundleLoaderOptions::new(&result.manifest_path, "worker", evaluator(directory).await);
    loader.resolve_external = Some(Arc::new(move |specifier: &str| {
        matches!(specifier, "@example/host" | "@example/dynamic").then(|| url.clone())
    }));
    let loaded = create_facet_bundle_loader(loader).unwrap().load().await.unwrap();
    assert_eq!(ids(&loaded.facets), ["external-facet"]);
    loaded.dispose().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn builds_plugin_packages_from_conventional_and_configured_facet_entries() {
    let temporary = tempfile::tempdir().unwrap();
    let directory = std::fs::canonicalize(temporary.path()).unwrap();
    let source = directory.join("src");
    std::fs::create_dir(&source).unwrap();
    let package = serde_json::json!({
        "name": "@example/conventional-plugin",
        "version": "1.2.3",
        "peerDependencies": { "@example/host": "^1.0.0" },
    });
    write(&directory.join("package.json"), &format!("{package}\n"));
    write(
        &source.join("session.ts"),
        "import \"@example/host/plugin\"; export default { id: \"package-session\", setup() {} };\n",
    );
    write(&source.join("tui.ts"), "export default { id: \"package-tui\", setup() {} };\n");
    write(&source.join("contract.ts"), "export const ignored = true;\n");
    write(&source.join("presentation.ts"), "export default { id: \"configured-tui\", setup() {} };\n");

    let defaults = |items: &[(&str, &str)]| -> IndexMap<String, String> {
        items.iter().map(|(name, path)| (name.to_string(), path.to_string())).collect()
    };
    let conventional = bundle_facet_package(BundleFacetPackageOptions {
        package_path: directory.clone(),
        outdir: directory.join("build"),
        default_facets: defaults(&[
            ("session", "src/session.ts"),
            ("tui", "src/tui.ts"),
            ("browser", "src/browser.ts"),
        ]),
    })
    .await
    .unwrap();
    assert_eq!(conventional.package_directory, directory);
    assert_eq!(conventional.manifest.plugin, plugin("@example/conventional-plugin", Some("1.2.3")));
    assert_eq!(conventional.manifest.entries.keys().collect::<Vec<_>>(), ["session", "tui"]);
    assert_eq!(conventional.manifest.entries["session"].external_imports, vec!["@example/host/plugin".to_string()]);
    assert!(conventional.manifest.entries["tui"].source_map.as_deref().unwrap().ends_with(".cjs.map"));

    let configured_package = serde_json::json!({
        "name": "@example/conventional-plugin",
        "version": "2.0.0",
        "chord": { "facets": { "session": false, "tui": "src/presentation.ts" }, "sourceMap": false },
    });
    write(&directory.join("package.json"), &format!("{configured_package}\n"));
    let configured = bundle_facet_package(BundleFacetPackageOptions {
        package_path: directory.join("package.json"),
        outdir: directory.join("build"),
        default_facets: defaults(&[("session", "src/session.ts"), ("tui", "src/tui.ts")]),
    })
    .await
    .unwrap();
    assert_eq!(configured.manifest.entries.keys().collect::<Vec<_>>(), ["tui"]);
    assert_eq!(configured.manifest.entries["tui"].source_map, None);
    let loader = FacetBundleLoaderOptions::new(&configured.manifest_path, "tui", evaluator(&directory).await);
    let loaded = create_facet_bundle_loader(loader).unwrap().load().await.unwrap();
    assert_eq!(ids(&loaded.facets), ["configured-tui"]);
    loaded.dispose().await.unwrap();
}

#[tokio::test]
async fn rejects_invalid_plugin_package_entry_configuration() {
    let temporary = tempfile::tempdir().unwrap();
    let directory = temporary.path();
    let package = serde_json::json!({
        "name": "invalid-plugin",
        "version": "1.0.0",
        "chord": { "facets": { "tui": "../outside.ts" } },
    });
    write(&directory.join("package.json"), &format!("{package}\n"));
    let error = bundle_facet_package(BundleFacetPackageOptions {
        package_path: directory.to_path_buf(),
        outdir: directory.join("build"),
        default_facets: IndexMap::new(),
    })
    .await
    .unwrap_err();
    assert!(error.to_string().contains("escapes the package directory"), "{error}");
}

#[tokio::test(flavor = "multi_thread")]
async fn rejects_corrupt_entries_and_invalid_module_exports() {
    let temporary = tempfile::tempdir().unwrap();
    let directory = temporary.path();
    let entry_path = directory.join("entry.ts");
    let output = directory.join("bundle");
    write(&entry_path, "export default { id: 'missing-setup' };\n");
    let options =
        BundleFacetsOptions::new(plugin("invalid-bundle", None), entries(&[("invalid", &entry_path)]), &output);
    let result = bundle_facets(options).await.unwrap();
    let loader = create_facet_bundle_loader(FacetBundleLoaderOptions::new(
        &result.manifest_path,
        "invalid",
        evaluator(directory).await,
    ))
    .unwrap();
    let error = loader.load().await.err().unwrap();
    assert!(error.to_string().contains("has no setup function"), "{error}");

    let manifest = read_facet_bundle_manifest(&result.manifest_path).await.unwrap();
    write(&output.join(&manifest.entries["invalid"].file), "export default {};\n");
    let error = loader.load().await.err().unwrap();
    assert!(error.to_string().contains("integrity check failed"), "{error}");
}
