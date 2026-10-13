//! The `llama.cpp` provider: loaded and sleeping router models as `openai-completions` chat
//! models, every selectable model as a classifier, and its persisted catalog (upstream
//! `extensions/llama/provider.ts`).

use std::collections::HashMap;
use std::sync::RwLock;

use async_trait::async_trait;
use pi_ai::Model;
use pi_ai::auth::Credential;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use super::auth::{credential_server_url, resolve_llama_auth};
use super::client::{LlamaClient, LlamaModelInfo, LlamaServerProps, llama_inference_url, status};
use crate::catalog::refresh::{DynamicCatalog, Persist, RefreshContext, RefreshCredentials};
use crate::catalog::store::{ModelsStoreEntry, model_type};

pub const LLAMA_PROVIDER_ID: &str = "llama.cpp";
pub const DEFAULT_LLAMA_SERVER_URL: &str = "http://127.0.0.1:8080";
const DEFAULT_CONTEXT_WINDOW: u64 = 128_000;

/// Loaded and sleeping models; unloaded presets too when router autoload can load them.
pub fn model_is_selectable(model: &LlamaModelInfo, router_autoload: bool) -> bool {
    model.is_loaded()
        || (router_autoload
            && model.is_status(status::UNLOADED)
            && model.status.failed != Some(true)
            && model.source.as_deref() == Some("preset"))
}

async fn router_autoload_enabled(client: &LlamaClient, catalog: &[LlamaModelInfo], cancel: &CancellationToken) -> bool {
    let has_preset =
        catalog.iter().any(|model| model.is_status(status::UNLOADED) && model.source.as_deref() == Some("preset"));
    has_preset && client.props(None, cancel).await.is_ok_and(|props| props.models_autoload == Some(true))
}

/// The `--ctx-size`, `-c`, or `-ctx` value from the model's command line.
pub fn configured_context_window(model: &LlamaModelInfo) -> Option<u64> {
    let args = model.status.args.as_deref().unwrap_or_default();
    args.windows(2)
        .filter(|pair| matches!(pair[0].as_str(), "--ctx-size" | "-c" | "-ctx"))
        .find_map(|pair| pair[1].parse::<u64>().ok().filter(|value| *value > 0))
}

/// Runtime context, then the command line, then the cached value, then the training context.
pub fn context_window_of(model: &LlamaModelInfo, cached: Option<u64>) -> u64 {
    let meta = model.meta.as_ref();
    meta.and_then(|meta| meta.n_ctx)
        .filter(|value| *value > 0)
        .or_else(|| configured_context_window(model))
        .or(cached.filter(|value| *value > 0))
        .or_else(|| meta.and_then(|meta| meta.n_ctx_train).filter(|value| *value > 0))
        .unwrap_or(DEFAULT_CONTEXT_WINDOW)
}

fn outputs(model: &LlamaModelInfo, modality: &str) -> bool {
    let outputs = model.architecture.as_ref().and_then(|architecture| architecture.output_modalities.as_ref());
    outputs.is_some_and(|outputs| outputs.iter().any(|output| output == modality))
}

/// A native decision model (llama.cpp 0.6.0 and later report `decisions` as an output).
pub fn is_decision_model(model: &LlamaModelInfo) -> bool {
    outputs(model, "decisions")
}

/// Decision-only models cannot generate text and are not listed for chat.
pub fn is_chat_model(model: &LlamaModelInfo) -> bool {
    !is_decision_model(model) || outputs(model, "text")
}

/// Decision models classify through `/v1/systemone`; chat models through next-token
/// label probabilities (`llama-cpp-classify`).
pub fn to_classifier_model(model: &LlamaModelInfo, server_url: &str, cached: Option<u64>) -> Model {
    let decision = is_decision_model(model);
    let base_url = if decision { llama_inference_url(server_url).unwrap_or_default() } else { server_url.to_string() };
    let value = json!({
        "type": "classifier",
        "id": model.id,
        "name": model.id,
        "api": if decision { "typesafe-system-one" } else { "llama-cpp-classify" },
        "provider": LLAMA_PROVIDER_ID,
        "baseUrl": base_url,
        "input": ["text"],
        "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0},
        "contextWindow": context_window_of(model, cached),
    });
    serde_json::from_value(value).expect("classifier model JSON is valid")
}

