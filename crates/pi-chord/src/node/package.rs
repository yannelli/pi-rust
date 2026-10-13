//! Plugin package bundling from `package.json` metadata (upstream `node/package.ts`).

use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};

use indexmap::IndexMap;

use super::bundler::{BundleFacetsOptions, BundleFacetsResult, bundle_facets};
use super::manifest::FacetBundlePlugin;
use crate::error::{ChordError, ChordResult};
use crate::json::Json;

#[derive(Clone, Debug)]
pub struct BundleFacetPackageOptions {
    /// Plugin package directory or its package.json path.
    pub package_path: PathBuf,
    pub outdir: PathBuf,
    /// Application conventions applied when the corresponding source file exists.
    pub default_facets: IndexMap<String, String>,
}

#[derive(Clone, Debug)]
pub struct BundleFacetPackageResult {
    pub manifest: super::FacetBundleManifest,
    pub manifest_path: PathBuf,
    pub package_directory: PathBuf,
    pub package_json_path: PathBuf,
}

struct Metadata {
    directory: PathBuf,
    json_path: PathBuf,
    name: String,
    version: String,
    peer_dependencies: Vec<String>,
    configured: IndexMap<String, Option<String>>,
    external: Vec<String>,
    source_map: bool,
}

/// A failure that keeps its underlying cause, like `new Error(message, { cause })`.
fn caused(message: String, cause: impl std::error::Error + Send + Sync + 'static) -> ChordError {
    ChordError::Aggregate { message, errors: vec![ChordError::other(cause)] }
}

fn fail(message: String) -> ChordError {
    ChordError::error(message)
}

async fn read_metadata(package_path: &Path) -> ChordResult<Metadata> {
    if package_path.as_os_str().is_empty() {
        return Err(ChordError::type_error("Facet package path must not be empty"));
    }
    let candidate = std::path::absolute(package_path).map_err(ChordError::other)?;
    let shown = candidate.display();
    let stats = tokio::fs::metadata(&candidate)
        .await
        .map_err(|error| caused(format!("Could not access facet package {shown}"), error))?;
    let (directory, json_path) = if stats.is_dir() {
        let directory = tokio::fs::canonicalize(&candidate).await.map_err(ChordError::other)?;
        (directory.clone(), directory.join("package.json"))
    } else if stats.is_file() && candidate.file_name().is_some_and(|name| name == "package.json") {
        let json_path = tokio::fs::canonicalize(&candidate).await.map_err(ChordError::other)?;
        (json_path.parent().map(Path::to_path_buf).unwrap_or_default(), json_path)
    } else {
        return Err(fail(format!("Facet package path must name a directory or package.json: {shown}")));
    };
    let at = json_path.display().to_string();
    let read_error = |error: ChordError| ChordError::Aggregate {
        message: format!("Could not read facet package metadata {at}"),
        errors: vec![error],
    };
    let text = tokio::fs::read_to_string(&json_path).await.map_err(|error| read_error(ChordError::other(error)))?;
    let parsed = Json::parse(&text).map_err(read_error)?;
    if parsed.as_object().is_none() {
        return Err(fail(format!("Facet package metadata must be an object: {at}")));
    }
    let text_field = |field: &str| parsed.get(field).and_then(Json::as_str).filter(|value| !value.is_empty());
    let name = text_field("name").ok_or_else(|| fail(format!("Facet package must have a non-empty name: {at}")))?;
    let version =
        text_field("version").ok_or_else(|| fail(format!("Facet package must have a non-empty version: {at}")))?;
    let peer_dependencies = parse_peer_dependencies(parsed.get("peerDependencies"), &at)?;
    let (configured, external, source_map) = parse_chord(parsed.get("chord"), &at)?;
    Ok(Metadata {
        name: name.to_string(),
        version: version.to_string(),
        directory,
        json_path,
        peer_dependencies,
        configured,
        external,
        source_map,
    })
}

fn parse_peer_dependencies(value: Option<&Json>, at: &str) -> ChordResult<Vec<String>> {
    let Some(value) = value else { return Ok(Vec::new()) };
    let invalid = || fail(format!("Facet package has invalid peerDependencies: {at}"));
    let map = value.as_object().ok_or_else(invalid)?;
    if map.iter().any(|(name, version)| name.is_empty() || version.as_str().is_none()) {
        return Err(invalid());
    }
    let mut names: Vec<String> = map.iter().map(|(name, _)| name.to_string()).collect();
    names.sort();
    Ok(names)
}

type ChordConfiguration = (IndexMap<String, Option<String>>, Vec<String>, bool);

