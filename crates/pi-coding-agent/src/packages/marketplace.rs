//! Marketplace extensions from signed `yannelli/pi-rust-extensions` releases, installed as
//! generated `pi.native` packages that share binaries under `<agentDir>/marketplace/bin`.

use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use base64::Engine as _;
use serde::{Deserialize, Serialize};

pub use super::marketplace_index::{
    BinaryAsset, MARKETPLACE_TARGETS, MarketplaceEntry, MarketplaceIndex, SUPPORTED_PROTOCOL_VERSION,
    SUPPORTED_SCHEMA_VERSION, host_target, validate_name, validate_tag,
};
use super::marketplace_index::{
    MAX_INDEX_BYTES, MAX_SIGNATURE_BYTES, download_limited, is_windows_target, parse_index, validate_target,
    verify_signature,
};

/// Base64 of the raw Ed25519 public key that signs `index.json`.
pub const MARKETPLACE_PUBLIC_KEY: &str = "G33uzqR2bwGJyZD3qnY4BNThlZzwyntF+DWRTMQYBBo=";
pub const DEFAULT_MARKETPLACE_URL: &str = "https://github.com/yannelli/pi-rust-extensions/releases";
pub const MARKETPLACE_URL_ENV: &str = "PI_MARKETPLACE_URL";
pub const MARKETPLACE_PACKAGE_SCOPE: &str = "@pi-rust-extensions";

/// Install state kept in the generated `package.json` under `piMarketplace`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MarketplaceState {
    pub name: String,
    pub tag: String,
    pub pinned: bool,
    pub target: String,
    pub binary: String,
    pub sha256: String,
    pub version: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InstalledPackage {
    pub name: String,
    pub tag: String,
    pub version: String,
    /// Installed from `marketplace:<name>@<tag>`; updates skip it.
    pub pinned: bool,
    pub target: String,
    pub binary_path: PathBuf,
    /// The package directory that holds the generated `package.json`.
    pub path: PathBuf,
}

/// A verified index joined with the local install state.
#[derive(Clone, Debug)]
pub struct MarketplaceCatalog {
    pub index: MarketplaceIndex,
    pub installed: Vec<InstalledPackage>,
    pub target: Option<&'static str>,
}

impl MarketplaceCatalog {
    pub fn installed(&self, name: &str) -> Option<&InstalledPackage> {
        self.installed.iter().find(|package| package.name == name)
    }
}

pub struct Marketplace {
    pub(crate) agent_dir: PathBuf,
    pub(crate) http: reqwest::Client,
    pub(crate) offline: bool,
    base_url: String,
    custom_base: bool,
    public_key: Vec<u8>,
}

