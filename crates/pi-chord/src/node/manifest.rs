//! Facet bundle manifests and transportable artifacts (upstream `node/manifest.ts`).

use std::path::{Path, PathBuf};

use base64::Engine;
use indexmap::IndexMap;

use crate::error::{ChordError, ChordResult};
use crate::json::{Json, JsonMap};

pub const FACET_BUNDLE_FORMAT: &str = "chord.facet-bundle";
pub const FACET_BUNDLE_FORMAT_VERSION: u64 = 2;
pub const FACET_BUNDLE_MANIFEST_FILE: &str = "chord-facets.json";
pub const FACET_BUNDLE_ARTIFACT_FORMAT: &str = "chord.facet-bundle-artifact";
pub const FACET_BUNDLE_ARTIFACT_FORMAT_VERSION: u64 = 2;

/// One bundled entry, described relative to its manifest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FacetBundleEntry {
    /// Content-addressed CommonJS filename relative to the manifest.
    pub file: String,
    /// SHA-256 subresource-integrity value for the JavaScript file.
    pub integrity: String,
    /// Imports left for the loading application to resolve.
    pub external_imports: Vec<String>,
    /// Source map filename relative to the manifest, when emitted.
    pub source_map: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FacetBundlePlugin {
    pub id: String,
    pub version: Option<String>,
}

/// A versioned `chord-facets.json`; entries keep JavaScript object key order when written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FacetBundleManifest {
    pub plugin: FacetBundlePlugin,
    pub entries: IndexMap<String, FacetBundleEntry>,
}

/// One self-contained manifest entry for storage or transport to another host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FacetBundleArtifact {
    pub plugin: FacetBundlePlugin,
    pub entry_name: String,
    pub entry: FacetBundleEntry,
    pub source: String,
    pub source_map_contents: Option<String>,
}

impl FacetBundleEntry {
    pub fn to_json(&self) -> Json {
        let mut map = JsonMap::new();
        map.insert("file", self.file.as_str());
        map.insert("integrity", self.integrity.as_str());
        map.insert(
            "externalImports",
            self.external_imports.iter().map(|item| Json::from(item.as_str())).collect::<Vec<_>>(),
        );
        if let Some(source_map) = &self.source_map {
            map.insert("sourceMap", source_map.as_str());
        }
        Json::from(map)
    }
}

impl FacetBundlePlugin {
    pub fn to_json(&self) -> Json {
        let mut map = JsonMap::new();
        map.insert("id", self.id.as_str());
        if let Some(version) = &self.version {
            map.insert("version", version.as_str());
        }
        Json::from(map)
    }
}

impl FacetBundleManifest {
    pub fn to_json(&self) -> Json {
        let mut entries = JsonMap::new();
        for (name, entry) in &self.entries {
            entries.insert(name.as_str(), entry.to_json());
        }
        let mut map = JsonMap::new();
        map.insert("format", FACET_BUNDLE_FORMAT);
        map.insert("formatVersion", FACET_BUNDLE_FORMAT_VERSION);
        map.insert("plugin", self.plugin.to_json());
        map.insert("entries", entries);
        Json::from(map)
    }

    /// The file text upstream writes: `JSON.stringify(manifest, null, 2)` and a newline.
    pub fn to_file_text(&self) -> String {
        format!("{}\n", self.to_json().to_string_pretty(2))
    }

    /// Parse and check a manifest value; `path` names it in errors.
    pub fn parse(value: &Json, path: &str) -> ChordResult<Self> {
        validate_manifest(value, path)
    }
}

impl FacetBundleArtifact {
    pub fn to_json(&self) -> Json {
        let mut map = JsonMap::new();
        map.insert("format", FACET_BUNDLE_ARTIFACT_FORMAT);
        map.insert("formatVersion", FACET_BUNDLE_ARTIFACT_FORMAT_VERSION);
        map.insert("plugin", self.plugin.to_json());
        map.insert("entryName", self.entry_name.as_str());
        map.insert("entry", self.entry.to_json());
        map.insert("source", self.source.as_str());
        if let Some(contents) = &self.source_map_contents {
            map.insert("sourceMapContents", contents.as_str());
        }
        Json::from(map)
    }

