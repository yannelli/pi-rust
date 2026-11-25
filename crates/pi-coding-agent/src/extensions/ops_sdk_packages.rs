//! `packages.*`: upstream `DefaultPackageManager` for SDK use (`js/sdk_packages.js`) over the Rust
//! package manager. Each call builds a manager from the options the JavaScript object keeps.

use std::path::Path;
use std::sync::Arc;

use futures::FutureExt;
use pi_js::{HostValue, JsError};
use serde_json::{Map, Value, json};

use super::ops::{Args, error};
use super::ops_session::block_on_fresh;
use super::runner::ExtensionRunner;
use crate::agent_session::lock;
use crate::packages::{
    MissingSourceAction, MissingSourceHandler, PackageManager, PackageManagerOptions, ParsedSource, ProgressEvent,
    parse_source,
};
use crate::resources::{ResolvedPaths, ResolvedResource, SourceScope};

fn manager(runner: &ExtensionRunner, options: &Value) -> Result<PackageManager, JsError> {
    let settings = lock(&runner.sdk.storage).settings(options["settingsManagerId"].as_u64().unwrap_or(0));
    let settings = settings.ok_or_else(|| error("SettingsManager has been disposed"))?;
    let (cwd, agent_dir) = (options["cwd"].as_str().unwrap_or("."), options["agentDir"].as_str().unwrap_or("."));
    let mut package_options = PackageManagerOptions::new(Path::new(cwd), Path::new(agent_dir), settings);
    package_options.builtin_extensions =
        serde_json::from_value(options["builtinExtensions"].clone()).unwrap_or_default();
    Ok(PackageManager::new(package_options))
}

fn scope(args: &Args, index: usize) -> Result<SourceScope, JsError> {
    match args.str(index)?.as_str() {
        "user" => Ok(SourceScope::User),
        "project" => Ok(SourceScope::Project),
        other => Err(error(format!("Invalid package scope: {other}"))),
    }
}

/// Upstream `ParsedSource` with the fields that are set; pi-rust adds `url` and `marketplace`.
fn parsed_json(source: &str) -> Value {
    let (kind, fields) = match parse_source(source) {
        ParsedSource::Npm(npm) => {
            let range = npm
                .version
                .as_deref()
                .filter(|_| npm.has_range)
                .and_then(|version| nodejs_semver::Range::parse(version).ok().map(|range| range.to_string()));
            let fields = json!({"spec": npm.spec, "name": npm.name, "version": npm.version, "range": range, "pinned": npm.pinned});
            ("npm", fields)
        }
        ParsedSource::Git(git) => {
            let fields =
                json!({"repo": git.repo, "host": git.host, "path": git.path, "ref": git.git_ref, "pinned": git.pinned});
            ("git", fields)
        }
        ParsedSource::Archive(archive) => {
            ("url", json!({"url": archive.url, "host": archive.host, "path": archive.path}))
        }
        ParsedSource::Marketplace { name, tag } => ("marketplace", json!({"name": name, "tag": tag})),
        ParsedSource::Local(local) => ("local", json!({"path": local.path})),
    };
    let mut parsed = Map::from_iter([("type".to_string(), json!(kind))]);
    parsed.extend(
        fields
            .as_object()
            .into_iter()
            .flatten()
            .filter(|(_, value)| !value.is_null())
            .map(|(k, v)| (k.clone(), v.clone())),
    );
    Value::Object(parsed)
}

fn resolved_json(paths: &ResolvedPaths) -> Value {
    let list = |items: &[ResolvedResource]| -> Vec<Value> {
        items
            .iter()
            .map(|item| json!({"path": item.path, "enabled": item.enabled, "metadata": item.metadata}))
            .collect()
    };
    json!({
        "extensions": list(&paths.extensions),
        "skills": list(&paths.skills),
        "prompts": list(&paths.prompts),
        "themes": list(&paths.themes),
    })
}

