//! Server plugin package profiles and builds (upstream `plugins/package.ts`).

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use indexmap::IndexMap;
use pi_chord::node::{
    BundleFacetPackageOptions, FACET_BUNDLE_MANIFEST_FILE, FacetBundleArtifact, bundle_facet_package,
    read_facet_bundle_artifact,
};
use pi_chord::{ChordError, ChordResult};
use serde_json::{Value, json};

const PLUGIN_PACKAGE_PROFILE_VERSION: u64 = 1;

/// Node's `path.resolve` for one path: absolute, with `.` and `..` resolved lexically.
pub fn resolve_path(path: &Path) -> PathBuf {
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let mut resolved = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                resolved.pop();
            }
            other => resolved.push(other),
        }
    }
    resolved
}

fn sha256_hex(text: &str) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, text.as_bytes());
    digest.as_ref().iter().map(|byte| format!("{byte:02x}")).collect()
}

pub fn normalize_plugin_package_paths(package_paths: &[String]) -> ChordResult<Vec<String>> {
    let mut normalized = Vec::new();
    for path in package_paths {
        if path.is_empty() {
            return Err(ChordError::error("Plugin package path must not be empty"));
        }
        normalized.push(resolve_path(Path::new(path)).display().to_string());
    }
    if normalized.iter().collect::<HashSet<_>>().len() != normalized.len() {
        return Err(ChordError::error("Plugin package paths must be unique"));
    }
    Ok(normalized)
}

/// Persist an explicit plugin package selection or restore it for a later server generation.
pub async fn restore_server_plugin_package_profile(
    directory: &Path,
    server_id: &str,
    configured_package_paths: Option<&[String]>,
) -> ChordResult<Vec<String>> {
    let path = directory.join(format!("plugin-packages-{server_id}.json"));
    let Some(configured) = configured_package_paths else {
        return Ok(read_plugin_package_profile(&path, false, None).await?.unwrap_or_default());
    };
    let package_paths = normalize_plugin_package_paths(configured)?;
    if package_paths.is_empty() {
        match tokio::fs::remove_file(&path).await {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => return Err(ChordError::other(error)),
            _ => {}
        }
    } else {
        write_plugin_package_profile(&path, &package_paths, None).await?;
    }
    Ok(package_paths)
}

fn session_plugin_profile_path(directory: &Path, server_id: &str, session_path: &str) -> PathBuf {
    let hash = &sha256_hex(session_path)[..24];
    directory.join(format!("session-plugin-packages-{server_id}-{hash}.json"))
}

/// Read the explicit plugin selection stored for one durable Session.
pub async fn read_session_plugin_package_profile(
    directory: &Path,
    server_id: &str,
    session_path: &str,
) -> ChordResult<Option<Vec<String>>> {
    let path = session_plugin_profile_path(directory, server_id, session_path);
    read_plugin_package_profile(&path, true, Some(session_path)).await
}

/// Remove the plugin selection stored for one deleted Session.
pub async fn remove_session_plugin_package_profile(
    directory: &Path,
    server_id: &str,
    session_path: &str,
) -> ChordResult<()> {
    match tokio::fs::remove_file(session_plugin_profile_path(directory, server_id, session_path)).await {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(ChordError::other(error)),
        _ => Ok(()),
    }
}

/// Persist the plugin selection for one durable Session.
pub async fn write_session_plugin_package_profile(
    directory: &Path,
    server_id: &str,
    session_path: &str,
    package_paths: &[String],
) -> ChordResult<()> {
    let path = session_plugin_profile_path(directory, server_id, session_path);
    write_plugin_package_profile(&path, &normalize_plugin_package_paths(package_paths)?, Some(session_path)).await
}

