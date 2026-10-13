//! `refreshModels`, `oauth.modifyModels`, `images`, and `classifiers` of a `pi.registerProvider`
//! config, called from the model runtime (upstream `composeModelProvider`).

use std::sync::{Arc, Weak};

use async_trait::async_trait;
use pi_ai::auth::OAuthCredential;
use pi_ai::classify::{ClassifierContext, ClassifierOptions, ClassifierResult};
use pi_ai::images::{AssistantImages, ImagesContext, ImagesOptions};
use pi_ai::model_operations::{classifier_error_result, image_error_result};
use pi_ai::{Model, RequestOptions};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::runner::ExtensionRunner;
use crate::agent_session::{ExtensionProviderHooks, RefreshModelsContext};

pub(crate) struct JsProviderHooks {
    pub runner: Weak<ExtensionRunner>,
    pub provider: String,
    pub refresh_models: bool,
    pub modify_models: bool,
    pub image_apis: Vec<String>,
    pub classifier_apis: Vec<String>,
}

fn strings(value: &Value) -> Vec<String> {
    value.as_array().into_iter().flatten().filter_map(Value::as_str).map(str::to_string).collect()
}

impl JsProviderHooks {
    /// The hooks a registration declares; `None` when it has none.
    pub fn from_config(runner: Weak<ExtensionRunner>, provider: &str, config: &Value) -> Option<Self> {
        let hooks = Self {
            runner,
            provider: provider.to_string(),
            refresh_models: config["hasRefreshModels"].as_bool() == Some(true),
            modify_models: config["oauth"]["hasModifyModels"].as_bool() == Some(true),
            image_apis: strings(&config["imageApis"]),
            classifier_apis: strings(&config["classifierApis"]),
        };
        let any = hooks.refresh_models
            || hooks.modify_models
            || !hooks.image_apis.is_empty()
            || !hooks.classifier_apis.is_empty();
        any.then_some(hooks)
    }

    fn runner(&self) -> Result<Arc<ExtensionRunner>, String> {
        self.runner.upgrade().ok_or_else(|| "The extension that registered this provider has shut down".to_string())
    }

    /// Call `method` with the provider name first; `cancel` becomes the `signal` of `options`.
    async fn call(&self, method: &str, mut args: Vec<Value>, cancel: &CancellationToken) -> Result<Value, String> {
        let runner = self.runner()?;
        let signal = runner.signal_for(cancel);
        if let Some(options) = args.last_mut().and_then(Value::as_object_mut) {
            options.insert("signal".into(), json!(signal));
        }
        args.insert(0, json!(self.provider));
        let result = runner.call_json(method, args).await;
        runner.release_signal(signal);
        result.map_err(|failure| failure.message)
    }
}

fn request_json(request: &RequestOptions) -> Value {
    json!({"apiKey": request.api_key, "headers": request.headers})
}

#[async_trait]
impl ExtensionProviderHooks for JsProviderHooks {
    fn has_refresh_models(&self) -> bool {
        self.refresh_models
    }

    fn has_modify_models(&self) -> bool {
        self.modify_models
    }

    fn image_apis(&self) -> Vec<String> {
        self.image_apis.clone()
    }

    fn classifier_apis(&self) -> Vec<String> {
        self.classifier_apis.clone()
    }

    async fn refresh_models(
        &self,
        context: RefreshModelsContext,
        cancel: CancellationToken,
    ) -> Result<Vec<Value>, String> {
        let context = json!({"credential": context.credential, "allowNetwork": context.allow_network,
            "force": context.force});
        let result = self.call("providerRefreshModels", vec![context], &cancel).await?;
        match result {
            Value::Array(models) => Ok(models),
            Value::Null => Ok(Vec::new()),
            other => Err(format!("refreshModels returned {other}, not a model list")),
        }
    }

    async fn modify_models(&self, models: Vec<Model>, credential: &OAuthCredential) -> Result<Vec<Model>, String> {
        let mut credential = json!(credential);
        credential["type"] = json!("oauth");
        let args = vec![json!(models), credential];
        let result = self.call("providerModifyModels", args, &CancellationToken::new()).await?;
        serde_json::from_value(result).map_err(|error| format!("modifyModels returned invalid models: {error}"))
    }

    async fn generate_images(
        &self,
        api: &str,
        model: &Model,
        context: &ImagesContext,
        options: ImagesOptions,
    ) -> AssistantImages {
        let cancel = options.request.cancel.clone();
        let mut request = request_json(&options.request);
        request["metadata"] = json!(options.metadata);
        let args = vec![json!(api), json!(model), json!(context), request];
        match self.call("providerImages", args, &cancel).await {
            Ok(value) => serde_json::from_value(value)
                .unwrap_or_else(|error| image_error_result(model, format!("Invalid image result: {error}"), false)),
            Err(message) => image_error_result(model, message, cancel.is_cancelled()),
        }
    }

    async fn classify(
        &self,
        api: &str,
        model: &Model,
        context: &ClassifierContext,
        options: ClassifierOptions,
    ) -> ClassifierResult {
        let cancel = options.request.cancel.clone();
        let mut request = request_json(&options.request);
        request["temperature"] = json!(options.temperature);
        let args = vec![json!(api), json!(model), json!(context), request];
        match self.call("providerClassify", args, &cancel).await {
            Ok(value) => serde_json::from_value(value).unwrap_or_else(|error| {
                classifier_error_result(model, format!("Invalid classifier result: {error}"), false)
            }),
            Err(message) => classifier_error_result(model, message, cancel.is_cancelled()),
        }
    }
}