pub(crate) fn sync(runner: &ExtensionRunner, name: &str, args: &Args) -> Result<HostValue, JsError> {
    if name == "parseSource" {
        return Ok(HostValue::Json(parsed_json(&args.str(0)?)));
    }
    let manager = manager(runner, &args.json(0))?;
    let failed = |failure: anyhow::Error| error(format!("{failure:#}"));
    let value = match name {
        "list" => {
            let packages = block_on_fresh(manager.list_configured_packages()).unwrap_or_default();
            let packages = packages.into_iter().map(|package| {
                let mut entry = json!({"source": package.source, "scope": package.scope, "filtered": package.filtered});
                if let Some(path) = package.installed_path {
                    entry["installedPath"] = json!(path);
                }
                entry
            });
            json!(packages.collect::<Vec<_>>())
        }
        "installedPath" => {
            let (source, scope) = (args.str(1)?, scope(args, 2)?);
            json!(block_on_fresh(manager.installed_path(&source, scope)).flatten())
        }
        "addSource" => json!(manager.add_source_to_settings(&args.str(1)?, args.bool(2)).map_err(failed)?),
        "removeSource" => json!(manager.remove_source_from_settings(&args.str(1)?, args.bool(2)).map_err(failed)?),
        other => return Err(error(format!("Unknown host operation: packages.{other}"))),
    };
    Ok(HostValue::Json(value))
}

/// A JavaScript `onMissing` callback: `install`, `skip`, or `error` for each missing source.
fn missing_handler(runner: &ExtensionRunner, key: u64) -> MissingSourceHandler {
    let host = runner.host().clone();
    Arc::new(move |source: String| {
        let host = host.clone();
        async move {
            let answer = host.call_method("invokeCallback", vec![json!(key).into(), json!(source).into()]).await;
            match answer.map(HostValue::into_json).ok().as_ref().and_then(Value::as_str) {
                Some("install") => MissingSourceAction::Install,
                Some("skip") => MissingSourceAction::Skip,
                _ => MissingSourceAction::Error,
            }
        }
        .boxed()
    })
}

pub(crate) async fn run(runner: Arc<ExtensionRunner>, name: &str, args: Args) -> Result<HostValue, JsError> {
    let options = args.json(0);
    let manager = manager(&runner, &options)?;
    if let Some(key) = options["progress"].as_u64() {
        let host = runner.host().clone();
        manager.set_progress_callback(Some(Arc::new(move |event: &ProgressEvent| {
            let (host, event) = (host.clone(), json!(event));
            tokio::spawn(
                async move { host.call_method("invokeCallback", vec![json!(key).into(), event.into()]).await },
            );
        })));
    }
    let failed = |failure: anyhow::Error| error(format!("{failure:#}"));
    let value = match name {
        "resolve" => {
            let missing = args.json(1).as_u64().map(|key| missing_handler(&runner, key));
            resolved_json(&manager.resolve(missing.as_ref()).await.map_err(failed)?)
        }
        "resolveExtensionSources" => {
            let sources: Vec<String> = serde_json::from_value(args.json(1)).map_err(error)?;
            let flags = args.json(2);
            let local = flags["local"].as_bool().unwrap_or(false);
            let temporary = flags["temporary"].as_bool().unwrap_or(false);
            resolved_json(&manager.resolve_extension_sources(&sources, local, temporary).await.map_err(failed)?)
        }
        "install" => json!(manager.install(&args.str(1)?, args.bool(2)).await.map_err(failed)?),
        "installAndPersist" => json!(manager.install_and_persist(&args.str(1)?, args.bool(2)).await.map_err(failed)?),
        "remove" => json!(manager.remove(&args.str(1)?, args.bool(2)).await.map_err(failed)?),
        "removeAndPersist" => json!(manager.remove_and_persist(&args.str(1)?, args.bool(2)).await.map_err(failed)?),
        "update" => json!(manager.update(args.opt_str(1).as_deref()).await.map_err(failed)?),
        "checkForAvailableUpdates" => json!(manager.check_for_available_updates().await),
        other => return Err(error(format!("Unknown host operation: packages.{other}"))),
    };
    Ok(HostValue::Json(value))
}