/// A chat model; `props` of a loaded model reveal thinking support in its chat template.
pub fn to_chat_model(
    model: &LlamaModelInfo,
    server_url: &str,
    props: Option<&LlamaServerProps>,
    cached: Option<u64>,
) -> Model {
    let context_window = context_window_of(model, cached);
    let reasoning =
        props.and_then(|props| props.chat_template.as_deref()).is_some_and(|t| t.contains("enable_thinking"));
    let images = model
        .architecture
        .as_ref()
        .and_then(|architecture| architecture.input_modalities.as_ref())
        .is_some_and(|inputs| inputs.iter().any(|input| input == "image"));
    let mut compat = json!({
        "supportsStore": false,
        "supportsDeveloperRole": false,
        "supportsReasoningEffort": false,
        "supportsUsageInStreaming": true,
        "supportsStrictMode": false,
        "maxTokensField": "max_tokens",
    });
    let mut value = json!({
        "id": model.id,
        "name": model.id,
        "api": "openai-completions",
        "provider": LLAMA_PROVIDER_ID,
        "baseUrl": llama_inference_url(server_url).unwrap_or_default(),
        "reasoning": reasoning,
        "input": if images { json!(["text", "image"]) } else { json!(["text"]) },
        "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0},
        "contextWindow": context_window,
        "maxTokens": context_window,
    });
    if reasoning {
        compat["thinkingFormat"] = json!("qwen-chat-template");
        value["thinkingLevelMap"] =
            json!({"off": "off", "minimal": null, "low": null, "medium": "medium", "high": null, "xhigh": null});
    }
    value["compat"] = compat;
    serde_json::from_value(value).expect("chat model JSON is valid")
}

#[derive(Default)]
struct Catalog {
    chat: Vec<Model>,
    classifiers: Vec<Model>,
}

/// The llama.cpp router provider. It has no bundled models and stays empty until configured.
#[derive(Default)]
pub struct LlamaProvider {
    catalog: RwLock<Catalog>,
    http: Option<reqwest::Client>,
}

impl LlamaProvider {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_http(mut self, http: reqwest::Client) -> Self {
        self.http = Some(http);
        self
    }

    fn set(&self, chat: Vec<Model>, classifiers: Vec<Model>) {
        *self.catalog.write().unwrap_or_else(|poisoned| poisoned.into_inner()) = Catalog { chat, classifiers };
    }

    /// Replace the models from a router catalog (as `/llama` does after loads and unloads).
    pub fn set_catalog(&self, catalog: &[LlamaModelInfo], server_url: &str, router_autoload: bool) {
        let selectable: Vec<&LlamaModelInfo> =
            catalog.iter().filter(|model| model_is_selectable(model, router_autoload)).collect();
        let chat = selectable
            .iter()
            .filter(|model| is_chat_model(model))
            .map(|model| to_chat_model(model, server_url, None, None));
        let classifiers = selectable.iter().map(|model| to_classifier_model(model, server_url, None));
        self.set(chat.collect(), classifiers.collect());
    }

    /// Chat models for `/model`.
    pub fn models(&self) -> Vec<Model> {
        self.catalog.read().unwrap_or_else(|poisoned| poisoned.into_inner()).chat.clone()
    }

    /// Chat models followed by classifiers.
    pub fn all_models(&self) -> Vec<Model> {
        let catalog = self.catalog.read().unwrap_or_else(|poisoned| poisoned.into_inner());
        catalog.chat.iter().chain(&catalog.classifiers).cloned().collect()
    }

