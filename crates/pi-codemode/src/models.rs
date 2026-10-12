//! `models.*` for scripts: the model registry methods documented in `codemode.md`. Classifier and
//! image calls appear as nested call rows, and their usage is added to the `codemode` result.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use pi_ai::Usage;
use serde_json::Value;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use crate::calls::{
    CallLog, CodemodeNestedCall, CodemodeNestedCallStatus, ERROR_PREVIEW_CHARS, combine_usage, truncate_text,
    usage_from_value,
};
use crate::discovery::spread_args;
use crate::host::{CodemodeHost, ModelType};
use crate::js::js_stringify;
use crate::model_checks::{check_classifier_context, check_images_context, describe_value};
use crate::sandbox::SandboxFunction;

/// `models.classify()` and `models.generateImages()` calls one script may have in flight.
pub const MAX_CONCURRENT_MODEL_CALLS: usize = 4;

/// Totals of the script's `models.*` calls.
#[derive(Default)]
pub(crate) struct ModelTotals {
    pub usage: Mutex<Option<Usage>>,
    /// Images returned by `models.generateImages()`, to notice a script that never shows them.
    pub generated_images: AtomicUsize,
}

impl ModelTotals {
    fn add_usage(&self, usage: Usage) {
        let mut total = self.usage.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        *total = Some(match total.as_ref() {
            Some(previous) => combine_usage(previous, &usage),
            None => usage,
        });
    }
}

struct ModelCalls {
    host: Arc<dyn CodemodeHost>,
    tool_call_id: String,
    docs_path: String,
    log: Arc<CallLog>,
    totals: Arc<ModelTotals>,
    limiter: Semaphore,
    count: AtomicUsize,
}

fn to_model_type(value: Option<&Value>) -> Result<ModelType, String> {
    match value.and_then(Value::as_str) {
        Some("chat") => Ok(ModelType::Chat),
        Some("image") => Ok(ModelType::Image),
        Some("classifier") => Ok(ModelType::Classifier),
        _ => Err(format!(
            "Unknown model type {}. Use \"chat\", \"image\", or \"classifier\".",
            value.map_or_else(|| "undefined".into(), js_stringify)
        )),
    }
}

fn to_provider(value: Option<&Value>) -> Result<Option<&str>, String> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(provider)) => Ok(Some(provider)),
        Some(_) => Err("provider must be a string".into()),
    }
}

/// Catalog entry for scripts. `headers` is dropped because models.json headers can carry credentials.
fn to_model_info(mut model: Value) -> Value {
    if let Some(object) = model.as_object_mut() {
        object.shift_remove("headers");
    }
    model
}

/// `an image`, `a classifier`.
fn with_article(word: &str) -> String {
    let article = if word.starts_with(['a', 'e', 'i', 'o', 'u']) { "an" } else { "a" };
    format!("{article} {word}")
}

fn string_field<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or("")
}

impl ModelCalls {
    /// Resolve the script's model by provider and id only, so a script-supplied `baseUrl` or
    /// `headers` never receives the credentials; check the context, then run the call as a row.
    async fn run(
        &self,
        name: &str,
        model_type: ModelType,
        args: Vec<Value>,
        cancel: CancellationToken,
    ) -> Result<Option<Value>, String> {
        let models = self.host.models().ok_or("models are not available")?;
        let kind = model_type.as_str();
        let list_hint = format!("List the {kind} models you can use with models.getAvailableOfType(\"{kind}\").");
        let model = args.first();
        let (provider, id) = match model {
            Some(Value::Object(fields)) => match (fields.get("provider"), fields.get("id")) {
                (Some(Value::String(provider)), Some(Value::String(id))) => (provider.clone(), id.clone()),
                _ => return Err(self.bad_model(name, kind, model, &list_hint)),
            },
            _ => return Err(self.bad_model(name, kind, model, &list_hint)),
        };
        let reference = format!("{provider}/{id}");
        let Some(resolved) = models.get_model_of_type(model_type, &provider, &id) else {
            let actual = ModelType::ALL
                .into_iter()
                .find(|other| *other != model_type && models.get_model_of_type(*other, &provider, &id).is_some());
            return Err(match actual {
                Some(actual) => format!(
                    "\"{reference}\" is {} model, not {} model. {list_hint}",
                    with_article(actual.as_str()),
                    with_article(kind)
                ),
                None => format!("Unknown {kind} model \"{reference}\". {list_hint}"),
            });
        };
        let context = args.get(1).cloned();
        let checked = match model_type {
            ModelType::Image => check_images_context(context.as_ref(), &self.docs_path)?,
            _ => check_classifier_context(context.as_ref(), &self.docs_path)?,
        };

        let number = self.count.fetch_add(1, Ordering::SeqCst) + 1;
        let row = self.log.push(CodemodeNestedCall {
            id: format!("{}/{name}/{number}", self.tool_call_id),
            name: name.into(),
            args: format!("{}/{}", string_field(&resolved, "provider"), string_field(&resolved, "id")),
            status: CodemodeNestedCallStatus::Running,
            duration_ms: None,
            error: None,
            cost: None,
        });
        let started = Instant::now();
        let result = {
            let _permit = self.limiter.acquire().await.map_err(|error| error.to_string())?;
            match model_type {
                ModelType::Image => models.generate_images(&resolved, checked, cancel).await,
                _ => models.classify(&resolved, checked, cancel).await,
            }
        }
        .map_err(|error| error.to_string())?;
        if model_type == ModelType::Image {
            let images = result.get("output").and_then(Value::as_array).map_or(0, |output| {
                output.iter().filter(|block| block.get("type").and_then(Value::as_str) == Some("image")).count()
            });
            self.totals.generated_images.fetch_add(images, Ordering::SeqCst);
        }
        let usage = result.get("usage").and_then(usage_from_value);
        self.log.update(row, |call| {
            call.duration_ms = Some(started.elapsed().as_secs_f64() * 1000.0);
            call.status = match string_field(&result, "stopReason") {
                "stop" => CodemodeNestedCallStatus::Ok,
                "aborted" => CodemodeNestedCallStatus::Cancelled,
                _ => CodemodeNestedCallStatus::Error,
            };
            if let Some(message) = result.get("errorMessage").and_then(Value::as_str).filter(|text| !text.is_empty()) {
                call.error = Some(truncate_text(message, ERROR_PREVIEW_CHARS));
            }
            if let Some(usage) = &usage {
                call.cost = Some(usage.cost.total);
            }
        });
        if let Some(usage) = usage {
            self.totals.add_usage(usage);
        }
        Ok(Some(result))
    }

