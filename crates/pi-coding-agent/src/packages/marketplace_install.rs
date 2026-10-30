//! Marketplace installs: download and check binaries, write the generated package, and
//! prune cached binaries that no installed package uses.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};

use anyhow::{Context, bail};
use serde_json::json;

use super::archive::extract_archive;
use super::marketplace::{MARKETPLACE_PACKAGE_SCOPE, Marketplace, MarketplaceState, remove_path};
use super::marketplace_index::{
    BinaryAsset, MarketplaceEntry, MarketplaceIndex, download_limited, host_target, is_windows_target, sha256_hex,
};
use super::source::ArchiveFormat;

static LOCK: Mutex<()> = Mutex::new(());
static STAGING_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Serializes binary placement, package writes, and pruning in this process.
pub(crate) fn lock() -> MutexGuard<'static, ()> {
    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn unique_suffix() -> String {
    format!("{}-{}", std::process::id(), STAGING_COUNTER.fetch_add(1, Ordering::Relaxed))
}

fn marker_path(binary: &Path) -> PathBuf {
    let mut name = binary.file_name().unwrap_or_default().to_os_string();
    name.push(".sha256");
    binary.with_file_name(name)
}

fn is_cached(binary: &Path, sha256: &str) -> bool {
    let is_file = std::fs::symlink_metadata(binary).is_ok_and(|meta| meta.is_file());
    is_file && std::fs::read_to_string(marker_path(binary)).is_ok_and(|marker| marker.trim() == sha256)
}

/// A checked binary in a staging directory that is deleted on drop.
struct Staged {
    dir: PathBuf,
    file: PathBuf,
}

impl Drop for Staged {
    fn drop(&mut self) {
        let _ = remove_path(&self.dir);
    }
}

fn real_dirs(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    entries
        .flatten()
        .filter(|entry| !entry.file_name().to_string_lossy().starts_with('.'))
        .filter(|entry| std::fs::symlink_metadata(entry.path()).is_ok_and(|meta| meta.is_dir()))
        .map(|entry| entry.path())
        .collect()
}

impl Marketplace {
    pub(crate) async fn install_from_index(
        &self,
        index: &MarketplaceIndex,
        name: &str,
        pinned: bool,
        expected: Option<MarketplaceState>,
    ) -> anyhow::Result<PathBuf> {
        let Some(entry) = index.entry(name) else {
            bail!("Marketplace release {} has no extension named {name}", index.tag);
        };
        let Some(target) = host_target() else {
            bail!("Marketplace extensions do not support {}-{}", std::env::consts::OS, std::env::consts::ARCH);
        };
        if !entry.platforms.iter().any(|platform| platform == target) {
            bail!("Marketplace extension {name} does not support {target}");
        }
        let Some(asset) = index.asset(entry, target) else {
            bail!("Marketplace release {} has no {} binary for {target}", index.tag, entry.binary);
        };
        let state = MarketplaceState {
            name: name.to_string(),
            tag: index.tag.clone(),
            pinned,
            target: target.to_string(),
            binary: entry.binary.clone(),
            sha256: asset.sha256.to_ascii_lowercase(),
            version: entry.version.clone(),
        };
        if let Some(path) = self.commit(&state, entry, None, expected.as_ref())? {
            return Ok(path);
        }
        let staged = self.download_binary(index, asset, &state).await?;
        self.commit(&state, entry, Some(staged), expected.as_ref())?.context("Could not place the marketplace binary")
    }

    /// Place the binary and write the package. `None` means the binary must be downloaded first.
    fn commit(
        &self,
        state: &MarketplaceState,
        entry: &MarketplaceEntry,
        staged: Option<Staged>,
        expected: Option<&MarketplaceState>,
    ) -> anyhow::Result<Option<PathBuf>> {
        let _guard = lock();
        let path = self.package_path(&state.name)?;
        if expected.is_some() && self.read_state(&state.name)?.as_ref() != expected {
            return Ok(Some(path));
        }
        let binary = self.binary_path(&state.tag, &state.target, &state.binary);
        match staged {
            Some(staged) => place_binary(&staged, &binary, &state.sha256)?,
            None if is_cached(&binary, &state.sha256) => {}
            None => return Ok(None),
        }
        write_package(&path, entry, state, &binary)?;
        self.prune_binaries();
        Ok(Some(path))
    }

