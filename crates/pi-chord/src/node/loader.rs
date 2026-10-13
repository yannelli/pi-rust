//! Facet bundle loading (upstream `node/bundle-loader.ts`): check, evaluate, and validate entries.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;

use super::manifest::{
    FACET_BUNDLE_MANIFEST_FILE, FacetBundleArtifact, FacetBundleManifest, resolve_bundle_file, verify_source,
};
use crate::error::{ChordError, ChordResult};
use crate::facets::{Facet, FacetLoader, LoadedFacets};
use crate::json::Json;

/// Maps an external import to a host target; `None` falls back to the evaluator's default.
pub type FacetBundleExternalResolver = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// One integrity-checked CommonJS entry, ready to evaluate.
pub struct FacetModule {
    pub source: String,
    pub module_path: PathBuf,
    pub external_imports: Vec<String>,
    pub resolve_external: Option<FacetBundleExternalResolver>,
}

/// One candidate from a module's default export.
pub enum ExportedFacet {
    Facet(Arc<dyn Facet>),
    /// Not an object with a non-empty string `id`.
    InvalidId,
    /// An object with an `id` but no `setup` function.
    MissingSetup(String),
}

/// What evaluating a module produced: its default export as facet candidates.
pub enum ModuleExports {
    NotAModule,
    Facets(Vec<ExportedFacet>),
}

/// Evaluates bundled CommonJS. Node uses `vm`; the `js` feature provides a QuickJS evaluator.
#[async_trait]
pub trait FacetModuleEvaluator: Send + Sync {
    async fn evaluate(&self, module: FacetModule) -> ChordResult<ModuleExports>;
}

#[derive(Clone)]
pub struct FacetBundleLoaderOptions {
    pub manifest_path: PathBuf,
    pub entry: String,
    /// Check the entry's SHA-256 integrity before evaluating it (default true).
    pub verify_integrity: bool,
    pub resolve_external: Option<FacetBundleExternalResolver>,
    pub evaluator: Arc<dyn FacetModuleEvaluator>,
}

impl FacetBundleLoaderOptions {
    pub fn new(manifest_path: impl Into<PathBuf>, entry: &str, evaluator: Arc<dyn FacetModuleEvaluator>) -> Self {
        let manifest_path = manifest_path.into();
        Self { manifest_path, entry: entry.to_string(), verify_integrity: true, resolve_external: None, evaluator }
    }
}

#[derive(Clone)]
pub struct FacetBundleArtifactLoaderOptions {
    /// An artifact value as received, for example from the wire.
    pub artifact: Json,
    pub resolve_external: Option<FacetBundleExternalResolver>,
    /// Parent of materialized generations; defaults to the system temp directory.
    pub temporary_directory: Option<PathBuf>,
    pub evaluator: Arc<dyn FacetModuleEvaluator>,
}