impl Marketplace {
    /// `PI_MARKETPLACE_URL` replaces the release asset base; the signature check still applies.
    pub fn new(agent_dir: PathBuf, http: reqwest::Client, offline: bool) -> Self {
        let custom = pi_ai::env::process_env_value(MARKETPLACE_URL_ENV).filter(|value| !value.trim().is_empty());
        Self {
            agent_dir: crate::resources::paths::absolute(&agent_dir),
            http,
            offline,
            custom_base: custom.is_some(),
            base_url: custom.unwrap_or_else(|| DEFAULT_MARKETPLACE_URL.to_string()).trim_end_matches('/').to_string(),
            public_key: base64::engine::general_purpose::STANDARD.decode(MARKETPLACE_PUBLIC_KEY).unwrap_or_default(),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_public_key(mut self, key: [u8; 32]) -> Self {
        self.public_key = key.to_vec();
        self
    }

    #[cfg(test)]
    pub(crate) fn with_base_url(mut self, url: &str) -> Self {
        self.base_url = url.trim_end_matches('/').to_string();
        self.custom_base = true;
        self
    }

    pub(crate) fn root(&self) -> PathBuf {
        self.agent_dir.join("marketplace")
    }

    pub(crate) fn bin_root(&self) -> PathBuf {
        self.root().join("bin")
    }

    pub(crate) fn binary_path(&self, tag: &str, target: &str, binary: &str) -> PathBuf {
        let file = if is_windows_target(target) { format!("{binary}.exe") } else { binary.to_string() };
        self.bin_root().join(tag).join(target).join(file)
    }

    /// The URL of `index.json` for `tag`, or for the latest release.
    pub fn index_url(&self, tag: Option<&str>) -> String {
        let base = &self.base_url;
        match (tag, self.custom_base) {
            (Some(tag), true) => format!("{base}/{tag}/index.json"),
            (Some(tag), false) => format!("{base}/download/{tag}/index.json"),
            (None, true) => format!("{base}/index.json"),
            (None, false) => format!("{base}/latest/download/index.json"),
        }
    }

    /// Download, verify, and check the index of `tag`, or of the latest release.
    pub async fn fetch_index(&self, tag: Option<&str>) -> anyhow::Result<MarketplaceIndex> {
        if self.offline {
            bail!("Cannot reach the marketplace in offline mode");
        }
        if let Some(tag) = tag {
            validate_tag(tag)?;
        }
        let url = self.index_url(tag);
        let body = download_limited(&self.http, &url, MAX_INDEX_BYTES).await?;
        let signature = download_limited(&self.http, &format!("{url}.sig"), MAX_SIGNATURE_BYTES).await?;
        verify_signature(&self.public_key, &body, &signature)?;
        parse_index(&body, &url, tag)
    }

    /// The verified index of `tag` (or the latest release) with the local install state.
    pub async fn browse(&self, tag: Option<&str>) -> anyhow::Result<MarketplaceCatalog> {
        let index = self.fetch_index(tag).await?;
        Ok(MarketplaceCatalog { index, installed: self.installed()?, target: host_target() })
    }

    /// `<agentDir>/marketplace/packages/<name>`, whether or not it is installed.
    pub fn package_path(&self, name: &str) -> anyhow::Result<PathBuf> {
        validate_name("extension", name)?;
        Ok(self.root().join("packages").join(name))
    }

    pub(crate) fn read_state(&self, name: &str) -> anyhow::Result<Option<MarketplaceState>> {
        let manifest = self.package_path(name)?.join("package.json");
        let text = match std::fs::read_to_string(&manifest) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error).with_context(|| format!("Could not read {}", manifest.display())),
        };
        let value: serde_json::Value =
            serde_json::from_str(&text).with_context(|| format!("Could not read {}", manifest.display()))?;
        let Some(state) = value.get("piMarketplace") else { return Ok(None) };
        let state: MarketplaceState = serde_json::from_value(state.clone())
            .with_context(|| format!("Marketplace state in {} is not valid", manifest.display()))?;
        if state.name != name {
            bail!("Marketplace state in {} names {}", manifest.display(), state.name);
        }
        validate_tag(&state.tag)?;
        validate_target(&state.target)?;
        validate_name("binary", &state.binary)?;
        Ok(Some(state))
    }

    pub(crate) fn to_installed(&self, state: MarketplaceState) -> anyhow::Result<InstalledPackage> {
        Ok(InstalledPackage {
            path: self.package_path(&state.name)?,
            binary_path: self.binary_path(&state.tag, &state.target, &state.binary),
            name: state.name,
            tag: state.tag,
            version: state.version,
            pinned: state.pinned,
            target: state.target,
        })
    }

    /// The installed package, or `None` when its manifest or binary is missing.
    pub fn installed_package(&self, name: &str) -> anyhow::Result<Option<InstalledPackage>> {
        let Some(state) = self.read_state(name)? else { return Ok(None) };
        let package = self.to_installed(state)?;
        let complete = std::fs::symlink_metadata(&package.binary_path).is_ok_and(|meta| meta.is_file());
        Ok(complete.then_some(package))
    }