fn parse_chord(value: Option<&Json>, at: &str) -> ChordResult<ChordConfiguration> {
    let Some(value) = value else { return Ok((IndexMap::new(), Vec::new(), true)) };
    let map =
        value.as_object().ok_or_else(|| fail(format!("Facet package chord configuration must be an object: {at}")))?;
    if map.iter().any(|(key, _)| !matches!(&**key, "facets" | "external" | "sourceMap")) {
        return Err(fail(format!("Facet package chord configuration has an unknown field: {at}")));
    }
    let mut facets = IndexMap::new();
    if let Some(configured) = value.get("facets") {
        let configured = configured
            .as_object()
            .ok_or_else(|| fail(format!("Facet package chord.facets must be an object: {at}")))?;
        for (name, source) in configured.iter() {
            let source = match source {
                Json::Bool(false) => None,
                Json::String(path) if !path.is_empty() => Some(path.to_string()),
                _ => return Err(fail(format!("Facet package has an invalid chord.facets entry: {at}"))),
            };
            if name.is_empty() {
                return Err(fail(format!("Facet package has an invalid chord.facets entry: {at}")));
            }
            facets.insert(name.to_string(), source);
        }
    }
    let mut external = Vec::new();
    if let Some(configured) = value.get("external") {
        let invalid = || fail(format!("Facet package chord.external must contain non-empty strings: {at}"));
        let items = configured.as_array().ok_or_else(invalid)?;
        let mut unique = BTreeSet::new();
        for item in items {
            unique.insert(item.as_str().filter(|text| !text.is_empty()).ok_or_else(invalid)?.to_string());
        }
        external = unique.into_iter().collect();
    }
    let source_map = match value.get("sourceMap") {
        None => true,
        Some(Json::Bool(flag)) => *flag,
        Some(_) => return Err(fail(format!("Facet package chord.sourceMap must be a boolean: {at}"))),
    };
    Ok((facets, external, source_map))
}

fn validate_mapping(name: &str, source: &str, kind: &str) -> ChordResult<()> {
    if name.is_empty() {
        return Err(fail(format!("Facet package {kind} entry name must not be empty")));
    }
    if source.is_empty() {
        return Err(fail(format!("Facet package {kind} entry {name} must have a source path")));
    }
    Ok(())
}

/// Whether `path` lies outside `base`, compared by components.
fn escapes(base: &Path, path: &Path) -> bool {
    path.strip_prefix(base).is_err()
}

fn resolve_entry(directory: &Path, source: &str, name: &str) -> ChordResult<PathBuf> {
    if Path::new(source).is_absolute() {
        return Err(fail(format!("Facet package entry {name} must be relative to the package directory")));
    }
    let mut path = directory.to_path_buf();
    for component in Path::new(source).components() {
        match component {
            Component::ParentDir => {
                path.pop();
            }
            Component::Normal(part) => path.push(part),
            _ => {}
        }
    }
    if path == directory || escapes(directory, &path) {
        return Err(fail(format!("Facet package entry {name} escapes the package directory")));
    }
    Ok(path)
}

async fn canonical_entry(directory: &Path, path: &Path, name: &str) -> ChordResult<PathBuf> {
    let canonical = tokio::fs::canonicalize(path).await.map_err(ChordError::other)?;
    if escapes(directory, &canonical) {
        return Err(fail(format!("Facet package entry {name} resolves outside the package directory")));
    }
    Ok(canonical)
}

async fn resolve_entries(
    metadata: &Metadata,
    defaults: &IndexMap<String, String>,
) -> ChordResult<IndexMap<String, PathBuf>> {
    let mut entries = IndexMap::new();
    for (name, source) in defaults {
        validate_mapping(name, source, "default")?;
        let path = resolve_entry(&metadata.directory, source, name)?;
        let stats = match tokio::fs::metadata(&path).await {
            Ok(stats) => stats,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(ChordError::other(error)),
        };
        if !stats.is_file() {
            return Err(fail(format!("Default facet entry {name} is not a file: {}", path.display())));
        }
        entries.insert(name.clone(), canonical_entry(&metadata.directory, &path, name).await?);
    }
    for (name, source) in &metadata.configured {
        let Some(source) = source else {
            entries.shift_remove(name);
            continue;
        };
        validate_mapping(name, source, "configured")?;
        let path = resolve_entry(&metadata.directory, source, name)?;
        let shown = path.display();
        let stats = tokio::fs::metadata(&path)
            .await
            .map_err(|error| caused(format!("Could not access configured facet entry {name}: {shown}"), error))?;
        if !stats.is_file() {
            return Err(fail(format!("Configured facet entry {name} is not a file: {shown}")));
        }
        entries.insert(name.clone(), canonical_entry(&metadata.directory, &path, name).await?);
    }
    if entries.is_empty() {
        let name = &metadata.name;
        return Err(fail(format!("Facet package {name} has no configured or conventional facet entries")));
    }
    Ok(entries)
}

/// Build a plugin package using package.json metadata and application-provided facet conventions.
pub async fn bundle_facet_package(options: BundleFacetPackageOptions) -> ChordResult<BundleFacetPackageResult> {
    let metadata = read_metadata(&options.package_path).await?;
    let entries = resolve_entries(&metadata, &options.default_facets).await?;
    let external = metadata
        .peer_dependencies
        .iter()
        .chain(&metadata.external)
        .flat_map(|specifier| [specifier.clone(), format!("{specifier}/*")])
        .collect();
    let plugin = FacetBundlePlugin { id: metadata.name.clone(), version: Some(metadata.version.clone()) };
    let mut bundle = BundleFacetsOptions::new(plugin, entries, options.outdir);
    bundle.working_directory = Some(metadata.directory.clone());
    bundle.external = external;
    bundle.source_map = metadata.source_map;
    let BundleFacetsResult { manifest, manifest_path } = bundle_facets(bundle).await?;
    Ok(BundleFacetPackageResult {
        manifest,
        manifest_path,
        package_directory: metadata.directory,
        package_json_path: metadata.json_path,
    })
}