    /// Check a transported artifact, including its source integrity.
    pub fn parse(value: &Json) -> ChordResult<Self> {
        let invalid = || ChordError::error("Invalid facet bundle artifact");
        let object = value.as_object().ok_or_else(invalid)?;
        let entry_name = object.get("entryName").and_then(Json::as_str).filter(|name| !name.is_empty());
        let source = object.get("source").and_then(Json::as_str);
        let format_ok = object.get("format").and_then(Json::as_str) == Some(FACET_BUNDLE_ARTIFACT_FORMAT)
            && object.get("formatVersion").and_then(Json::as_f64) == Some(FACET_BUNDLE_ARTIFACT_FORMAT_VERSION as f64);
        let (Some(entry_name), Some(source), true) = (entry_name, source, format_ok) else { return Err(invalid()) };
        let mut entries = JsonMap::new();
        entries.insert(entry_name, object.get("entry").cloned().unwrap_or(Json::Null));
        let mut wrapper = JsonMap::new();
        wrapper.insert("format", FACET_BUNDLE_FORMAT);
        wrapper.insert("formatVersion", FACET_BUNDLE_FORMAT_VERSION);
        if let Some(plugin) = object.get("plugin") {
            wrapper.insert("plugin", plugin.clone());
        }
        wrapper.insert("entries", entries);
        let manifest = validate_manifest(&Json::from(wrapper), "facet bundle artifact")?;
        let entry = manifest.entries.get(entry_name).cloned().ok_or_else(invalid)?;
        let contents = object.get("sourceMapContents");
        let source_map_contents = match (&entry.source_map, contents) {
            (None, None) => None,
            (None, Some(_)) => {
                return Err(ChordError::error("Facet bundle artifact has source map contents without a source map"));
            }
            (Some(_), Some(Json::String(text))) => Some(text.to_string()),
            (Some(_), _) => return Err(ChordError::error("Facet bundle artifact is missing its source map contents")),
        };
        verify_source(source, &entry)?;
        Ok(Self {
            plugin: manifest.plugin,
            entry_name: entry_name.to_string(),
            entry,
            source: source.to_string(),
            source_map_contents,
        })
    }
}

/// `sha256-` plus the base64 SHA-256 digest, as subresource integrity writes it.
pub fn integrity_of(bytes: &[u8]) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, bytes);
    format!("sha256-{}", base64::engine::general_purpose::STANDARD.encode(digest.as_ref()))
}

pub(crate) fn parse_integrity(integrity: &str) -> ChordResult<&str> {
    match integrity.strip_prefix("sha256-") {
        Some(digest) if !digest.is_empty() => Ok(digest),
        _ => Err(ChordError::error("Facet bundle entry has an invalid SHA-256 integrity value")),
    }
}

pub(crate) fn verify_source(source: &str, entry: &FacetBundleEntry) -> ChordResult<()> {
    let expected = parse_integrity(&entry.integrity)?;
    if integrity_of(source.as_bytes()).strip_prefix("sha256-") != Some(expected) {
        return Err(ChordError::error(format!("Facet bundle integrity check failed for {}", entry.file)));
    }
    Ok(())
}

/// A bundle file named relative to its manifest: a bare filename, never a path.
pub(crate) fn resolve_bundle_file(manifest_path: &Path, file: &str, label: &str) -> ChordResult<PathBuf> {
    let bare = Path::new(file).file_name().and_then(|name| name.to_str()) == Some(file)
        && !file.contains(['/', '\\'])
        && file != "."
        && file != "..";
    if file.is_empty() || !bare {
        return Err(ChordError::error(format!("Facet bundle {label} must be a filename relative to its manifest")));
    }
    Ok(manifest_path.parent().unwrap_or(Path::new("")).join(file))
}