    async fn download_binary(
        &self,
        index: &MarketplaceIndex,
        asset: &BinaryAsset,
        state: &MarketplaceState,
    ) -> anyhow::Result<Staged> {
        let url = index.asset_url(asset)?;
        let bytes = download_limited(&self.http, url.as_str(), asset.size).await?;
        if bytes.len() as u64 != asset.size {
            bail!("Binary {url} has {} bytes, but the index lists {}", bytes.len(), asset.size);
        }
        if sha256_hex(&bytes) != state.sha256 {
            bail!("Binary {url} has a SHA-256 that does not match the index. Do not use this download.");
        }
        let binary = self.binary_path(&state.tag, &state.target, &state.binary);
        let parent = binary.parent().context("Marketplace binary path has no parent")?;
        std::fs::create_dir_all(parent)?;
        let dir = parent.join(format!(".staging-{}-{}", state.binary, unique_suffix()));
        let staged = Staged { file: dir.join(binary.file_name().unwrap_or_default()), dir };
        let format = if is_windows_target(&state.target) { ArchiveFormat::Zip } else { ArchiveFormat::TarGz };
        extract_archive(&bytes, format, &staged.dir)?;
        if !std::fs::symlink_metadata(&staged.file).is_ok_and(|meta| meta.is_file()) {
            bail!("Archive {url} has no {} file", staged.file.file_name().unwrap_or_default().to_string_lossy());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&staged.file, std::fs::Permissions::from_mode(0o755))?;
        }
        Ok(staged)
    }

    /// Binaries named by every package state; any state that cannot be read is an error.
    fn referenced_binaries(&self) -> anyhow::Result<HashSet<PathBuf>> {
        let entries = match std::fs::read_dir(self.root().join("packages")) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(HashSet::new()),
            Err(error) => return Err(error.into()),
        };
        let mut referenced = HashSet::new();
        for entry in entries {
            let name = entry?.file_name().into_string().map_err(|_| anyhow::anyhow!("Package name is not UTF-8"))?;
            if name.starts_with('.') {
                continue;
            }
            if let Some(state) = self.read_state(&name)? {
                referenced.insert(self.binary_path(&state.tag, &state.target, &state.binary));
            }
        }
        Ok(referenced)
    }

    /// Delete cached binaries and markers that no installed package references.
    pub(crate) fn prune_binaries(&self) {
        let Ok(referenced) = self.referenced_binaries() else { return };
        for tag_dir in real_dirs(&self.bin_root()) {
            for target_dir in real_dirs(&tag_dir) {
                let Ok(entries) = std::fs::read_dir(&target_dir) else { continue };
                for entry in entries.flatten() {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if name.starts_with('.') {
                        continue;
                    }
                    let path = entry.path();
                    let owner = name.strip_suffix(".sha256").map(|binary| target_dir.join(binary));
                    if !referenced.contains(&path) && !owner.is_some_and(|owner| referenced.contains(&owner)) {
                        let _ = remove_path(&path);
                    }
                }
                let _ = std::fs::remove_dir(&target_dir);
            }
            let _ = std::fs::remove_dir(&tag_dir);
        }
    }
}

fn place_binary(staged: &Staged, binary: &Path, sha256: &str) -> anyhow::Result<()> {
    let parent = binary.parent().context("Marketplace binary path has no parent")?;
    std::fs::create_dir_all(parent)?;
    let marker = marker_path(binary);
    remove_path(&marker)?;
    if std::fs::symlink_metadata(binary).is_ok_and(|meta| !meta.is_file()) {
        remove_path(binary)?;
    }
    std::fs::rename(&staged.file, binary).with_context(|| format!("Could not write {}", binary.display()))?;
    std::fs::write(&marker, sha256)?;
    Ok(())
}

fn write_package(path: &Path, entry: &MarketplaceEntry, state: &MarketplaceState, binary: &Path) -> anyhow::Result<()> {
    if std::fs::symlink_metadata(path).is_ok_and(|meta| !meta.is_dir()) {
        remove_path(path)?;
    }
    std::fs::create_dir_all(path)?;
    let manifest = json!({
        "name": format!("{MARKETPLACE_PACKAGE_SCOPE}/{}", state.name),
        "version": state.version,
        "description": entry.description,
        "pi": { "native": [{ "command": binary.to_string_lossy(), "args": entry.args, "env": {} }] },
        "piMarketplace": state,
    });
    let temp = path.join(format!(".package.json.{}", unique_suffix()));
    std::fs::write(&temp, serde_json::to_string_pretty(&manifest)?)?;
    std::fs::rename(&temp, path.join("package.json")).inspect_err(|_| {
        let _ = std::fs::remove_file(&temp);
    })?;
    Ok(())
}
