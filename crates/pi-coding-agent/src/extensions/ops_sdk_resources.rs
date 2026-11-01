//! `resources.*`: upstream `DefaultResourceLoader` for SDK use. Rust holds each loader, its paths
//! and its stored layers; `sdk_resources.js` runs each override hook between `load` and `set`.

mod kinds;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use pi_js::{HostValue, JsError};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use super::ops::{Args, error};
use super::runner::ExtensionRunner;
use crate::agent_session::lock;
use crate::agent_session::services::{ResourceOptions, resource_loader_options};
use crate::packages::{PackageManager, PackageManagerOptions};
use crate::resources::paths::resolve_path;
use crate::resources::{
    ExtensionResourcePaths, PathMetadata, ResolvedPaths, ResourceLoader, ResourceLoaderOptions, ResourceType,
};
use crate::settings::{SettingsManager, SettingsScope};

struct Entry {
    loader: ResourceLoader,
    home: PathBuf,
    settings: Option<Arc<SettingsManager>>,
}

#[derive(Default)]
pub(crate) struct ResourcesState {
    loaders: HashMap<u64, Entry>,
    next_id: u64,
}

impl ResourcesState {
    fn entry(&mut self, id: u64) -> Result<&mut Entry, JsError> {
        self.loaders.get_mut(&id).ok_or_else(|| error("DefaultResourceLoader is not registered"))
    }
}

impl Entry {
    fn settings(&self) -> Result<Arc<SettingsManager>, JsError> {
        self.settings.clone().ok_or_else(|| error("DefaultResourceLoader has no SettingsManager"))
    }
}

#[derive(Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct LoaderData {
    additional_skill_paths: Vec<String>,
    additional_prompt_template_paths: Vec<String>,
    additional_theme_paths: Vec<String>,
    no_extensions: bool,
    disabled_builtin_extensions: Vec<String>,
    no_skills: bool,
    no_prompt_templates: bool,
    no_themes: bool,
    no_context_files: bool,
    system_prompt: Option<String>,
    append_system_prompt: Option<Vec<String>>,
}

#[derive(Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct ExtendPaths {
    skill_paths: Vec<ExtendEntry>,
    prompt_paths: Vec<ExtendEntry>,
    theme_paths: Vec<ExtendEntry>,
}

#[derive(Deserialize)]
struct ExtendEntry {
    path: String,
    metadata: PathMetadata,
}

fn typed<T: DeserializeOwned>(value: Value, member: &str) -> Result<T, JsError> {
    serde_json::from_value(value).map_err(|failure| JsError::type_error(format!("{member}: {failure}")))
}

fn expand_home(raw: &str, home: &Path) -> String {
    match raw.trim().strip_prefix('~') {
        Some("") => home.display().to_string(),
        Some(rest) if rest.starts_with('/') || (cfg!(windows) && rest.starts_with('\\')) => {
            home.join(&rest[1..]).display().to_string()
        }
        _ => raw.to_string(),
    }
}

fn loader_options(entry: &Entry, settings: &SettingsManager, data: &LoaderData) -> ResourceLoaderOptions {
    let expand = |paths: &[String]| paths.iter().map(|path| expand_home(path, &entry.home)).collect();
    let resources = ResourceOptions {
        skill_paths: expand(&data.additional_skill_paths),
        prompt_template_paths: expand(&data.additional_prompt_template_paths),
        theme_paths: expand(&data.additional_theme_paths),
        no_extensions: data.no_extensions,
        no_skills: data.no_skills,
        no_prompt_templates: data.no_prompt_templates,
        no_themes: data.no_themes,
        no_context_files: data.no_context_files,
        system_prompt: data.system_prompt.clone(),
        append_system_prompt: data.append_system_prompt.clone(),
        disabled_builtin_extensions: data.disabled_builtin_extensions.clone(),
        home: Some(entry.home.clone()),
        ..ResourceOptions::default()
    };
    let current = &entry.loader.options;
    let mut options = resource_loader_options(&current.cwd, &current.agent_dir, settings, &resources);
    options.builtin_extensions.clear();
    options
}