async fn read_plugin_package_profile(
    path: &Path,
    allow_empty: bool,
    session_path: Option<&str>,
) -> ChordResult<Option<Vec<String>>> {
    let text = match tokio::fs::read_to_string(path).await {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            let message = format!("Could not read experimental plugin package profile {}: {error}", path.display());
            return Err(ChordError::error(message));
        }
    };
    let invalid = || ChordError::error(format!("Invalid experimental plugin package profile {}", path.display()));
    let parsed: Value = serde_json::from_str(&text).map_err(|_| {
        ChordError::error(format!("Could not read experimental plugin package profile {}", path.display()))
    })?;
    let object = parsed.as_object().ok_or_else(invalid)?;
    let allowed =
        |key: &str| key == "version" || key == "packagePaths" || (key == "sessionPath" && session_path.is_some());
    let session_matches = match session_path {
        None => !object.contains_key("sessionPath"),
        Some(expected) => object.get("sessionPath").and_then(Value::as_str) == Some(expected),
    };
    let paths = object.get("packagePaths").and_then(Value::as_array).ok_or_else(invalid)?;
    let paths: Option<Vec<String>> =
        paths.iter().map(|path| path.as_str().filter(|path| !path.is_empty()).map(str::to_string)).collect();
    let paths = paths.ok_or_else(invalid)?;
    if !object.keys().all(|key| allowed(key))
        || object.get("version").and_then(Value::as_u64) != Some(PLUGIN_PACKAGE_PROFILE_VERSION)
        || !session_matches
        || (!allow_empty && paths.is_empty())
    {
        return Err(invalid());
    }
    normalize_plugin_package_paths(&paths).map(Some)
}

async fn write_plugin_package_profile(
    path: &Path,
    package_paths: &[String],
    session_path: Option<&str>,
) -> ChordResult<()> {
    let mut value = json!({ "version": PLUGIN_PACKAGE_PROFILE_VERSION });
    if let Some(session_path) = session_path {
        value["sessionPath"] = Value::from(session_path);
    }
    value["packagePaths"] = Value::from(package_paths.to_vec());
    let text = format!("{}\n", serde_json::to_string_pretty(&value).map_err(ChordError::other)?);
    let mut options = tokio::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(path).await.map_err(ChordError::other)?;
    tokio::io::AsyncWriteExt::write_all(&mut file, text.as_bytes()).await.map_err(ChordError::other)
}

fn plugin_build_directory_name(package_path: &Path) -> String {
    let directory = if package_path.file_name().is_some_and(|name| name == "package.json") {
        package_path.parent().unwrap_or(package_path)
    } else {
        package_path
    };
    let base: String = directory
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || "._-".contains(c) { c } else { '-' })
        .collect();
    let base = if base.is_empty() { "plugin".to_string() } else { base };
    let label = if base.ends_with("-plugin") { base } else { format!("{base}-plugin") };
    format!("{label}-{}", &sha256_hex(&package_path.display().to_string())[..12])
}

/// A serialized server-owned builder for one configured plugin package.
#[derive(Clone)]
pub struct ConfiguredServerPluginPackage {
    pub manifest_path: PathBuf,
    package_path: PathBuf,
    outdir: PathBuf,
    builds: Arc<tokio::sync::Mutex<()>>,
}

impl ConfiguredServerPluginPackage {
    /// Build the conventional `src/session.ts` and `src/tui.ts` entries; returns the TUI artifact.
    pub async fn build(&self) -> ChordResult<Vec<FacetBundleArtifact>> {
        let _serialized = self.builds.lock().await;
        let default_facets: IndexMap<String, String> =
            [("session", "src/session.ts"), ("tui", "src/tui.ts")].map(|(k, v)| (k.to_string(), v.to_string())).into();
        let options = BundleFacetPackageOptions {
            package_path: self.package_path.clone(),
            outdir: self.outdir.clone(),
            default_facets,
        };
        let result = bundle_facet_package(options).await?;
        if !result.manifest.entries.contains_key("tui") {
            return Ok(Vec::new());
        }
        Ok(vec![read_facet_bundle_artifact(&result.manifest_path, "tui").await?])
    }
}

pub fn create_server_plugin_package(
    directory: &Path,
    server_id: &str,
    package_path: &str,
) -> ConfiguredServerPluginPackage {
    let package_path = resolve_path(Path::new(package_path));
    let outdir = directory.join("plugin-builds").join(server_id).join(plugin_build_directory_name(&package_path));
    ConfiguredServerPluginPackage {
        manifest_path: outdir.join(FACET_BUNDLE_MANIFEST_FILE),
        package_path,
        outdir,
        builds: Arc::default(),
    }
}