    pub fn installed_tag(&self, name: &str) -> anyhow::Result<Option<String>> {
        Ok(self.installed_package(name)?.map(|package| package.tag))
    }

    /// Installed marketplace packages, sorted by name. Entries that are not valid are skipped.
    pub fn installed(&self) -> anyhow::Result<Vec<InstalledPackage>> {
        let Ok(entries) = std::fs::read_dir(self.root().join("packages")) else { return Ok(Vec::new()) };
        let mut packages: Vec<InstalledPackage> = entries
            .flatten()
            .filter_map(|entry| entry.file_name().into_string().ok())
            .filter_map(|name| self.installed_package(&name).ok().flatten())
            .collect();
        packages.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(packages)
    }

    /// Install `name` from `tag` (pinned) or from the latest release.
    pub async fn install(&self, name: &str, tag: Option<&str>) -> anyhow::Result<PathBuf> {
        validate_name("extension", name)?;
        let index = self.fetch_index(tag).await?;
        self.install_from_index(&index, name, tag.is_some(), None).await
    }

    /// Move an unpinned package to a newer latest release. Offline mode and an older latest
    /// release are errors; pinned packages keep the installed release.
    pub async fn update(&self, name: &str) -> anyhow::Result<UpdateOutcome> {
        let Some(state) = self.read_state(name)? else {
            bail!("Marketplace extension {name} is not installed");
        };
        let path = self.package_path(name)?;
        if self.offline {
            bail!("Cannot update marketplace extension {name} in offline mode");
        }
        if state.pinned {
            return Ok(UpdateOutcome::Pinned { tag: state.tag, path });
        }
        let index = self.fetch_index(None).await?;
        match compare_tags(&index.tag, &state.tag) {
            Some(std::cmp::Ordering::Greater) => {
                Ok(UpdateOutcome::Updated(self.install_from_index(&index, name, false, Some(state)).await?))
            }
            Some(std::cmp::Ordering::Less) => bail!(
                "Marketplace release {} is older than the installed {} of {name}; the update was refused",
                index.tag,
                state.tag
            ),
            _ => Ok(UpdateOutcome::UpToDate(path)),
        }
    }

    /// True when an unpinned installed package has a newer release.
    pub async fn has_update(&self, name: &str) -> anyhow::Result<bool> {
        let Some(state) = self.read_state(name)? else { return Ok(false) };
        if state.pinned || self.offline {
            return Ok(false);
        }
        let index = self.fetch_index(None).await?;
        let available = index.entry(name).is_some_and(|entry| {
            entry.platforms.contains(&state.target) && index.asset(entry, &state.target).is_some()
        });
        Ok(available && is_newer_tag(&index.tag, &state.tag))
    }

    /// Delete the package directory and the binaries that no other package uses.
    pub fn remove(&self, name: &str) -> anyhow::Result<()> {
        let path = self.package_path(name)?;
        let _guard = super::marketplace_install::lock();
        remove_path(&path)?;
        self.prune_binaries();
        Ok(())
    }
}

/// The result of [`Marketplace::update`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UpdateOutcome {
    Updated(PathBuf),
    UpToDate(PathBuf),
    Pinned { tag: String, path: PathBuf },
}

fn tag_version(tag: &str) -> Option<semver::Version> {
    semver::Version::parse(tag.strip_prefix('v').unwrap_or(tag)).ok()
}

/// Release tags compared as semver without the leading `v`; `None` when either does not parse.
pub fn compare_tags(tag: &str, installed: &str) -> Option<std::cmp::Ordering> {
    Some(tag_version(tag)?.cmp(&tag_version(installed)?))
}

pub fn is_newer_tag(tag: &str, installed: &str) -> bool {
    compare_tags(tag, installed) == Some(std::cmp::Ordering::Greater)
}

/// Remove a file, symlink, or directory without following symlinks.
pub(crate) fn remove_path(path: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(path),
        Ok(_) => std::fs::remove_file(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}