    fn bad_model(&self, name: &str, kind: &str, model: Option<&Value>, list_hint: &str) -> String {
        // undefined arrives as null: spread arguments cross the sandbox as a JSON array.
        let undefined_hint = if matches!(model, None | Some(Value::Null)) {
            " models.getModelOfType() returns undefined for an unknown provider or id."
        } else {
            ""
        };
        format!(
            "{name}() expects {} model as its first argument, got {}.{undefined_hint} {list_hint}",
            with_article(kind),
            describe_value(model)
        )
    }

    fn catalog(&self, name: &str, args: Vec<Value>) -> Result<Option<Value>, String> {
        let models = self.host.models().ok_or("models are not available")?;
        if name == "getModelOfType" {
            let (Some(Value::String(provider)), Some(Value::String(id))) = (args.get(1), args.get(2)) else {
                let described: Vec<String> = args.iter().map(|arg| describe_value(Some(arg))).collect();
                return Err(format!(
                    "models.getModelOfType(type, provider, id) expects three strings, got ({}). The provider and the id are separate arguments, for example models.getModelOfType(\"classifier\", \"typesafe\", \"jev-latest\").",
                    described.join(", ")
                ));
            };
            let model = models.get_model_of_type(to_model_type(args.first())?, provider, id);
            return Ok(model.map(to_model_info));
        }
        let listed = models.get_models_of_type(to_model_type(args.first())?, to_provider(args.get(1))?);
        Ok(Some(Value::Array(listed.into_iter().map(to_model_info).collect())))
    }

    async fn available(&self, args: Vec<Value>, cancel: CancellationToken) -> Result<Option<Value>, String> {
        let models = self.host.models().ok_or("models are not available")?;
        let model_type = to_model_type(args.first())?;
        let available = models
            .get_available_of_type(model_type, to_provider(args.get(1))?, cancel)
            .await
            .map_err(|error| error.to_string())?;
        Ok(Some(Value::Array(available.into_iter().map(to_model_info).collect())))
    }
}

/// The `models.*` globals of one script.
pub(crate) fn model_globals(
    host: Arc<dyn CodemodeHost>,
    tool_call_id: &str,
    docs_path: &str,
    log: Arc<CallLog>,
    totals: Arc<ModelTotals>,
) -> Vec<SandboxFunction> {
    let calls = Arc::new(ModelCalls {
        host,
        tool_call_id: tool_call_id.into(),
        docs_path: docs_path.into(),
        log,
        totals,
        limiter: Semaphore::new(MAX_CONCURRENT_MODEL_CALLS),
        count: AtomicUsize::new(0),
    });
    let global = |name: &'static str| {
        let calls = calls.clone();
        SandboxFunction::new(format!("models.{name}"), move |args, cancel| {
            let calls = calls.clone();
            async move {
                let args = spread_args(args);
                match name {
                    "getAvailableOfType" => calls.available(args, cancel).await,
                    "classify" => calls.run("models.classify", ModelType::Classifier, args, cancel).await,
                    "generateImages" => calls.run("models.generateImages", ModelType::Image, args, cancel).await,
                    _ => calls.catalog(name, args),
                }
            }
        })
        .with_spread()
    };
    ["getModelsOfType", "getAvailableOfType", "getModelOfType", "classify", "generateImages"]
        .into_iter()
        .map(global)
        .collect()
}
