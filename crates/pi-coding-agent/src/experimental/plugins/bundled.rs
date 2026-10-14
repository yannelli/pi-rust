//! Facet loaders for server-built plugin bundles (upstream `plugins/bundled.ts`). Bundles run in the embedded
//! QuickJS runtime, where the plugin API module resolves to the service tokens of `plugin.rs`.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use pi_chord::facets::{FacetLoader, LoadedFacets, combine_facet_loaders};
use pi_chord::node::{
    FacetBundleArtifact, FacetBundleArtifactLoaderOptions, FacetBundleExternalResolver, FacetBundleLoaderOptions,
    FacetModuleEvaluator, QuickJsFacetEvaluator, create_facet_bundle_artifact_loader, create_facet_bundle_loader,
    read_facet_bundle_manifest,
};
use pi_chord::{ChordError, ChordResult, Json, JsonMap};
use tokio::sync::OnceCell;

use crate::experimental::plugin::{PI_PLUGIN_API, plugin_api_exports};

const PRESENTATION_FACET_BUNDLES_KEY: &str = "presentationFacetBundles";

/// A QuickJS evaluator with the plugin API module registered.
pub async fn create_plugin_evaluator() -> ChordResult<Arc<dyn FacetModuleEvaluator>> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
    let evaluator = QuickJsFacetEvaluator::new(cwd).await?;
    evaluator.handle().register_virtual_module(PI_PLUGIN_API, pi_js::VirtualModule::Json(plugin_api_exports()));
    Ok(evaluator)
}

fn resolve_plugin_external() -> FacetBundleExternalResolver {
    Arc::new(|specifier: &str| (specifier == PI_PLUGIN_API).then(|| PI_PLUGIN_API.to_string()))
}

/// Loads `entry` from a manifest, or nothing when the manifest has no such entry.
struct OptionalBundleLoader {
    manifest_path: PathBuf,
    entry: &'static str,
    inner: OnceCell<Arc<dyn FacetLoader>>,
}

#[async_trait]
impl FacetLoader for OptionalBundleLoader {
    async fn load(&self) -> ChordResult<LoadedFacets> {
        let manifest = read_facet_bundle_manifest(&self.manifest_path).await?;
        if !manifest.entries.contains_key(self.entry) {
            return Ok(LoadedFacets::fixed(Vec::new()));
        }
        let loader = self
            .inner
            .get_or_try_init(|| async {
                let mut options =
                    FacetBundleLoaderOptions::new(&self.manifest_path, self.entry, create_plugin_evaluator().await?);
                options.resolve_external = Some(resolve_plugin_external());
                create_facet_bundle_loader(options)
            })
            .await?;
        loader.load().await
    }
}

/// The Session entries of the selected plugin manifests, or `None` without plugins.
pub fn create_session_plugin_facet_loader(manifest_paths: &[String]) -> Option<Arc<dyn FacetLoader>> {
    if manifest_paths.is_empty() {
        return None;
    }
    let loaders = manifest_paths
        .iter()
        .map(|path| {
            let loader =
                OptionalBundleLoader { manifest_path: PathBuf::from(path), entry: "session", inner: OnceCell::new() };
            Arc::new(loader) as Arc<dyn FacetLoader>
        })
        .collect();
    Some(combine_facet_loaders(loaders))
}

pub fn create_presentation_facet_data(artifacts: &[FacetBundleArtifact]) -> Json {
    let mut map = JsonMap::new();
    map.insert(
        PRESENTATION_FACET_BUNDLES_KEY,
        Json::array(artifacts.iter().map(FacetBundleArtifact::to_json).collect()),
    );
    Json::object(map)
}

/// Loads one transported artifact, creating its evaluator on first load.
struct ArtifactLoader {
    artifact: Json,
    inner: OnceCell<Arc<dyn FacetLoader>>,
}

#[async_trait]
impl FacetLoader for ArtifactLoader {
    async fn load(&self) -> ChordResult<LoadedFacets> {
        let loader = self
            .inner
            .get_or_try_init(|| async {
                create_facet_bundle_artifact_loader(FacetBundleArtifactLoaderOptions {
                    artifact: self.artifact.clone(),
                    resolve_external: Some(resolve_plugin_external()),
                    temporary_directory: None,
                    evaluator: create_plugin_evaluator().await?,
                })
            })
            .await?;
        loader.load().await
    }
}

/// Create local loaders only from artifacts selected and sent by the connected server.
pub fn create_presentation_facet_loaders(data: &Json) -> ChordResult<Vec<Arc<dyn FacetLoader>>> {
    let Some(object) = data.as_object() else { return Err(ChordError::error("Invalid presentation plugin data")) };
    let Some(artifacts) = object.get(PRESENTATION_FACET_BUNDLES_KEY) else { return Ok(Vec::new()) };
    let Some(artifacts) = artifacts.as_array() else {
        return Err(ChordError::error("Invalid presentation plugin bundle list"));
    };
    for artifact in artifacts {
        FacetBundleArtifact::parse(artifact)?;
    }
    let loader = |artifact: &Json| {
        Arc::new(ArtifactLoader { artifact: artifact.clone(), inner: OnceCell::new() }) as Arc<dyn FacetLoader>
    };
    Ok(artifacts.iter().map(loader).collect())
}
