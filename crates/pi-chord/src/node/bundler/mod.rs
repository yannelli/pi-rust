//! The facet bundler (upstream `node/bundle.ts`): oxc in place of esbuild, same output contract.

mod graph;
mod rewrite;
mod sourcemap;

use std::path::{Path, PathBuf};

use indexmap::IndexMap;

pub use graph::FacetBundlePlatform;
use graph::{GraphOptions, bundle_entry};

use super::manifest::{
    FACET_BUNDLE_MANIFEST_FILE, FacetBundleEntry, FacetBundleManifest, FacetBundlePlugin, integrity_of,
};
use crate::error::{ChordError, ChordResult};

#[derive(Clone, Debug)]
pub struct BundleFacetsOptions {
    pub plugin: FacetBundlePlugin,
    /// Application-selected entry names mapped to TypeScript or JavaScript source files.
    pub entries: IndexMap<String, PathBuf>,
    pub outdir: PathBuf,
    pub working_directory: Option<PathBuf>,
    /// Package imports left for the loading application to resolve; `pkg/*` matches subpaths.
    pub external: Vec<String>,
    pub source_map: bool,
    /// Global identifiers or dotted paths replaced by the given JavaScript expression text.
    pub define: Vec<(String, String)>,
    pub platform: FacetBundlePlatform,
}

impl BundleFacetsOptions {
    pub fn new(plugin: FacetBundlePlugin, entries: IndexMap<String, PathBuf>, outdir: impl Into<PathBuf>) -> Self {
        Self {
            plugin,
            entries,
            outdir: outdir.into(),
            working_directory: None,
            external: Vec::new(),
            source_map: false,
            define: Vec::new(),
            platform: FacetBundlePlatform::Node,
        }
    }
}

#[derive(Clone, Debug)]
pub struct BundleFacetsResult {
    pub manifest: FacetBundleManifest,
    pub manifest_path: PathBuf,
}

fn validate(options: &BundleFacetsOptions) -> ChordResult<()> {
    if options.plugin.id.is_empty() {
        return Err(ChordError::type_error("Facet bundle plugin ID must not be empty"));
    }
    if options.plugin.version.as_deref() == Some("") {
        return Err(ChordError::type_error("Facet bundle plugin version must not be empty"));
    }
    if options.entries.is_empty() {
        return Err(ChordError::type_error("Facet bundle must contain at least one entry"));
    }
    for (name, source) in &options.entries {
        if name.is_empty() {
            return Err(ChordError::type_error("Facet bundle entry name must not be empty"));
        }
        if source.as_os_str().is_empty() {
            return Err(ChordError::type_error(format!("Facet bundle entry {name} must have a source path")));
        }
    }
    if options.external.iter().any(String::is_empty) {
        return Err(ChordError::type_error("Facet bundle external import must not be empty"));
    }
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn short_hash(value: &str) -> String {
    hex(ring::digest::digest(&ring::digest::SHA256, value.as_bytes()).as_ref())[..12].to_string()
}

/// Eight upper-case base32 characters of the content hash, like esbuild's `[hash]`.
fn content_hash(code: &str) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    let digest = ring::digest::digest(&ring::digest::SHA256, code.as_bytes());
    let bits = u64::from_be_bytes(digest.as_ref()[..8].try_into().unwrap_or_default());
    (0..8).map(|index| ALPHABET[((bits >> (59 - index * 5)) & 31) as usize] as char).collect()
}

fn random_id() -> ChordResult<String> {
    use ring::rand::SecureRandom;
    let mut bytes = [0u8; 16];
    ring::rand::SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| ChordError::error("Could not generate a random ID"))?;
    Ok(hex(&bytes))
}

fn io(error: std::io::Error) -> ChordError {
    ChordError::other(error)
}

fn bundle_one(
    name: &str,
    source: &Path,
    directory: &Path,
    out: &Path,
    options: &BundleFacetsOptions,
    cwd: &Path,
) -> ChordResult<FacetBundleEntry> {
    let mut external = vec!["@earendil-works/chord".to_string(), "@earendil-works/chord/*".to_string()];
    for item in &options.external {
        if !external.contains(item) {
            external.push(item.clone());
        }
    }
    let graph = GraphOptions {
        working_directory: cwd,
        external: &external,
        define: &options.define,
        platform: options.platform,
    };
    let output = bundle_entry(source, &graph)
        .map_err(|error| ChordError::error(format!("Could not bundle facet entry {name}\n{error}")))?;
    let file = format!("facet-{}-{}.cjs", short_hash(name), content_hash(&output.code));
    let source_map = options.source_map.then(|| format!("{file}.map"));
    let mut code = output.code.clone();
    if let Some(map_file) = &source_map {
        code.push_str(&format!("//# sourceMappingURL={map_file}\n"));
        std::fs::write(directory.join(map_file), output.source_map(out)).map_err(io)?;
    }
    std::fs::write(directory.join(&file), &code).map_err(io)?;
    Ok(FacetBundleEntry {
        integrity: integrity_of(code.as_bytes()),
        file,
        external_imports: output.external_imports,
        source_map,
    })
}

fn replace_directory(temporary: &Path, output: &Path) -> ChordResult<()> {
    let backup = PathBuf::from(format!("{}.old-{}", output.display(), random_id()?));
    let moved = match std::fs::rename(output, &backup) {
        Ok(()) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(io(error)),
    };
    if let Err(error) = std::fs::rename(temporary, output) {
        if moved {
            std::fs::rename(&backup, output).map_err(io)?;
        }
        return Err(io(error));
    }
    if moved {
        let _ = std::fs::remove_dir_all(&backup);
    }
    Ok(())
}

fn build(options: &BundleFacetsOptions) -> ChordResult<BundleFacetsResult> {
    validate(options)?;
    let cwd = match &options.working_directory {
        Some(directory) => std::path::absolute(directory).map_err(io)?,
        None => std::env::current_dir().map_err(io)?,
    };
    let output = std::path::absolute(cwd.join(&options.outdir)).map_err(io)?;
    let parent = output.parent().map(Path::to_path_buf).unwrap_or_else(|| cwd.clone());
    std::fs::create_dir_all(&parent).map_err(io)?;
    let base = output.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
    let temporary = parent.join(format!(".{base}.tmp-{}", random_id()?));
    std::fs::create_dir(&temporary).map_err(io)?;
    let result = (|| {
        let mut names: Vec<&String> = options.entries.keys().collect();
        names.sort();
        let mut entries = IndexMap::new();
        for name in names {
            let source = cwd.join(&options.entries[name]);
            entries.insert(name.clone(), bundle_one(name, &source, &temporary, &output, options, &cwd)?);
        }
        let manifest = FacetBundleManifest { plugin: options.plugin.clone(), entries };
        std::fs::write(temporary.join(FACET_BUNDLE_MANIFEST_FILE), manifest.to_file_text()).map_err(io)?;
        replace_directory(&temporary, &output)?;
        Ok(BundleFacetsResult { manifest, manifest_path: output.join(FACET_BUNDLE_MANIFEST_FILE) })
    })();
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&temporary);
    }
    result
}

/// Bundle each facet entry into an independent content-addressed CommonJS file plus a manifest.
pub async fn bundle_facets(options: BundleFacetsOptions) -> ChordResult<BundleFacetsResult> {
    tokio::task::spawn_blocking(move || build(&options)).await.map_err(|error| ChordError::error(error.to_string()))?
}