    fn client(&self, server_url: &str, api_key: Option<String>) -> anyhow::Result<LlamaClient> {
        let client = LlamaClient::new(server_url, api_key).map_err(anyhow::Error::msg)?;
        Ok(match &self.http {
            Some(http) => client.with_http(http.clone()),
            None => client,
        })
    }
}

fn is_llama_classifier(model: &Model) -> bool {
    model_type(model) == "classifier" && matches!(model.api.as_str(), "llama-cpp-classify" | "typesafe-system-one")
}

#[async_trait]
impl DynamicCatalog for LlamaProvider {
    fn provider_id(&self) -> &str {
        LLAMA_PROVIDER_ID
    }

    fn compose(&self, _builtin: Vec<Model>) -> Vec<Model> {
        self.all_models()
    }

    async fn refresh(&self, context: &RefreshContext<'_>) -> anyhow::Result<()> {
        let mut cached: HashMap<String, u64> = HashMap::new();
        if let Some(stored) = context.stored {
            let own = stored.models.iter().filter(|model| model.provider == LLAMA_PROVIDER_ID);
            let (classifiers, chat): (Vec<Model>, Vec<Model>) = own
                .filter(|model| {
                    is_llama_classifier(model) || (model_type(model) == "chat" && model.api == "openai-completions")
                })
                .cloned()
                .partition(is_llama_classifier);
            for model in chat.iter().chain(&classifiers) {
                cached.insert(model.id.clone(), model.context_window);
            }
            if !context.publisher.publish(Persist::Unchanged).await {
                return Ok(());
            }
            self.set(chat, classifiers);
        }
        let Some(Credential::ApiKey(credential)) = context.credential else { return Ok(()) };
        if !context.allow_network || context.cancel.is_cancelled() {
            return Ok(());
        }
        let Some(server_url) = credential_server_url(Some(credential)).map_err(anyhow::Error::msg)? else {
            return Ok(());
        };
        let client = self.client(&server_url, credential.key.clone())?;
        let catalog = client.list(false, context.cancel).await?;
        if context.cancel.is_cancelled() {
            return Ok(());
        }
        let autoload = router_autoload_enabled(&client, &catalog, context.cancel).await;
        let selectable: Vec<&LlamaModelInfo> =
            catalog.iter().filter(|model| model_is_selectable(model, autoload)).collect();
        // Only loaded models expose their chat template without loading or waking them.
        let refreshed = selectable.iter().filter(|model| is_chat_model(model)).map(|model| async {
            let cached = cached.get(&model.id).copied();
            if !model.is_status(status::LOADED) {
                return Ok::<_, anyhow::Error>(to_chat_model(model, &server_url, None, cached));
            }
            let props = client.props(Some(&model.id), context.cancel).await?;
            Ok(to_chat_model(model, &server_url, Some(&props), cached))
        });
        let chat = futures::future::try_join_all(refreshed).await?;
        let classifiers: Vec<Model> = selectable
            .iter()
            .map(|model| to_classifier_model(model, &server_url, cached.get(&model.id).copied()))
            .collect();
        if context.cancel.is_cancelled() {
            return Ok(());
        }
        let models = chat.iter().chain(&classifiers).cloned().collect();
        let entry = ModelsStoreEntry { checked_at: Some(pi_ai::now_ms()), ..ModelsStoreEntry::with_models(models) };
        if context.publisher.publish(Persist::Write(entry)).await {
            self.set(chat, classifiers);
        }
        Ok(())
    }

    async fn resolve_credential(
        &self,
        stored: Option<&Credential>,
        _credentials: &dyn RefreshCredentials,
    ) -> anyhow::Result<Option<Credential>> {
        let stored = match stored {
            Some(Credential::ApiKey(credential)) => Some(credential),
            _ => None,
        };
        let env = |name: &str| pi_ai::env::process_env_value(name);
        let resolved = resolve_llama_auth(stored, &env).map_err(anyhow::Error::msg)?;
        Ok(resolved.map(|resolved| {
            Credential::ApiKey(pi_ai::auth::ApiKeyCredential { key: resolved.auth.api_key, env: resolved.env })
        }))
    }
}
