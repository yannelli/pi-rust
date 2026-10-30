//! The models of a Cursor login, `aiserver.v1.AiService/AvailableModels` (pi-rust extension): one
//! model per variant, with Cursor's context limit and the route the agent service expects.

use serde_json::{Value as Json, json};

use super::{MODEL_LIST_TIMEOUT, ModelListContext, Personalized};
use crate::auth::oauth::random_uuid;
use crate::models::is_chat_model;
use crate::providers::cursor::proto::{Value, Writer, field, fields};
use crate::providers::cursor::{CURSOR_BASE_URL, cursor_headers};
use crate::types::Model;

const PATH: &str = "/aiserver.v1.AiService/AvailableModels";

/// One routable variant: `id` is the variant's legacy slug (or the model name), `model` and
/// `parameters` go in `RequestedModel`.
#[derive(Debug, PartialEq)]
pub(crate) struct Variant {
    pub id: String,
    pub name: String,
    pub model: String,
    pub parameters: Vec<(String, String)>,
    pub max_mode: bool,
    pub context_window: Option<u64>,
    pub images: Option<bool>,
}

fn text<'a>(message: &[(u32, Value<'a>)], number: u32) -> Option<&'a str> {
    field(message, number).and_then(Value::as_str).map(str::trim).filter(|text| !text.is_empty())
}

fn flag(message: &[(u32, Value)], number: u32) -> Option<bool> {
    field(message, number).and_then(Value::as_u64).map(|value| value != 0)
}

fn limit(message: &[(u32, Value)], number: u32) -> Option<u64> {
    field(message, number).and_then(Value::as_u64).filter(|limit| *limit > 0)
}

/// The variants of an `AvailableModelsResponse`, without hidden models and repeated ids.
pub(crate) fn variants(response: &[u8]) -> Result<Vec<Variant>, String> {
    let mut out: Vec<Variant> = Vec::new();
    for (_, details) in fields(response)?.into_iter().filter(|(number, _)| *number == 2) {
        let details = fields(details.as_bytes().unwrap_or_default())?;
        let Some(model) = text(&details, 1) else { continue };
        if flag(&details, 35) == Some(true) {
            continue;
        }
        let display = text(&details, 17).unwrap_or(model);
        let (standard, max) = (limit(&details, 15), limit(&details, 16));
        let images = flag(&details, 10);
        let configs: Vec<_> =
            details.iter().filter(|(number, _)| *number == 30).filter_map(|(_, v)| v.as_bytes()).collect();
        let mut push = |variant: Variant| {
            if !out.iter().any(|known| known.id == variant.id) {
                out.push(variant);
            }
        };
        if configs.is_empty() {
            let base = Variant {
                id: model.into(),
                name: display.into(),
                model: model.into(),
                parameters: Vec::new(),
                max_mode: false,
                context_window: standard,
                images,
            };
            push(base);
            continue;
        }
        for config in configs {
            let config = fields(config)?;
            let Some(id) = text(&config, 11) else { continue };
            let mut parameters = Vec::new();
            for (_, parameter) in config.iter().filter(|(number, _)| *number == 1) {
                let parameter = fields(parameter.as_bytes().unwrap_or_default())?;
                if let Some(id) = text(&parameter, 1) {
                    parameters.push((id.to_string(), text(&parameter, 2).unwrap_or_default().to_string()));
                }
            }
            let max_mode = flag(&config, 3).unwrap_or(false);
            push(Variant {
                id: id.into(),
                name: text(&config, 2).map_or_else(|| display.to_string(), |variant| format!("{display} {variant}")),
                model: model.into(),
                parameters,
                max_mode,
                context_window: if max_mode { max.or(standard) } else { standard },
                images,
            });
        }
    }
    Ok(out)
}

/// A catalog model for `variant`, with the catalog model of the same id (else the first one) as
/// the template for cost, limits, and base URL.
fn model_for(variant: &Variant, catalog: &[Model]) -> Option<Model> {
    let chats: Vec<&Model> = catalog.iter().filter(|model| is_chat_model(model)).collect();
    let template = chats.iter().find(|model| model.id == variant.id).or_else(|| chats.first())?;
    let mut model = Model { id: variant.id.clone(), name: variant.name.clone(), ..(*template).clone() };
    if let Some(window) = variant.context_window {
        model.context_window = window;
        model.max_tokens = model.max_tokens.min(window);
    }
    if let Some(images) = variant.images {
        model.input = if images { vec!["text".into(), "image".into()] } else { vec!["text".into()] };
    }
    let parameters: Vec<Json> =
        variant.parameters.iter().map(|(id, value)| json!({"id": id, "value": value})).collect();
    let mut compat = model.compat.clone().filter(Json::is_object).unwrap_or_else(|| json!({}));
    compat["cursorModel"] = json!(variant.model);
    compat["cursorParameters"] = Json::Array(parameters);
    compat["cursorMaxMode"] = json!(variant.max_mode);
    model.compat = Some(compat);
    Some(model)
}

