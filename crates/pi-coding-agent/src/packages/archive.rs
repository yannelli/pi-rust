//! URL archive packages: download a `.tar.gz`, `.tgz`, or `.zip` package with the HTTP
//! client and unpack it with pure-Rust readers. Upstream treats every URL as git; archive
//! URLs are a pi-rust addition that lets packages install without git or npm.

use std::io::Read;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};

use super::PackageManager;
use super::manifest::has_dependencies;
use super::paths::{ensure_git_ignore, prune_empty_parents, sibling_marker};
use super::runner::git_dependency_install_args;
use super::source::{ArchiveFormat, ArchiveSource};
use crate::resources::SourceScope;

/// Validators of the last download, kept next to the install directory.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ArchiveMeta {
    url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    etag: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_modified: Option<String>,
}

fn meta_path(target: &Path) -> PathBuf {
    sibling_marker(target, "pi-archive.json")
}

fn read_meta(target: &Path) -> Option<ArchiveMeta> {
    serde_json::from_str(&std::fs::read_to_string(meta_path(target)).ok()?).ok()
}

/// A relative path with only normal components, or `None` for entries that escape.
fn safe_relative(path: &Path) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => out.push(part),
            Component::CurDir => {}
            _ => return None,
        }
    }
    (!out.as_os_str().is_empty()).then_some(out)
}

fn extract_tar_gz(bytes: &[u8], dest: &Path) -> anyhow::Result<()> {
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(bytes));
    for entry in archive.entries()? {
        let mut entry = entry?;
        let kind = entry.header().entry_type();
        let Some(relative) = safe_relative(&entry.path()?) else { continue };
        let target = dest.join(relative);
        if kind.is_dir() {
            std::fs::create_dir_all(&target)?;
        } else if kind.is_file() {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            entry.unpack(&target)?;
        }
    }
    Ok(())
}

fn extract_zip(bytes: &[u8], dest: &Path) -> anyhow::Result<()> {
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes))?;
    for index in 0..archive.len() {
        let mut file = archive.by_index(index)?;
        if file.is_symlink() {
            continue;
        }
        let Some(relative) = file.enclosed_name().and_then(|name| safe_relative(&name)) else { continue };
        let target = dest.join(relative);
        if file.is_dir() {
            std::fs::create_dir_all(&target)?;
            continue;
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut content = Vec::new();
        file.read_to_end(&mut content)?;
        std::fs::write(&target, content)?;
        #[cfg(unix)]
        if let Some(mode) = file.unix_mode() {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&target, std::fs::Permissions::from_mode(mode & 0o755))?;
        }
    }
    Ok(())
}

