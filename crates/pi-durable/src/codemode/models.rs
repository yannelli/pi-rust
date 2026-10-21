//! `models.*` for code mode scripts, as in the coding agent's code mode: the model catalog, and classifier and image
//! model calls. The catalog functions are journaled host globals. `models.classify()` and `models.generateImages()`
//! are paid effects, so each runs as an internal tool: a nested durable call with its own task, usage, and replay.

use std::sync::{Arc, LazyLock};

use pi_ai::{ContentBlock, Usage};
use pi_chord::{Context, Json};
use pi_codemode::js::js_stringify;
use pi_codemode::{CodemodeModels, MAX_CONCURRENT_MODEL_CALLS, ModelType};
use serde_json::{Value, json};

use super::model_checks::{check_classifier_context, check_images_context, describe_value};
use super::run::AbortLink;
use crate::errors::DurableError;
use crate::harness::{ToolCaller, ToolExecutionResult, ToolRegistration, define_tool};

/// Reserved names of code mode's internal tools; they are never listed to scripts or the model.
pub const CODEMODE_INTERNAL_PREFIX: &str = "pi.codemode.";
pub const CLASSIFY_TOOL: &str = "pi.codemode.models.classify";
pub const GENERATE_IMAGES_TOOL: &str = "pi.codemode.models.generateImages";

/// The internal tool a script's call of a `models` global becomes.
pub(super) fn model_call(global: &str) -> Option<&'static str> {
    match global {
        "models.classify" => Some(CLASSIFY_TOOL),
        "models.generateImages" => Some(GENERATE_IMAGES_TOOL),
        _ => None,
    }
}

/// The globals of `models.classify()` and `models.generateImages()`.
pub(super) const MODEL_CALL_GLOBALS: [&str; 2] = ["models.classify", "models.generateImages"];

/// The catalog functions, host globals of the `models` namespace.
pub(super) const CATALOG_GLOBALS: [&str; 3] =
    ["models.getModelsOfType", "models.getAvailableOfType", "models.getModelOfType"];

pub static MODELS_DESCRIPTION: LazyLock<String> = LazyLock::new(|| {
    [
        "`models` (all functions return promises):".to_owned(),
        "- `models.getModelsOfType(type, provider?)`, `models.getAvailableOfType(type, provider?)` (configured auth only), `models.getModelOfType(type, provider, id)`: the catalog; `type` is \"chat\", \"image\", or \"classifier\".".to_owned(),
        "- `models.classify(model, { state: { ... }, images?: [{ type: \"image\", data, mimeType }], questions: { <id>: { type: \"choice\", instructions, criteria: { <label>: <meaning> } } | { type: \"score\", instructions, criteria: [<lowest level>, ..., <highest level>] } | { type: \"bool\", instructions, criteria: { true: <meaning>, false: <meaning> } } } })` resolves to `{ answers: { <id>: ... }, stopReason, errorMessage?, usage? }`.".to_owned(),
        "- `models.generateImages(model, { input: [{ type: \"text\", text: <prompt> }, ...optional { type: \"image\", data, mimeType }] })` resolves to `{ output: [image blocks], stopReason, errorMessage?, usage? }`; show each image with `image(block)`.".to_owned(),
        format!("At most {MAX_CONCURRENT_MODEL_CALLS} classify/generateImages calls run at once; more wait."),
    ]
    .join("\n")
});