/// The catalog replaced by the login's models. 401, 403, and 404 leave the catalog unchanged.
pub(crate) async fn personalize_cursor_catalog(
    catalog: Vec<Model>,
    context: &ModelListContext,
) -> Result<Personalized, String> {
    let Some(key) = context.key() else { return Ok(Personalized::Models(catalog)) };
    let body = Writer::new().bool(2, true).bool(5, true).bool(7, true).bool(12, true).finish();
    let mut request = context.client.post(format!("{}{PATH}", context.base(CURSOR_BASE_URL))).body(body);
    for (name, value) in cursor_headers(key, &random_uuid(), "application/proto") {
        request = request.header(name, value);
    }
    let exchange = async {
        let response = request.send().await?;
        Ok::<_, reqwest::Error>((response.status().as_u16(), response.bytes().await?))
    };
    let failed = |reason: String| format!("Cursor model list request failed: {reason}");
    let (status, bytes) = tokio::select! {
        () = context.cancel.cancelled() => return Err(failed("cancelled".into())),
        result = tokio::time::timeout(MODEL_LIST_TIMEOUT, exchange) => match result {
            Err(_) => return Err(failed("timed out".into())),
            Ok(result) => result.map_err(|error| failed(error.to_string()))?,
        },
    };
    if matches!(status, 401 | 403 | 404) {
        return Ok(Personalized::Models(catalog));
    }
    if !(200..300).contains(&status) {
        return Err(failed(status.to_string()));
    }
    let Ok(variants) = variants(&bytes) else { return Ok(Personalized::Unmatched(catalog)) };
    let models: Vec<Model> = variants.iter().filter_map(|variant| model_for(variant, &catalog)).collect();
    Ok(if models.is_empty() { Personalized::Unmatched(catalog) } else { Personalized::Models(models) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::http_mock::{Reply, RouteServer};

    fn response() -> Vec<u8> {
        let parameter = |id: &str, value: &str| Writer::new().string(1, id).string(2, value);
        let high =
            Writer::new().message(1, parameter("thinking", "true")).string(2, "High").string(11, "claude-opus-5-high");
        let max =
            Writer::new().message(1, parameter("thinking", "true")).bool(3, true).string(11, "claude-opus-5-high-max");
        let opus = Writer::new()
            .string(1, "claude-opus-5")
            .bool(10, true)
            .varint(15, 300_000)
            .varint(16, 1_000_000)
            .string(17, "Claude Opus 5")
            .message(30, high)
            .message(30, max);
        let composer = Writer::new().string(1, "composer-2.5").bool(10, false).varint(15, 200_000);
        let hidden = Writer::new().string(1, "internal").bool(35, true);
        Writer::new().message(2, opus).message(2, composer).message(2, hidden).finish()
    }

    fn catalog() -> Vec<Model> {
        let model = |id: &str| Model {
            id: id.into(),
            api: "cursor-agent".into(),
            provider: "cursor".into(),
            context_window: 200_000,
            max_tokens: 64_000,
            model_type: Some("chat".into()),
            ..Model::default()
        };
        vec![model("claude-opus-5-high"), model("gpt-5.5")]
    }

    #[tokio::test]
    async fn variants_become_models_with_cursor_limits_and_routes() {
        let server = RouteServer::start(|_| Reply::new(200, "application/proto", response())).await;
        let context = ModelListContext {
            base_url: Some(server.base_url.clone()),
            client: reqwest::Client::builder().no_proxy().build().unwrap(),
            ..ModelListContext::new(Some("cursor-token".into()))
        };
        let Personalized::Models(models) = personalize_cursor_catalog(catalog(), &context).await.unwrap() else {
            panic!("models")
        };
        let ids: Vec<&str> = models.iter().map(|model| model.id.as_str()).collect();
        assert_eq!(ids, ["claude-opus-5-high", "claude-opus-5-high-max", "composer-2.5"]);
        assert_eq!((models[0].context_window, models[0].name.as_str()), (300_000, "Claude Opus 5 High"));
        assert_eq!(models[1].context_window, 1_000_000);
        assert_eq!(models[2].input, ["text"]);
        let compat = models[1].compat.as_ref().unwrap();
        assert_eq!(
            (compat["cursorModel"].as_str(), compat["cursorMaxMode"].as_bool()),
            (Some("claude-opus-5"), Some(true))
        );
        assert_eq!(compat["cursorParameters"], json!([{"id": "thinking", "value": "true"}]));
        let request = &server.requests()[0];
        assert_eq!(request.path, PATH);
        assert_eq!(request.headers["authorization"], "Bearer cursor-token");
        assert_eq!(request.headers["content-type"], "application/proto");
    }

    #[tokio::test]
    async fn a_rejected_login_keeps_the_catalog() {
        let server = RouteServer::start(|_| Reply::new(401, "application/json", "{}")).await;
        let context = ModelListContext {
            base_url: Some(server.base_url.clone()),
            client: reqwest::Client::builder().no_proxy().build().unwrap(),
            ..ModelListContext::new(Some("cursor-token".into()))
        };
        let result = personalize_cursor_catalog(catalog(), &context).await.unwrap();
        assert_eq!(result, Personalized::Models(catalog()));
    }
}