fn extend(entry: &mut Entry, paths: Value) -> Result<Value, JsError> {
    let paths: ExtendPaths = typed(paths, "DefaultResourceLoader.extendResources")?;
    let home = &entry.home;
    let entries = |list: Vec<ExtendEntry>| -> Vec<(PathBuf, PathMetadata)> {
        list.into_iter().map(|item| (PathBuf::from(expand_home(&item.path, home)), item.metadata)).collect()
    };
    let paths = ExtensionResourcePaths {
        skill_paths: entries(paths.skill_paths),
        prompt_paths: entries(paths.prompt_paths),
        theme_paths: entries(paths.theme_paths),
    };
    let kinds = entry.loader.extend_paths(paths).into_iter().map(|kind| match kind {
        ResourceType::Skills => "skills",
        ResourceType::Prompts => "prompts",
        _ => "themes",
    });
    Ok(json!(kinds.collect::<Vec<_>>()))
}

pub(crate) fn sync(runner: &ExtensionRunner, name: &str, args: &Args) -> Result<HostValue, JsError> {
    if name == "create" {
        let (home, base) = (PathBuf::from(args.str(2)?), PathBuf::from(args.str(3)?));
        let cwd = resolve_path(&expand_home(&args.str(0)?, &home), &base);
        let agent_dir = resolve_path(&expand_home(&args.str(1)?, &home), &base);
        let loader = ResourceLoader::new(ResourceLoaderOptions::new(&cwd, &agent_dir));
        let mut state = lock(&runner.resources);
        state.next_id += 1;
        let id = state.next_id;
        state.loaders.insert(id, Entry { loader, home, settings: None });
        let (cwd, agent_dir) = (cwd.display().to_string(), agent_dir.display().to_string());
        return Ok(HostValue::Json(json!({"id": id, "cwd": cwd, "agentDir": agent_dir})));
    }
    if name == "configure" {
        let settings = lock(&runner.sdk.storage).settings(args.u64(1));
        let settings = settings.ok_or_else(|| error("SettingsManager has been disposed"))?;
        let data: LoaderData = typed(args.json(2), "DefaultResourceLoader")?;
        let mut state = lock(&runner.resources);
        let entry = state.entry(args.u64(0))?;
        entry.loader.options = loader_options(entry, &settings, &data);
        entry.settings = Some(settings);
        return Ok(HostValue::Json(Value::Null));
    }
    let mut state = lock(&runner.resources);
    let entry = state.entry(args.u64(0))?;
    let value = match name {
        "load" => kinds::load(entry, &args.str(1)?)?,
        "set" => kinds::set(entry, &args.str(1)?, args.json(2), args.bool(3), args.json(4))?,
        "extend" => extend(entry, args.json(1))?,
        other => return Err(error(format!("Unknown host operation: resources.{other}"))),
    };
    Ok(HostValue::Json(value))
}

/// Upstream `packageManager.resolve()` and the path stage; JavaScript then loads each kind.
pub(crate) async fn reload(runner: Arc<ExtensionRunner>, args: Args) -> Result<HostValue, JsError> {
    let id = args.u64(0);
    let (cwd, agent_dir, settings) = {
        let mut state = lock(&runner.resources);
        let entry = state.entry(id)?;
        (entry.loader.options.cwd.clone(), entry.loader.options.agent_dir.clone(), entry.settings()?)
    };
    let scopes = [SettingsScope::Global, SettingsScope::Project];
    let (packages, warnings) = if scopes.iter().all(|scope| settings.packages(*scope).is_empty()) {
        (ResolvedPaths::default(), Vec::new())
    } else {
        let manager = PackageManager::new(PackageManagerOptions::new(&cwd, &agent_dir, settings.clone()));
        let resolved =
            manager.resolve_package_resources(None).await.map_err(|failure| error(format!("{failure:#}")))?;
        let warnings = manager.take_install_failures().into_iter().map(|failure| {
            let (text, prefix) = (failure.warning(), format!("Extension package \"{}\": ", failure.source));
            json!({"path": failure.source, "warning": text.strip_prefix(&prefix).unwrap_or(&text)})
        });
        (resolved, warnings.collect())
    };
    let has_packages = packages != ResolvedPaths::default();
    let mut state = lock(&runner.resources);
    let entry = state.entry(id)?;
    entry.loader.options.package_resources = packages;
    entry.loader.resolve_paths();
    let extensions = entry.loader.resources().extensions.len();
    Ok(HostValue::Json(json!({"warnings": warnings, "packages": has_packages, "extensions": extensions})))
}