/// `an image`, `a classifier`.
fn with_article(word: &str) -> String {
    let article = if word.starts_with(['a', 'e', 'i', 'o', 'u']) { "an" } else { "a" };
    format!("{article} {word}")
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

/// Catalog entry for scripts. `headers` is dropped because configured headers can carry credentials.
fn to_model_info(mut model: Value) -> Value {
    if let Some(object) = model.as_object_mut() {
        object.shift_remove("headers");
    }
    model
}

/// Why a catalog call failed: an error the script gets, or the call's abort.
pub(super) enum CatalogError {
    Script(String),
    Aborted(DurableError),
}

impl From<String> for CatalogError {
    fn from(message: String) -> Self {
        Self::Script(message)
    }
}

/// The catalog functions: what a script's call of `name` with `args` (the argument list) resolves to.
pub(super) async fn catalog_call(
    name: &str,
    args: Option<&Value>,
    models: &dyn CodemodeModels,
    context: &Context,
) -> Result<Option<Value>, CatalogError> {
    let list = args.and_then(Value::as_array).cloned().unwrap_or_default();
    let (kind, provider, id) = (list.first(), list.get(1), list.get(2));
    let listed = match name {
        "models.getModelsOfType" => models.get_models_of_type(to_model_type(kind)?, to_provider(provider)?),
        "models.getAvailableOfType" => {
            let (kind, provider) = (to_model_type(kind)?, to_provider(provider)?);
            let link = AbortLink::token(context);
            let available = models.get_available_of_type(kind, provider, link.cancel.clone()).await;
            if let Some(signal) = context.abort_signal() {
                signal.throw_if_aborted().map_err(|error| CatalogError::Aborted(error.into()))?;
            }
            available.map_err(|error| error.to_string())?
        }
        _ => {
            let (Some(Value::String(provider)), Some(Value::String(id))) = (provider, id) else {
                let described: Vec<String> = list.iter().map(|arg| describe_value(Some(arg))).collect();
                return Err(CatalogError::Script(format!(
                    "models.getModelOfType(type, provider, id) expects three strings, got ({}). The provider and the id are separate arguments, for example models.getModelOfType(\"classifier\", \"typesafe\", \"jev-latest\").",
                    described.join(", ")
                )));
            };
            let model = models.get_model_of_type(to_model_type(kind)?, provider, id);
            return Ok(model.map(to_model_info));
        }
    };
    Ok(Some(Value::Array(listed.into_iter().map(to_model_info).collect())))
}

/// Resolve the script's model by provider and id only, so a script-supplied `baseUrl` or `headers` never receives
/// the credentials, with errors that say what to do.
fn resolve_model(
    name: &str,
    kind: ModelType,
    model: Option<&Value>,
    models: &dyn CodemodeModels,
) -> Result<Value, String> {
    let label = kind.as_str();
    let list_hint = format!("List the {label} models you can use with models.getAvailableOfType(\"{label}\").");
    let fields = model.and_then(Value::as_object);
    let provider = fields.and_then(|fields| fields.get("provider")).and_then(Value::as_str);
    let id = fields.and_then(|fields| fields.get("id")).and_then(Value::as_str);
    let (Some(provider), Some(id)) = (provider, id) else {
        let undefined_hint = match model {
            None | Some(Value::Null) => " models.getModelOfType() returns undefined for an unknown provider or id.",
            _ => "",
        };
        return Err(format!(
            "{name}() expects {} model as its first argument, got {}.{undefined_hint} {list_hint}",
            with_article(label),
            describe_value(model)
        ));
    };
    if let Some(resolved) = models.get_model_of_type(kind, provider, id) {
        return Ok(resolved);
    }
    let actual = [ModelType::Chat, ModelType::Image, ModelType::Classifier]
        .into_iter()
        .find(|other| *other != kind && models.get_model_of_type(*other, provider, id).is_some());
    Err(match actual {
        Some(actual) => format!(
            "\"{provider}/{id}\" is {} model, not {} model. {list_hint}",
            with_article(actual.as_str()),
            with_article(label)
        ),
        None => format!("Unknown {label} model \"{provider}/{id}\". {list_hint}"),
    })
}

/// The result of a model call as the tool returns it: the whole result for the script, usage counted by the Harness.
fn model_result(label: &str, result: Value) -> ToolExecutionResult {
    let stop_reason = result.get("stopReason").and_then(Value::as_str).unwrap_or("undefined");
    let ok = stop_reason == "stop";
    let text = if ok {
        format!("{label} done")
    } else {
        let message = result.get("errorMessage").and_then(Value::as_str).unwrap_or_default();
        format!("{label} {stop_reason}: {message}")
    };
    let usage = result.get("usage").and_then(|usage| serde_json::from_value::<Usage>(usage.clone()).ok());
    ToolExecutionResult {
        output: Some(vec![ContentBlock::text(text)]),
        structured_output: Some(Json::from(result)),
        is_error: (!ok).then_some(true),
        usage,
        ..ToolExecutionResult::default()
    }
}

/// One internal tool: resolve the model, check the context, then run the call with the context's abort.
fn model_tool(name: &'static str, kind: ModelType, models: Arc<dyn CodemodeModels>) -> ToolRegistration {
    let global = name.trim_start_matches(CODEMODE_INTERNAL_PREFIX);
    let parameters = json!({
        "type": "object",
        "properties": { "model": {}, "context": {} },
        "required": ["model", "context"],
    });
    define_tool(name, format!("{global}() of code mode scripts."), parameters, move |args, _, context| {
        let models = models.clone();
        async move {
            let failed = |message: String| DurableError::error(message);
            let model = resolve_model(global, kind, args.get("model"), &*models).map_err(failed)?;
            let checked = match kind {
                ModelType::Image => check_images_context(args.get("context")),
                _ => check_classifier_context(args.get("context")),
            }
            .map_err(failed)?;
            let link = AbortLink::token(&context);
            let result = match kind {
                ModelType::Image => models.generate_images(&model, checked, link.cancel.clone()).await,
                _ => models.classify(&model, checked, link.cancel.clone()).await,
            };
            if let Some(signal) = context.abort_signal() {
                signal.throw_if_aborted()?;
            }
            let result = result.map_err(|error| DurableError::error(error.to_string()))?;
            let field = |key: &str| model.get(key).and_then(Value::as_str).unwrap_or_default().to_owned();
            let verb = global.trim_start_matches("models.");
            Ok(model_result(&format!("{verb} {}/{}", field("provider"), field("id")), result))
        }
    })
    .with_structured_output_schema(json!({}))
    .with_callers(vec![ToolCaller::Tools])
}

/// The internal tools behind `models.classify()` and `models.generateImages()`.
pub fn create_model_tools(models: Arc<dyn CodemodeModels>) -> Vec<ToolRegistration> {
    vec![
        model_tool(CLASSIFY_TOOL, ModelType::Classifier, models.clone()),
        model_tool(GENERATE_IMAGES_TOOL, ModelType::Image, models),
    ]
}

/// Image blocks in a `models.generateImages()` result.
pub(super) fn count_images(value: Option<&Json>) -> usize {
    let output = value.and_then(|value| value.get("output")).and_then(Json::as_array);
    output.map_or(0, |output| {
        output.iter().filter(|item| item.get("type").and_then(Json::as_str) == Some("image")).count()
    })
}