/// Unpack an archive into `dest`. A single top-level directory (`package/` in npm tarballs,
/// `<repo>-<ref>/` in forge archives) becomes the package root.
pub(crate) fn extract_archive(bytes: &[u8], format: ArchiveFormat, dest: &Path) -> anyhow::Result<()> {
    let staging = dest.with_file_name(format!(
        ".{}.pi-extract-{}",
        dest.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default(),
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging)?;
    let result = match format {
        ArchiveFormat::TarGz => extract_tar_gz(bytes, &staging),
        ArchiveFormat::Zip => extract_zip(bytes, &staging),
    };
    if let Err(error) = result {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(error.context("Could not unpack archive"));
    }
    let entries: Vec<PathBuf> = std::fs::read_dir(&staging)?.flatten().map(|entry| entry.path()).collect();
    let root = match entries.as_slice() {
        [only] if only.is_dir() => only.clone(),
        _ => staging.clone(),
    };
    if dest.exists() {
        std::fs::remove_dir_all(dest)?;
    }
    std::fs::rename(&root, dest)?;
    let _ = std::fs::remove_dir_all(&staging);
    Ok(())
}

enum Download {
    Body { bytes: Vec<u8>, meta: ArchiveMeta },
    NotModified,
}

impl PackageManager {
    async fn download_archive(&self, url: &str, previous: Option<&ArchiveMeta>) -> anyhow::Result<Download> {
        let mut request = self
            .http()
            .get(url)
            .header(reqwest::header::USER_AGENT, crate::platform::pi_user_agent(crate::platform::VERSION));
        if let Some(previous) = previous.filter(|meta| meta.url == url) {
            if let Some(etag) = &previous.etag {
                request = request.header(reqwest::header::IF_NONE_MATCH, etag);
            }
            if let Some(modified) = &previous.last_modified {
                request = request.header(reqwest::header::IF_MODIFIED_SINCE, modified);
            }
        }
        let response = request.send().await.with_context(|| format!("Could not download {url}"))?;
        if response.status() == reqwest::StatusCode::NOT_MODIFIED && previous.is_some() {
            return Ok(Download::NotModified);
        }
        if !response.status().is_success() {
            bail!("Could not download {url}: HTTP {}", response.status().as_u16());
        }
        let header = |name| response.headers().get(name).and_then(|value| value.to_str().ok()).map(String::from);
        let meta = ArchiveMeta {
            url: url.to_string(),
            etag: header(reqwest::header::ETAG),
            last_modified: header(reqwest::header::LAST_MODIFIED),
        };
        let bytes = response.bytes().await?.to_vec();
        Ok(Download::Body { bytes, meta })
    }

    /// Download and unpack the archive; with `refresh`, only when the server reports a change.
    pub(crate) async fn install_archive(
        &self,
        source: &ArchiveSource,
        scope: SourceScope,
        refresh: bool,
    ) -> anyhow::Result<()> {
        let target = self.archive_install_path(source, scope)?;
        let previous = if refresh && target.exists() { read_meta(&target) } else { None };
        let (bytes, meta) = match self.download_archive(&source.url, previous.as_ref()).await? {
            Download::NotModified => return Ok(()),
            Download::Body { bytes, meta } => (bytes, meta),
        };
        let root = self.install_root(scope, "url")?;
        if let Some(root) = &root {
            ensure_git_ignore(root);
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let result = async {
            extract_archive(&bytes, source.format, &target)?;
            std::fs::write(meta_path(&target), serde_json::to_string_pretty(&meta)?)?;
            if has_dependencies(&target) {
                let manager = self.package_manager_name()?;
                self.run_npm(git_dependency_install_args(&manager), Some(&target)).await?;
            }
            anyhow::Ok(())
        }
        .await;
        if result.is_err() {
            self.discard_archive(&target, root.as_deref());
        }
        result
    }

    fn discard_archive(&self, target: &Path, root: Option<&Path>) {
        let _ = std::fs::remove_dir_all(target);
        let _ = std::fs::remove_file(meta_path(target));
        prune_empty_parents(target, root);
    }

    pub(crate) fn remove_archive(&self, source: &ArchiveSource, scope: SourceScope) -> anyhow::Result<()> {
        let target = self.archive_install_path(source, scope)?;
        self.discard_archive(&target, self.install_root(scope, "url")?.as_deref());
        Ok(())
    }

    /// Ask the server whether the archive changed since the recorded download.
    pub(crate) async fn archive_has_available_update(&self, source: &ArchiveSource, target: &Path) -> bool {
        if self.is_offline() {
            return false;
        }
        let Some(meta) = read_meta(target).filter(|meta| meta.etag.is_some() || meta.last_modified.is_some()) else {
            return false;
        };
        let mut request = self.http().head(&source.url);
        if let Some(etag) = &meta.etag {
            request = request.header(reqwest::header::IF_NONE_MATCH, etag);
        }
        if let Some(modified) = &meta.last_modified {
            request = request.header(reqwest::header::IF_MODIFIED_SINCE, modified);
        }
        let Ok(response) = request.timeout(super::NETWORK_TIMEOUT).send().await else { return false };
        if !response.status().is_success() {
            return false;
        }
        let header = |name| response.headers().get(name).and_then(|value| value.to_str().ok()).map(String::from);
        let changed = |stored: &Option<String>, current: Option<String>| match (stored, current) {
            (Some(stored), Some(current)) => Some(*stored != current),
            _ => None,
        };
        changed(&meta.etag, header(reqwest::header::ETAG))
            .or_else(|| changed(&meta.last_modified, header(reqwest::header::LAST_MODIFIED)))
            .unwrap_or(false)
    }
}
