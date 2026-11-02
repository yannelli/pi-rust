//! `resources.*`: upstream `DefaultResourceLoader` for SDK use. Rust holds each loader, its paths
//! and its stored layers; `sdk_resources.js` runs each override hook between `load` and `set`.
//! A child session shares the loader, and binds the extensions its last reload loaded.

mod extensions;
mod kinds;
mod mirror;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};

use pi_js::{HostValue, JsError};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

pub(crate) use self::extensions::{Hosted, ParentLink};
pub(crate) use self::mirror::{child_sync, forward};
use super::ops::{Args, error};
use super::runner::ExtensionRunner;
use crate::agent_session::lock;
use crate::agent_session::services::{ResourceOptions, resource_loader_options};
use crate::packages::{PackageManager, PackageManagerOptions};
use crate::resources::paths::{is_local_path, resolve_path};
use crate::resources::{
    ExtensionResourcePaths, PathMetadata, ResolvedPaths, ResourceLoader, ResourceLoaderOptions, ResourceType,
};
use crate::settings::{SettingsManager, SettingsScope};

struct Entry {
    loader: Arc<Mutex<ResourceLoader>>,
    home: PathBuf,
    settings: Option<Arc<SettingsManager>>,
    loaded: extensions::Loaded,
}

#[derive(Default)]
pub(crate) struct ResourcesState {
    loaders: HashMap<u64, Entry>,
    next_id: u64,
    /// The child host of each factory group, while it lives.
    hosts: HashMap<u64, Weak<ExtensionRunner>>,
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
    additional_extension_paths: Vec<String>,
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

fn loader_options(
    home: &Path,
    current: &ResourceLoaderOptions,
    settings: &SettingsManager,
    data: &LoaderData,
) -> ResourceLoaderOptions {
    let expand = |paths: &[String]| paths.iter().map(|path| expand_home(path, home)).collect();
    let resources = ResourceOptions {
        extension_sources: expand(&data.additional_extension_paths),
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
        home: Some(home.to_path_buf()),
        ..ResourceOptions::default()
    };
    let mut options = resource_loader_options(&current.cwd, &current.agent_dir, settings, &resources);
    options.builtin_extensions.clear();
    options
}

fn extend(loader: &mut ResourceLoader, home: &Path, paths: Value) -> Result<Value, JsError> {
    let paths: ExtendPaths = typed(paths, "DefaultResourceLoader.extendResources")?;
    let entries = |list: Vec<ExtendEntry>| -> Vec<(PathBuf, PathMetadata)> {
        list.into_iter().map(|item| (PathBuf::from(expand_home(&item.path, home)), item.metadata)).collect()
    };
    let paths = ExtensionResourcePaths {
        skill_paths: entries(paths.skill_paths),
        prompt_paths: entries(paths.prompt_paths),
        theme_paths: entries(paths.theme_paths),
    };
    let kinds = loader.extend_paths(paths).into_iter().map(|kind| match kind {
        ResourceType::Skills => "skills",
        ResourceType::Prompts => "prompts",
        _ => "themes",
    });
    Ok(json!(kinds.collect::<Vec<_>>()))
}

pub(crate) fn sync(runner: &ExtensionRunner, name: &str, args: &Args) -> Result<HostValue, JsError> {
    let value = match name {
        "create" => {
            let (home, base) = (PathBuf::from(args.str(2)?), PathBuf::from(args.str(3)?));
            let cwd = resolve_path(&expand_home(&args.str(0)?, &home), &base);
            let agent_dir = resolve_path(&expand_home(&args.str(1)?, &home), &base);
            let loader = Arc::new(Mutex::new(ResourceLoader::new(ResourceLoaderOptions::new(&cwd, &agent_dir))));
            let mut state = lock(&runner.resources);
            state.next_id += 1;
            let id = state.next_id;
            state.loaders.insert(id, Entry { loader, home, settings: None, loaded: Default::default() });
            json!({"id": id, "cwd": cwd.display().to_string(), "agentDir": agent_dir.display().to_string()})
        }
        "configure" => {
            let settings = lock(&runner.sdk.storage).settings(args.u64(1));
            let settings = settings.ok_or_else(|| error("SettingsManager has been disposed"))?;
            let data: LoaderData = typed(args.json(2), "DefaultResourceLoader")?;
            if data.additional_extension_paths.iter().any(|path| !is_local_path(path)) {
                return Err(JsError::unsupported("DefaultResourceLoader.additionalExtensionPaths package sources"));
            }
            let (loader, home) = {
                let mut state = lock(&runner.resources);
                let entry = state.entry(args.u64(0))?;
                entry.settings = Some(settings.clone());
                (entry.loader.clone(), entry.home.clone())
            };
            let mut loader = lock(&*loader);
            let options = loader_options(&home, &loader.options, &settings, &data);
            loader.options = options;
            Value::Null
        }
        "mirror" => mirror::store(runner, args)?,
        "deliver" => mirror::deliver(runner, args),
        "models" => mirror::group_models(runner, args)?,
        _ => {
            let (loader, settings, home) = {
                let mut state = lock(&runner.resources);
                let entry = state.entry(args.u64(0))?;
                (entry.loader.clone(), entry.settings(), entry.home.clone())
            };
            let mut loader = lock(&*loader);
            match name {
                "load" => kinds::load(&mut loader, settings?.as_ref(), &args.str(1)?)?,
                "set" => kinds::set(&mut loader, &args.str(1)?, args.json(2), args.bool(3), args.json(4))?,
                "extend" => extend(&mut loader, &home, args.json(1))?,
                other => return Err(error(format!("Unknown host operation: resources.{other}"))),
            }
        }
    };
    Ok(HostValue::Json(value))
}

pub(crate) type SessionResources = (Arc<Mutex<ResourceLoader>>, Option<(Hosted, u64)>);

/// The loader and the extensions a new child session binds, with the parent's target for it.
pub(crate) fn take(runner: &ExtensionRunner, id: u64) -> Result<SessionResources, JsError> {
    let mut state = lock(&runner.resources);
    let entry = state.entry(id)?;
    let hosted = extensions::take(runner, &mut entry.loaded)?;
    Ok((entry.loader.clone(), hosted))
}

/// End every child host of `runner`'s loaders.
pub(crate) fn shutdown(runner: &ExtensionRunner) {
    let loaders = std::mem::take(&mut lock(&runner.resources).loaders);
    drop(loaders);
}

pub(crate) async fn run(runner: Arc<ExtensionRunner>, name: &str, args: Args) -> Result<HostValue, JsError> {
    let value = match name {
        "reload" => reload(&runner, &args).await?,
        "finish" => extensions::finish(&runner, args.u64(0)).await?,
        "order" => extensions::order(&runner, args.u64(0), args.json(1)).await?,
        "busReady" => mirror::bus_ready(&runner, args.u64(0)).await?,
        other => return Err(error(format!("Unknown host operation: resources.{other}"))),
    };
    Ok(HostValue::Json(value))
}

/// Upstream `packageManager.resolve()`, the path stage, and the extension files of one phase;
/// JavaScript then runs the factories and loads each resource kind.
async fn reload(runner: &Arc<ExtensionRunner>, args: &Args) -> Result<Value, JsError> {
    let id = args.u64(0);
    let (loader, settings) = {
        let mut state = lock(&runner.resources);
        let entry = state.entry(id)?;
        (entry.loader.clone(), entry.settings()?)
    };
    let (cwd, agent_dir) = {
        let loader = lock(&*loader);
        (loader.options.cwd.clone(), loader.options.agent_dir.clone())
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
        (resolved, warnings.collect::<Vec<_>>())
    };
    let has_packages = packages != ResolvedPaths::default();
    let (entries, missing) = {
        let mut loader = lock(&*loader);
        loader.options.package_resources = packages;
        loader.resolve_paths();
        let resources = loader.resources();
        let missing = resources
            .extension_diagnostics
            .iter()
            .map(|diagnostic| json!({"path": diagnostic.path, "error": diagnostic.message}));
        (super::loader::load_entries(&resources.extensions), missing.collect::<Vec<_>>())
    };
    let phase = args.str(1)?;
    let reload = extensions::Reload { id, loader, settings, cwd, agent_dir, entries, warnings };
    let mut loaded = extensions::load(runner, reload, &phase, args.bool(2)).await?;
    loaded["packages"] = json!(has_packages);
    loaded["missing"] = json!(if phase == "pretrust" { Vec::new() } else { missing });
    Ok(loaded)
}