fn absolute(path: &Path) -> PathBuf {
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Read and check a versioned facet bundle manifest.
pub async fn read_facet_bundle_manifest(path: impl AsRef<Path>) -> ChordResult<FacetBundleManifest> {
    let path = absolute(path.as_ref());
    let shown = path.display().to_string();
    let read_error = || ChordError::error(format!("Could not read facet bundle manifest {shown}"));
    let text = tokio::fs::read_to_string(&path).await.map_err(|_| read_error())?;
    let value = Json::parse(&text).map_err(|_| read_error())?;
    FacetBundleManifest::parse(&value, &shown)
}

/// Read and check one transportable entry from a facet bundle on disk.
pub async fn read_facet_bundle_artifact(
    manifest_path: impl AsRef<Path>,
    entry: &str,
) -> ChordResult<FacetBundleArtifact> {
    if entry.is_empty() {
        return Err(ChordError::type_error("Facet bundle entry name must not be empty"));
    }
    let manifest_path = absolute(manifest_path.as_ref());
    let manifest = read_facet_bundle_manifest(&manifest_path).await?;
    let found =
        manifest.entries.get(entry).cloned().ok_or_else(|| {
            ChordError::error(format!("Facet bundle {} has no entry named {entry}", manifest.plugin.id))
        })?;
    let module_path = resolve_bundle_file(&manifest_path, &found.file, "entry")?;
    let source = tokio::fs::read_to_string(&module_path).await.map_err(ChordError::other)?;
    verify_source(&source, &found)?;
    let source_map_contents = match &found.source_map {
        None => None,
        Some(file) => {
            let path = resolve_bundle_file(&manifest_path, file, "source map")?;
            Some(tokio::fs::read_to_string(path).await.map_err(ChordError::other)?)
        }
    };
    Ok(FacetBundleArtifact {
        plugin: manifest.plugin,
        entry_name: entry.to_string(),
        entry: found,
        source,
        source_map_contents,
    })
}

const NODE_BUILTINS: &[&str] = &[
    "assert",
    "assert/strict",
    "async_hooks",
    "buffer",
    "child_process",
    "cluster",
    "console",
    "constants",
    "crypto",
    "dgram",
    "diagnostics_channel",
    "dns",
    "dns/promises",
    "domain",
    "events",
    "fs",
    "fs/promises",
    "http",
    "http2",
    "https",
    "inspector",
    "module",
    "net",
    "os",
    "path",
    "path/posix",
    "path/win32",
    "perf_hooks",
    "process",
    "punycode",
    "querystring",
    "readline",
    "readline/promises",
    "repl",
    "stream",
    "stream/consumers",
    "stream/promises",
    "stream/web",
    "string_decoder",
    "sys",
    "timers",
    "timers/promises",
    "tls",
    "trace_events",
    "tty",
    "url",
    "util",
    "util/types",
    "v8",
    "vm",
    "wasi",
    "worker_threads",
    "zlib",
];

/// Whether Node treats `specifier` as a built-in module.
pub(crate) fn is_builtin(specifier: &str) -> bool {
    specifier.starts_with("node:") || NODE_BUILTINS.contains(&specifier)
}

fn validate_package_specifier(specifier: &str) -> ChordResult<()> {
    let segments: Vec<&str> = specifier.split('/').collect();
    let package_parts = if specifier.starts_with('@') { 2 } else { 1 };
    let invalid = specifier.starts_with(['.', '/', '#'])
        || specifier.contains([':', '\\'])
        || segments.len() < package_parts
        || segments.iter().any(|part| part.is_empty() || *part == "." || *part == "..");
    if invalid {
        return Err(ChordError::error(format!(
            "Facet bundle artifact has an unsupported external import: {specifier}"
        )));
    }
    Ok(())
}

fn facets_from_module(exports: ModuleExports, plugin: &str, entry: &str) -> ChordResult<Vec<Arc<dyn Facet>>> {
    let ModuleExports::Facets(candidates) = exports else {
        return Err(ChordError::error(format!("Facet bundle entry {plugin}/{entry} did not export a module")));
    };
    if candidates.is_empty() {
        return Err(ChordError::error(format!("Facet bundle entry {plugin}/{entry} exported no facets")));
    }
    let mut facets: Vec<Arc<dyn Facet>> = Vec::new();
    for candidate in candidates {
        match candidate {
            ExportedFacet::Facet(facet) => facets.push(facet),
            ExportedFacet::InvalidId => {
                let message = format!("Facet bundle entry {plugin}/{entry} has a facet with an invalid ID");
                return Err(ChordError::error(message));
            }
            ExportedFacet::MissingSetup(id) => {
                let message = format!("Facet bundle entry {plugin}/{entry} facet {id} has no setup function");
                return Err(ChordError::error(message));
            }
        }
    }
    let mut seen = std::collections::HashSet::new();
    if !facets.iter().all(|facet| seen.insert(facet.id().to_string())) {
        return Err(ChordError::error(format!("Facet bundle entry {plugin}/{entry} exports duplicate facet IDs")));
    }
    Ok(facets)
}

struct BundleLoader {
    options: FacetBundleLoaderOptions,
}

impl BundleLoader {
    async fn evaluate(&self, manifest: &FacetBundleManifest, module_path: PathBuf) -> ChordResult<Vec<Arc<dyn Facet>>> {
        let entry = &manifest.entries[&self.options.entry];
        let source = tokio::fs::read_to_string(&module_path).await.map_err(ChordError::other)?;
        if self.options.verify_integrity {
            verify_source(&source, entry)?;
        }
        for specifier in &entry.external_imports {
            if !is_builtin(specifier) {
                validate_package_specifier(specifier)?;
            }
        }
        let module = FacetModule {
            source,
            module_path,
            external_imports: entry.external_imports.clone(),
            resolve_external: self.options.resolve_external.clone(),
        };
        let exports = self.options.evaluator.evaluate(module).await?;
        facets_from_module(exports, &manifest.plugin.id, &self.options.entry)
    }
}

#[async_trait]
impl FacetLoader for BundleLoader {
    async fn load(&self) -> ChordResult<LoadedFacets> {
        let manifest_path = absolute(&self.options.manifest_path);
        let manifest = read_facet_bundle_manifest(&manifest_path).await?;
        let entry = &self.options.entry;
        let Some(found) = manifest.entries.get(entry) else {
            return Err(ChordError::error(format!("Facet bundle {} has no entry named {entry}", manifest.plugin.id)));
        };
        let module_path = resolve_bundle_file(&manifest_path, &found.file, "entry")?;
        match self.evaluate(&manifest, module_path).await {
            Ok(facets) => Ok(LoadedFacets::fixed(facets)),
            Err(error) => {
                let plugin = &manifest.plugin.id;
                Err(ChordError::error(format!("Could not load facet bundle entry {plugin}/{entry}: {error}")))
            }
        }
    }
}

/// A reusable loader for one entry of a facet bundle manifest; each load evaluates a fresh generation.
pub fn create_facet_bundle_loader(options: FacetBundleLoaderOptions) -> ChordResult<Arc<dyn FacetLoader>> {
    if options.entry.is_empty() {
        return Err(ChordError::type_error("Facet bundle entry name must not be empty"));
    }
    Ok(Arc::new(BundleLoader { options }))
}

struct ArtifactLoader {
    artifact: FacetBundleArtifact,
    parent: PathBuf,
    resolve_external: Option<FacetBundleExternalResolver>,
    evaluator: Arc<dyn FacetModuleEvaluator>,
}

async fn make_temporary_directory(parent: &Path) -> ChordResult<PathBuf> {
    use ring::rand::SecureRandom;
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    tokio::fs::create_dir_all(parent).await.map_err(ChordError::other)?;
    let random = ring::rand::SystemRandom::new();
    loop {
        let mut bytes = [0u8; 6];
        random.fill(&mut bytes).map_err(|_| ChordError::error("Could not generate a temporary directory name"))?;
        let suffix: String = bytes.iter().map(|byte| ALPHABET[*byte as usize % ALPHABET.len()] as char).collect();
        let path = parent.join(format!("chord-facet-{suffix}"));
        match tokio::fs::create_dir(&path).await {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(ChordError::other(error)),
        }
    }
}

async fn remove_directory(path: &Path) -> ChordResult<()> {
    match tokio::fs::remove_dir_all(path).await {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(ChordError::other(error)),
        _ => Ok(()),
    }
}

impl ArtifactLoader {
    async fn materialize(&self, directory: &Path) -> ChordResult<()> {
        let artifact = &self.artifact;
        let mut entries = indexmap::IndexMap::new();
        entries.insert(artifact.entry_name.clone(), artifact.entry.clone());
        let manifest = FacetBundleManifest { plugin: artifact.plugin.clone(), entries };
        let write = |name: &str, text: &str| {
            let path = directory.join(name);
            let text = text.to_string();
            async move { tokio::fs::write(path, text).await.map_err(ChordError::other) }
        };
        write(&artifact.entry.file, &artifact.source).await?;
        write(FACET_BUNDLE_MANIFEST_FILE, &manifest.to_file_text()).await?;
        if let (Some(file), Some(contents)) = (&artifact.entry.source_map, &artifact.source_map_contents) {
            write(file, contents).await?;
        }
        Ok(())
    }

    async fn load_from(&self, directory: &Path) -> ChordResult<LoadedFacets> {
        self.materialize(directory).await?;
        let mut options = FacetBundleLoaderOptions::new(
            directory.join(FACET_BUNDLE_MANIFEST_FILE),
            &self.artifact.entry_name,
            self.evaluator.clone(),
        );
        options.resolve_external = self.resolve_external.clone();
        create_facet_bundle_loader(options)?.load().await
    }
}

#[async_trait]
impl FacetLoader for ArtifactLoader {
    async fn load(&self) -> ChordResult<LoadedFacets> {
        let directory = make_temporary_directory(&self.parent).await?;
        match self.load_from(&directory).await {
            Ok(loaded) => {
                let facets = loaded.facets.clone();
                Ok(LoadedFacets::new(facets, move || async move {
                    let results = [loaded.dispose().await, remove_directory(&directory).await];
                    let errors: Vec<ChordError> = results.into_iter().filter_map(Result::err).collect();
                    crate::facets::single_or_aggregate(errors, "Failed to dispose facet bundle artifact")
                }))
            }
            Err(error) => match remove_directory(&directory).await {
                Ok(()) => Err(error),
                Err(cleanup) => Err(ChordError::Aggregate {
                    message: "Facet bundle artifact loading and cleanup failed".into(),
                    errors: vec![error, cleanup],
                }),
            },
        }
    }
}

/// Check a transported artifact and materialize a fresh temporary generation for each load.
pub fn create_facet_bundle_artifact_loader(
    options: FacetBundleArtifactLoaderOptions,
) -> ChordResult<Arc<dyn FacetLoader>> {
    let artifact = FacetBundleArtifact::parse(&options.artifact)?;
    let parent = absolute(&options.temporary_directory.unwrap_or_else(std::env::temp_dir));
    Ok(Arc::new(ArtifactLoader {
        artifact,
        parent,
        resolve_external: options.resolve_external,
        evaluator: options.evaluator,
    }))
}
