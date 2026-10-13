//! The `pi` key of a package's `package.json` (upstream `core/pi-manifest.ts`), gallery
//! metadata, and the host-provided dependency rules of `docs/packages.md`.

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::resources::{ResolvedResource, ResourceType};

/// Resource entries a package declares under `pi` in `package.json`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PiManifest {
    pub extensions: Option<Vec<String>>,
    pub skills: Option<Vec<String>>,
    pub prompts: Option<Vec<String>>,
    pub themes: Option<Vec<String>>,
    /// Gallery preview image and video.
    pub image: Option<String>,
    pub video: Option<String>,
}

impl PiManifest {
    pub fn entries(&self, kind: ResourceType) -> Option<&Vec<String>> {
        match kind {
            ResourceType::Extensions => self.extensions.as_ref(),
            ResourceType::Skills => self.skills.as_ref(),
            ResourceType::Prompts => self.prompts.as_ref(),
            ResourceType::Themes => self.themes.as_ref(),
        }
    }
}

/// The keyword that lists an npm package in the pi package gallery.
pub const GALLERY_KEYWORD: &str = "pi-package";

/// Packages pi supplies to extensions; they belong in `peerDependencies` with `"*"`.
pub const HOST_PROVIDED_EXTENSION_PACKAGES: [&str; 10] = [
    "@earendil-works/pi-agent-core",
    "@earendil-works/pi-ai",
    "@earendil-works/pi-coding-agent",
    "@earendil-works/pi-tui",
    "@mariozechner/pi-agent-core",
    "@mariozechner/pi-ai",
    "@mariozechner/pi-coding-agent",
    "@mariozechner/pi-tui",
    "@sinclair/typebox",
    "typebox",
];

/// Parse a `package.json`, ignoring a byte order mark. `None` when missing or invalid.
pub fn read_package_json(path: &Path) -> Option<Value> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(text.trim_start_matches('\u{feff}')).ok()
}

fn string_list(value: Option<&Value>) -> Option<Vec<String>> {
    let items = value?.as_array()?;
    items.iter().map(|item| item.as_str().map(String::from)).collect()
}

/// The `pi` manifest of a `package.json`; `None` when the file or the `pi` object is missing.
/// A resource list counts only when it is an array of strings.
pub fn read_pi_manifest(package_json: &Path) -> Option<PiManifest> {
    let package = read_package_json(package_json)?;
    let pi = package.as_object()?.get("pi")?.as_object()?;
    let text = |key: &str| pi.get(key).and_then(Value::as_str).map(String::from);
    Some(PiManifest {
        extensions: string_list(pi.get("extensions")),
        skills: string_list(pi.get("skills")),
        prompts: string_list(pi.get("prompts")),
        themes: string_list(pi.get("themes")),
        image: text("image"),
        video: text("video"),
    })
}

/// The `version` of the `package.json` in `dir`.
pub fn installed_version(dir: &Path) -> Option<String> {
    let package = read_package_json(&dir.join("package.json"))?;
    package.get("version")?.as_str().map(String::from)
}

/// Gallery listing data of a package: it is listed when its keywords include `pi-package`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GalleryInfo {
    pub name: Option<String>,
    pub description: Option<String>,
    pub listed: bool,
    pub image: Option<String>,
    pub video: Option<String>,
}

pub fn gallery_info(package: &Value) -> GalleryInfo {
    let text = |key: &str| package.get(key).and_then(Value::as_str).map(String::from);
    let keywords = string_list(package.get("keywords")).unwrap_or_default();
    let pi = package.get("pi");
    let media = |key: &str| pi.and_then(|pi| pi.get(key)).and_then(Value::as_str).map(String::from);
    GalleryInfo {
        name: text("name"),
        description: text("description"),
        listed: keywords.iter().any(|keyword| keyword == GALLERY_KEYWORD),
        image: media("image"),
        video: media("video"),
    }
}

fn dependency_names(package: &Value, key: &str) -> Vec<String> {
    package.get(key).and_then(Value::as_object).map(|deps| deps.keys().cloned().collect()).unwrap_or_default()
}

/// The warning for host-provided packages listed in `dependencies` of `package_json`.
pub fn host_dependency_warning(package_json: &Path) -> Option<String> {
    let package = read_package_json(package_json)?;
    let mut host: Vec<String> = dependency_names(&package, "dependencies")
        .into_iter()
        .filter(|name| HOST_PROVIDED_EXTENSION_PACKAGES.contains(&name.as_str()))
        .collect();
    if host.is_empty() {
        return None;
    }
    host.sort();
    Some(format!(
        "Host-provided extension packages must be declared in peerDependencies with a \"*\" range, not dependencies: {}. Installed copies can bypass the extension loader and create duplicate runtime modules.",
        host.join(", ")
    ))
}

/// Extension warnings for every package root among `extensions` (upstream
/// `collectExtensionPackageWarnings`): `(package.json path, warning)`.
pub fn collect_extension_package_warnings(extensions: &[ResolvedResource]) -> Vec<(PathBuf, String)> {
    let mut roots: Vec<&PathBuf> =
        extensions.iter().filter_map(|resource| resource.metadata.package_root.as_ref()).collect();
    roots.dedup();
    let mut seen = std::collections::HashSet::new();
    roots
        .into_iter()
        .filter(|root| seen.insert(root.to_path_buf()))
        .filter_map(|root| {
            let path = root.join("package.json");
            host_dependency_warning(&path).map(|warning| (path, warning))
        })
        .collect()
}

/// Whether `dir/package.json` declares runtime `dependencies` that are absent from
/// `dir/node_modules` (upstream `hasMissingGitDependencies`).
pub fn has_missing_dependencies(dir: &Path) -> bool {
    let Some(package) = read_package_json(&dir.join("package.json")) else { return false };
    let modules = crate::resources::paths::normalize(&dir.join("node_modules"));
    dependency_names(&package, "dependencies").iter().any(|name| {
        let path = crate::resources::paths::normalize(&modules.join(name));
        path != modules && path.starts_with(&modules) && !path.exists()
    })
}

/// Whether `dir/package.json` declares any runtime `dependencies`.
pub fn has_dependencies(dir: &Path) -> bool {
    read_package_json(&dir.join("package.json"))
        .is_some_and(|package| !dependency_names(&package, "dependencies").is_empty())
}
