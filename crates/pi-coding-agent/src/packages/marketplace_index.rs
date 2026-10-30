//! Signed marketplace index: download `index.json` and `index.json.sig`, verify the Ed25519
//! signature over the exact bytes, then check the schema, protocol, and pi version.

use std::collections::{BTreeMap, HashSet};

use anyhow::{Context, anyhow, bail};
use base64::Engine as _;
use serde::{Deserialize, Serialize};

pub const SUPPORTED_SCHEMA_VERSION: u64 = 1;
pub const SUPPORTED_PROTOCOL_VERSION: u64 = 1;
pub const MARKETPLACE_TARGETS: [&str; 5] = [
    "aarch64-apple-darwin",
    "x86_64-apple-darwin",
    "x86_64-unknown-linux-musl",
    "aarch64-unknown-linux-musl",
    "x86_64-pc-windows-msvc",
];
pub(crate) const MAX_INDEX_BYTES: u64 = 4 * 1024 * 1024;
pub(crate) const MAX_SIGNATURE_BYTES: u64 = 1024;
const MAX_NAME_LEN: usize = 64;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BinaryAsset {
    pub url: String,
    pub sha256: String,
    pub size: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MarketplaceEntry {
    pub name: String,
    #[serde(default)]
    pub display_name: String,
    #[serde(default)]
    pub description: String,
    pub version: String,
    pub binary: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub platforms: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MarketplaceIndex {
    pub schema_version: u64,
    pub repository: String,
    pub tag: String,
    pub protocol_version: u64,
    pub min_pi_version: String,
    /// Binary name, then target, then the release asset.
    pub binaries: BTreeMap<String, BTreeMap<String, BinaryAsset>>,
    pub extensions: Vec<MarketplaceEntry>,
    /// URL of `index.json`; relative binary URLs resolve against it.
    #[serde(skip)]
    pub url: String,
}

impl MarketplaceIndex {
    pub fn entry(&self, name: &str) -> Option<&MarketplaceEntry> {
        self.extensions.iter().find(|entry| entry.name == name)
    }

    pub fn asset(&self, entry: &MarketplaceEntry, target: &str) -> Option<&BinaryAsset> {
        self.binaries.get(&entry.binary)?.get(target)
    }

    /// Absolute `http` or `https` URL of an asset.
    pub fn asset_url(&self, asset: &BinaryAsset) -> anyhow::Result<url::Url> {
        let base =
            url::Url::parse(&self.url).with_context(|| format!("Marketplace index URL {} is not valid", self.url))?;
        let url = base.join(&asset.url).with_context(|| format!("Binary URL {} is not valid", asset.url))?;
        if !matches!(url.scheme(), "http" | "https") {
            bail!("Binary URL {url} must use http or https");
        }
        Ok(url)
    }
}

/// The marketplace target of this machine, or `None` when no release binary exists for it.
pub fn host_target() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Some("aarch64-apple-darwin"),
        ("macos", "x86_64") => Some("x86_64-apple-darwin"),
        ("linux", "x86_64") => Some("x86_64-unknown-linux-musl"),
        ("linux", "aarch64") => Some("aarch64-unknown-linux-musl"),
        ("windows", "x86_64") => Some("x86_64-pc-windows-msvc"),
        _ => None,
    }
}

pub(crate) fn is_windows_target(target: &str) -> bool {
    target.contains("-windows-")
}

fn check_name(kind: &str, value: &str, extra: &[char]) -> anyhow::Result<()> {
    let mut chars = value.chars();
    let first_ok = chars.next().is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
    let rest_ok = chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || extra.contains(&c));
    if !first_ok || !rest_ok || value.len() > MAX_NAME_LEN || value.contains("..") {
        bail!("Marketplace {kind} \"{value}\" is not valid");
    }
    Ok(())
}

/// Extension and binary names: lowercase letters, digits, `-`, `_`, and `.`.
pub fn validate_name(kind: &str, value: &str) -> anyhow::Result<()> {
    check_name(kind, value, &['_', '.'])
}

/// Release tags such as `v0.1.0`: letters, digits, `-`, `_`, `.`, and `+`.
pub fn validate_tag(tag: &str) -> anyhow::Result<()> {
    let ok = tag.len() <= MAX_NAME_LEN
        && !tag.contains("..")
        && tag.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
        && tag.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '+'));
    if !ok {
        bail!("Marketplace tag \"{tag}\" is not valid");
    }
    Ok(())
}