/// JavaScript `String(value)` for error messages.
fn js_string(value: Option<&Json>) -> String {
    match value {
        None => "undefined".into(),
        Some(Json::String(text)) => text.to_string(),
        Some(Json::Object(_)) => "[object Object]".into(),
        Some(Json::Array(items)) => items.iter().map(|item| js_string(Some(item))).collect::<Vec<_>>().join(","),
        Some(other) => other.to_string(),
    }
}

fn validate_manifest(value: &Json, path: &str) -> ChordResult<FacetBundleManifest> {
    let error = |message: String| ChordError::error(message);
    let object = value
        .as_object()
        .filter(|object| object.get("format").and_then(Json::as_str) == Some(FACET_BUNDLE_FORMAT))
        .ok_or_else(|| error(format!("Invalid facet bundle manifest format in {path}")))?;
    let version = object.get("formatVersion");
    if version.and_then(Json::as_f64) != Some(FACET_BUNDLE_FORMAT_VERSION as f64) {
        let shown = js_string(version);
        return Err(error(format!("Unsupported facet bundle manifest version in {path}: {shown}")));
    }
    let plugin = object.get("plugin").and_then(Json::as_object);
    let id = plugin.and_then(|plugin| plugin.get("id")).and_then(Json::as_str).filter(|id| !id.is_empty());
    let (Some(plugin), Some(id)) = (plugin, id) else {
        return Err(error(format!("Facet bundle manifest has an invalid plugin identity in {path}")));
    };
    let version = match plugin.get("version") {
        None => None,
        Some(Json::String(version)) if !version.is_empty() => Some(version.to_string()),
        Some(_) => return Err(error(format!("Facet bundle manifest has an invalid plugin version in {path}"))),
    };
    let entries_value = object.get("entries").and_then(Json::as_object).filter(|entries| !entries.is_empty());
    let entries_value =
        entries_value.ok_or_else(|| error(format!("Facet bundle manifest has no entries in {path}")))?;
    let mut entries = IndexMap::new();
    for (name, candidate) in entries_value.iter() {
        let candidate = candidate
            .as_object()
            .filter(|_| !name.is_empty())
            .ok_or_else(|| error(format!("Facet bundle manifest has an invalid entry in {path}")))?;
        let file = candidate
            .get("file")
            .and_then(Json::as_str)
            .ok_or_else(|| error(format!("Facet bundle entry {name} has no file")))?;
        resolve_bundle_file(Path::new(path), file, &format!("entry {name}"))?;
        let integrity = candidate
            .get("integrity")
            .and_then(Json::as_str)
            .ok_or_else(|| error(format!("Facet bundle entry {name} has no integrity")))?;
        parse_integrity(integrity)?;
        let imports = candidate.get("externalImports").and_then(Json::as_array);
        let imports: Option<Vec<String>> =
            imports.and_then(|items| items.iter().map(|item| item.as_str().map(str::to_string)).collect());
        let external_imports =
            imports.ok_or_else(|| error(format!("Facet bundle entry {name} has invalid external imports")))?;
        let unique: std::collections::HashSet<&String> = external_imports.iter().collect();
        if unique.len() != external_imports.len() {
            return Err(error(format!("Facet bundle entry {name} has duplicate external imports")));
        }
        let source_map = match candidate.get("sourceMap") {
            None => None,
            Some(Json::String(source_map)) => {
                resolve_bundle_file(Path::new(path), source_map, &format!("entry {name} source map"))?;
                Some(source_map.to_string())
            }
            Some(_) => return Err(error(format!("Facet bundle entry {name} has an invalid source map"))),
        };
        let entry =
            FacetBundleEntry { file: file.to_string(), integrity: integrity.to_string(), external_imports, source_map };
        entries.insert(name.to_string(), entry);
    }
    Ok(FacetBundleManifest { plugin: FacetBundlePlugin { id: id.to_string(), version }, entries })
}
