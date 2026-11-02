//! SDK `ModelRuntime` instances (upstream `ModelRuntime.create`): JavaScript holds ids of native runtimes
//! kept here, id 0 is the runner's own; none need a session, so extension factories can use them.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pi_ai::Model;
use pi_ai::auth::FileCredentialStore;
use pi_js::{HostValue, JsError};
use serde_json::{Value, json};

use super::ops::{Args, error};
use super::runner::ExtensionRunner;
use crate::agent_session::{
    CachedFileModelsStore, CatalogSetup, ModelRuntime, ModelRuntimeOptions, catalog_base_url, lock,
};
use crate::catalog::{CatalogConfig, RefreshOptions, models_store_path};
use crate::config::{auth_path, models_path, offline};

#[derive(Default)]
pub(crate) struct ModelRuntimes(Mutex<HashMap<u64, Arc<ModelRuntime>>>);

impl ExtensionRunner {
    pub(crate) fn sdk_model_runtime(&self, id: u64) -> Result<Arc<ModelRuntime>, JsError> {
        if id == 0 {
            return Ok(self.model_runtime().clone());
        }
        let runtime = lock(&self.model_runtimes.0).get(&id).cloned();
        runtime.ok_or_else(|| error(format!("Unknown ModelRuntime: {id}")))
    }
}

fn of_provider(models: impl IntoIterator<Item = Model>, provider: Option<&str>) -> Vec<Model> {
    models.into_iter().filter(|model| provider.is_none_or(|wanted| model.provider == wanted)).collect()
}

fn of_type(model: &Model, kind: &str) -> bool {
    model.model_type.as_deref().unwrap_or("chat") == kind
}

/// `modelRuntime.read(id, method, ...)`; method arguments start at 2 as in `models.read`.
pub(crate) fn sync(runner: &ExtensionRunner, name: &str, args: &Args) -> Result<HostValue, JsError> {
    if name != "read" {
        return Err(error(format!("Unknown model runtime operation: {name}")));
    }
    let id = args.u64(0);
    let runtime = runner.sdk_model_runtime(id)?;
    let method = args.str(1)?;
    let all = || runtime.registry().all_models().to_vec();
    let available = |models: Vec<Model>| -> Vec<Model> {
        models.into_iter().filter(|model| runtime.has_configured_auth(&model.provider)).collect()
    };
    let value = match method.as_str() {
        "getModels" => json!(of_provider(runtime.all_models(), args.opt_str(2).as_deref())),
        "getAllModels" => json!(of_provider(all(), args.opt_str(2).as_deref())),
        "getAllAvailable" => json!(available(of_provider(all(), args.opt_str(2).as_deref()))),
        "findOfType" => {
            let (kind, provider, model_id) = (args.str(2)?, args.str(3)?, args.str(4)?);
            let found = all().into_iter().find(|m| m.provider == provider && m.id == model_id && of_type(m, &kind));
            json!(found)
        }
        "getModelsOfType" | "getAvailableOfType" => {
            let kind = args.str(2)?;
            let models = of_provider(all(), args.opt_str(3).as_deref());
            let models: Vec<Model> = models.into_iter().filter(|model| of_type(model, &kind)).collect();
            let models = if method == "getModelsOfType" { models } else { available(models) };
            json!(models)
        }
        "getProviders" => {
            let registry = runtime.registry();
            let providers = registry.providers().into_iter().map(|provider| {
                let name = registry.provider_name(&provider).unwrap_or(&provider).to_string();
                json!({"id": provider, "name": name})
            });
            json!(providers.collect::<Vec<_>>())
        }
        _ => {
            let registered = || if id == 0 { runner.provider_ids() } else { Vec::new() };
            super::ops_session::read_models(&runtime, registered, &method, args)?
        }
    };
    Ok(HostValue::Json(value))
}

pub(crate) async fn run(runner: Arc<ExtensionRunner>, name: &str, args: Args) -> Result<HostValue, JsError> {
    if name == "create" {
        return Ok(HostValue::Json(create(&runner, &args.json(0)).await?));
    }
    let runtime = runner.sdk_model_runtime(args.u64(0))?;
    let value = match name {
        "refresh" => refresh(&runner, &runtime, &args.json(1)).await,
        "setRuntimeApiKey" => {
            runtime.set_runtime_api_key(&args.str(1)?, &args.str(2)?);
            Value::Null
        }
        "removeRuntimeApiKey" => {
            runtime.remove_runtime_api_key(&args.str(1)?).await;
            Value::Null
        }
        "listCredentials" => json!(runtime.list_credentials().await.map_err(error)?),
        "getAvailable" => {
            runtime.refresh_configured_providers().await;
            json!(of_provider(runtime.available_models(), args.opt_str(1).as_deref()))
        }
        _ => super::ops_async::runtime_op(&runner, runtime, name, args).await?,
    };
    Ok(HostValue::Json(value))
}

async fn create(runner: &Arc<ExtensionRunner>, options: &Value) -> Result<Value, JsError> {
    let path = |key: &str| options[key].as_str().map(PathBuf::from);
    let agent_dir = path("agentDir").unwrap_or_else(|| runner.agent_dir().to_path_buf());
    let models = path("modelsPath").unwrap_or_else(|| models_path(&agent_dir));
    let store_dir = models.parent().map(Path::to_path_buf).unwrap_or_default();
    let mut runtime_options = ModelRuntimeOptions::for_agent_dir(&agent_dir);
    runtime_options.credentials =
        Arc::new(FileCredentialStore::new(path("authPath").unwrap_or_else(|| auth_path(&agent_dir))));
    runtime_options.models_path = models;
    runtime_options.credential_env = runner.model_runtime().resolver().env().clone();
    let runtime = Arc::new(ModelRuntime::create(runtime_options).await);
    let store = CachedFileModelsStore::new(path("modelsStorePath").unwrap_or_else(|| models_store_path(&store_dir)));
    let base_url = options["catalogBaseUrl"].as_str().map(str::to_string).or_else(catalog_base_url);
    let config = CatalogConfig { catalog_base_url: base_url, ..Default::default() };
    runtime.enable_dynamic_catalogs(CatalogSetup { store: Arc::new(store), config }).await;
    let network = options["allowModelNetwork"].as_bool() == Some(true) && !offline();
    if options["refreshOnCreate"].as_bool() != Some(false) && network {
        let cancel = runner.token(options["signal"].as_u64()).child_token();
        let timer = options["modelRefreshTimeoutMs"].as_u64().map(|ms| {
            let cancel = cancel.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(ms)).await;
                cancel.cancel();
            })
        });
        runtime
            .refresh_catalogs_with(RefreshOptions { allow_network: true, providers: None, force: false, cancel })
            .await;
        if let Some(timer) = timer {
            timer.abort();
        }
    }
    let id = runner.next_id();
    lock(&runner.model_runtimes.0).insert(id, runtime);
    Ok(json!(id))
}

async fn refresh(runner: &ExtensionRunner, runtime: &ModelRuntime, options: &Value) -> Value {
    let allow_network = options["allowNetwork"].as_bool().unwrap_or(!offline());
    let providers =
        options["providers"].as_array().map(|list| list.iter().filter_map(Value::as_str).map(str::to_string).collect());
    let cancel = runner.token(options["signal"].as_u64());
    let force = options["force"].as_bool().unwrap_or(false);
    let result =
        runtime.refresh_catalogs_with(RefreshOptions { allow_network, providers, force, cancel: cancel.clone() }).await;
    json!({"aborted": result.aborted || cancel.is_cancelled(), "errors": result.errors})
}