pub(crate) fn validate_target(target: &str) -> anyhow::Result<()> {
    if !MARKETPLACE_TARGETS.contains(&target) {
        bail!("Marketplace target \"{target}\" is not supported");
    }
    Ok(())
}

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, bytes);
    digest.as_ref().iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Verify the base64 signature in `index.json.sig` against the exact index bytes.
pub(crate) fn verify_signature(public_key: &[u8], body: &[u8], signature: &[u8]) -> anyhow::Result<()> {
    let text: String = String::from_utf8_lossy(signature).chars().filter(|c| !c.is_ascii_whitespace()).collect();
    let raw = base64::engine::general_purpose::STANDARD
        .decode(text)
        .map_err(|_| anyhow!("Marketplace index signature is not valid base64"))?;
    ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, public_key)
        .verify(body, &raw)
        .map_err(|_| anyhow!("Marketplace index signature is not valid. Do not use this index."))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SchemaProbe {
    schema_version: u64,
}

/// Parse and check a verified index. `expected_tag` is the tag that the caller asked for.
pub(crate) fn parse_index(body: &[u8], url: &str, expected_tag: Option<&str>) -> anyhow::Result<MarketplaceIndex> {
    let probe: SchemaProbe = serde_json::from_slice(body).context("Marketplace index is not valid JSON")?;
    if probe.schema_version != SUPPORTED_SCHEMA_VERSION {
        bail!("Marketplace index schema {} is not supported. Update pi to use this marketplace.", probe.schema_version);
    }
    let mut index: MarketplaceIndex = serde_json::from_slice(body).context("Marketplace index is not valid")?;
    index.url = url.to_string();
    if index.protocol_version != SUPPORTED_PROTOCOL_VERSION {
        bail!(
            "Marketplace extensions use protocol {}. This pi supports protocol {SUPPORTED_PROTOCOL_VERSION}. Update pi.",
            index.protocol_version
        );
    }
    let minimum = semver::Version::parse(&index.min_pi_version)
        .with_context(|| format!("Marketplace minPiVersion \"{}\" is not valid", index.min_pi_version))?;
    let current = semver::Version::parse(crate::platform::VERSION)?;
    if current < minimum {
        bail!("Marketplace release {} needs pi {minimum} or later. This pi is {current}.", index.tag);
    }
    validate_tag(&index.tag)?;
    if let Some(expected) = expected_tag.filter(|expected| *expected != index.tag) {
        bail!("Marketplace index has tag {}, but tag {expected} was requested", index.tag);
    }
    for (binary, targets) in &index.binaries {
        validate_name("binary", binary)?;
        for asset in targets.values() {
            let hex = asset.sha256.len() == 64 && asset.sha256.chars().all(|c| c.is_ascii_hexdigit());
            if !hex || asset.size == 0 {
                bail!("Marketplace binary {binary} has a sha256 or size that is not valid");
            }
        }
    }
    let mut names = HashSet::new();
    for entry in &index.extensions {
        validate_name("extension", &entry.name)?;
        validate_name("binary", &entry.binary)?;
        if !names.insert(entry.name.as_str()) {
            bail!("Marketplace index lists extension {} more than once", entry.name);
        }
    }
    Ok(index)
}

/// GET `url` into memory, and stop when the body exceeds `limit` bytes.
pub(crate) async fn download_limited(http: &reqwest::Client, url: &str, limit: u64) -> anyhow::Result<Vec<u8>> {
    let mut response = http
        .get(url)
        .header(reqwest::header::USER_AGENT, crate::platform::pi_user_agent(crate::platform::VERSION))
        .send()
        .await
        .with_context(|| format!("Could not download {url}"))?;
    if !response.status().is_success() {
        bail!("Could not download {url}: HTTP {}", response.status().as_u16());
    }
    let too_large = || anyhow!("Download {url} is larger than {limit} bytes");
    if response.content_length().is_some_and(|length| length > limit) {
        return Err(too_large());
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.with_context(|| format!("Could not download {url}"))? {
        if (bytes.len() + chunk.len()) as u64 > limit {
            return Err(too_large());
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}
