//! The models of a Cursor login, `aiserver.v1.AiService/AvailableModels` (pi-rust extension): one
//! model per Cursor model and speed, with a route for each thinking level, as Cursor's own ACP
//! server groups them. Max mode variants are left out.

use serde_json::{Map, Value as Json, json};

use super::{MODEL_LIST_TIMEOUT, ModelListContext, Personalized};
use crate::auth::oauth::random_uuid;
use crate::models::is_chat_model;
use crate::providers::cursor::proto::{Value, Writer, field, fields};
use crate::providers::cursor::{CURSOR_BASE_URL, cursor_headers};
use crate::types::{Model, ThinkingLevel};

const PATH: &str = "/aiserver.v1.AiService/AvailableModels";
const EFFORT_KEYS: [&str; 3] = ["effort", "reasoning_effort", "reasoning"];

/// A variant's legacy slug and the `RequestedModel` parameters that select it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Route {
    pub slug: String,
    pub parameters: Vec<(String, String)>,
}

/// One pi model: `model` at one speed, with the variant of each thinking level.
#[derive(Debug, PartialEq)]
pub(crate) struct CursorModel {
    pub id: String,
    pub name: String,
    pub model: String,
    pub default: Route,
    pub levels: Vec<(ThinkingLevel, Route)>,
    pub context_window: Option<u64>,
    pub images: Option<bool>,
}

struct Config {
    route: Route,
    level: Option<ThinkingLevel>,
    fast: bool,
    default: bool,
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

fn parameter<'a>(parameters: &'a [(String, String)], id: &str) -> Option<&'a str> {
    parameters.iter().find(|(key, _)| key == id).map(|(_, value)| value.as_str())
}

/// The pi level of a variant: `thinking=false` is `off`, else the effort value.
fn level(parameters: &[(String, String)]) -> Option<ThinkingLevel> {
    let effort = EFFORT_KEYS.iter().find_map(|key| parameter(parameters, key)).and_then(|effort| match effort {
        "none" => Some(ThinkingLevel::Off),
        "extra-high" | "extra_high" => Some(ThinkingLevel::Xhigh),
        other => other.parse().ok(),
    });
    match parameter(parameters, "thinking") {
        Some("false") => Some(ThinkingLevel::Off),
        Some(_) => effort.filter(|level| *level != ThinkingLevel::Off).or(Some(ThinkingLevel::High)),
        None => effort,
    }
}

/// `256k`, `1m`, or a plain number of tokens.
fn context_size(value: &str) -> Option<u64> {
    let (number, scale) = match value.to_ascii_lowercase() {
        size if size.ends_with('k') => (size.trim_end_matches('k').to_string(), 1_000),
        size if size.ends_with('m') => (size.trim_end_matches('m').to_string(), 1_000_000),
        size => (size, 1),
    };
    number.parse::<u64>().ok().map(|number| number * scale)
}

fn configs(details: &[(u32, Value)]) -> Result<Vec<Config>, String> {
    let mut out = Vec::new();
    for (_, config) in details.iter().filter(|(number, _)| *number == 30) {
        let config = fields(config.as_bytes().unwrap_or_default())?;
        let Some(slug) = text(&config, 11) else { continue };
        if flag(&config, 3) == Some(true) {
            continue;
        }
        let mut parameters = Vec::new();
        for (_, entry) in config.iter().filter(|(number, _)| *number == 1) {
            let entry = fields(entry.as_bytes().unwrap_or_default())?;
            if let Some(id) = text(&entry, 1) {
                parameters.push((id.to_string(), text(&entry, 2).unwrap_or_default().to_string()));
            }
        }
        out.push(Config {
            level: level(&parameters),
            fast: parameter(&parameters, "fast") == Some("true"),
            default: flag(&config, 5) == Some(true),
            route: Route { slug: slug.into(), parameters },
        });
    }
    Ok(out)
}

/// The models of an `AvailableModelsResponse`, without hidden models and repeated ids.
pub(crate) fn models(response: &[u8]) -> Result<Vec<CursorModel>, String> {
    let mut out: Vec<CursorModel> = Vec::new();
    for (_, details) in fields(response)?.into_iter().filter(|(number, _)| *number == 2) {
        let details = fields(details.as_bytes().unwrap_or_default())?;
        let Some(model) = text(&details, 1) else { continue };
        if flag(&details, 35) == Some(true) {
            continue;
        }
        let display = text(&details, 17).unwrap_or(model).split_whitespace().collect::<Vec<_>>().join(" ");
        let (standard, images) = (limit(&details, 15), flag(&details, 10));
        let configs = configs(&details)?;
        let default_level = configs.iter().find(|config| config.default).and_then(|config| config.level);
        let plain = Config {
            route: Route { slug: model.into(), parameters: Vec::new() },
            level: None,
            fast: false,
            default: true,
        };
        let configs = if configs.is_empty() { vec![plain] } else { configs };
        for fast in [false, true] {
            let group: Vec<&Config> = configs.iter().filter(|config| config.fast == fast).collect();
            let Some(first) = group.first() else { continue };
            let mut levels: Vec<(ThinkingLevel, Route, bool)> = Vec::new();
            for config in &group {
                let Some(level) = config.level else { continue };
                match levels.iter_mut().find(|(known, _, _)| *known == level) {
                    // A later variant of the same level replaces an earlier one, unless that is the default.
                    Some(entry) if !entry.2 => *entry = (level, config.route.clone(), config.default),
                    Some(_) => {}
                    None => levels.push((level, config.route.clone(), config.default)),
                }
            }
            levels.sort_by_key(|(level, _, _)| *level);
            let default = group
                .iter()
                .find(|config| config.default)
                .or_else(|| group.iter().find(|config| config.level.is_some() && config.level == default_level))
                .unwrap_or(first);
            let context = parameter(&default.route.parameters, "context").and_then(context_size);
            let id = if fast { format!("{model}-fast") } else { model.to_string() };
            if out.iter().any(|known| known.id == id) {
                continue;
            }
            out.push(CursorModel {
                name: if fast { format!("{display} Fast") } else { display.clone() },
                id,
                model: model.into(),
                default: default.route.clone(),
                levels: levels.into_iter().map(|(level, route, _)| (level, route)).collect(),
                context_window: context.or(standard),
                images,
            });
        }
    }
    Ok(out)
}

fn route_json(route: &Route) -> Json {
    let parameters: Vec<Json> = route.parameters.iter().map(|(id, value)| json!({"id": id, "value": value})).collect();
    json!({"slug": route.slug, "parameters": parameters})
}

/// A catalog model for `cursor`, with the catalog model of the same id (else the first one) as
/// the template for cost, limits, and base URL.
fn model_for(cursor: &CursorModel, catalog: &[Model]) -> Option<Model> {
    let chats: Vec<&Model> = catalog.iter().filter(|model| is_chat_model(model)).collect();
    let template = chats.iter().find(|model| model.id == cursor.id).or_else(|| chats.first())?;
    let mut model = Model { id: cursor.id.clone(), name: cursor.name.clone(), ..(*template).clone() };
    if let Some(window) = cursor.context_window {
        model.context_window = window;
        model.max_tokens = model.max_tokens.min(window);
    }
    if let Some(images) = cursor.images {
        model.input = if images { vec!["text".into(), "image".into()] } else { vec!["text".into()] };
    }
    let default = route_json(&cursor.default);
    let mut compat = model.compat.clone().filter(Json::is_object).unwrap_or_else(|| json!({}));
    compat["cursorModel"] = json!(cursor.model);
    compat["cursorSlug"] = default["slug"].clone();
    compat["cursorParameters"] = default["parameters"].clone();
    compat["cursorMaxMode"] = json!(false);
    model.reasoning = cursor.levels.len() > 1;
    if model.reasoning {
        let levels: Map<String, Json> =
            cursor.levels.iter().map(|(level, route)| (level.as_str().to_string(), route_json(route))).collect();
        let offered = |level: ThinkingLevel| cursor.levels.iter().any(|(known, _)| *known == level);
        model.thinking_level_map = Some(
            ThinkingLevel::ALL
                .into_iter()
                .map(|level| (level.as_str().to_string(), offered(level).then(|| level.as_str().to_string())))
                .collect(),
        );
        compat["cursorLevels"] = Json::Object(levels);
    } else {
        model.thinking_level_map = None;
        if let Some(object) = compat.as_object_mut() {
            object.remove("cursorLevels");
        }
    }
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
    let Ok(listed) = models(&bytes) else { return Ok(Personalized::Unmatched(catalog)) };
    let models: Vec<Model> = listed.iter().filter_map(|cursor| model_for(cursor, &catalog)).collect();
    Ok(if models.is_empty() { Personalized::Unmatched(catalog) } else { Personalized::Models(models) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::http_mock::{Reply, RouteServer};

    fn response() -> Vec<u8> {
        let parameter = |id: &str, value: &str| Writer::new().string(1, id).string(2, value);
        let variant = |slug: &str, effort: &str, fast: &str, context: &str| {
            Writer::new()
                .message(1, parameter("context", context))
                .message(1, parameter("reasoning_effort", effort))
                .message(1, parameter("fast", fast))
                .string(2, &format!("Grok 4.7  {effort}"))
                .string(11, slug)
        };
        let grok = Writer::new()
            .string(1, "grok-4.7")
            .bool(10, true)
            .varint(15, 256_000)
            .varint(16, 500_000)
            .string(17, "Grok  4.7")
            .message(30, variant("grok-4.7-low", "low", "false", "256k"))
            .message(30, variant("grok-4.7-low-fast", "low", "true", "256k"))
            .message(30, variant("grok-4.7-high", "high", "false", "256k"))
            .message(30, variant("grok-4.7-high-fast", "high", "true", "256k").bool(5, true))
            .message(30, variant("grok-4.7-high-max", "high", "false", "500k").bool(3, true));
        let thinking = |slug: &str, thinking: &str, effort: &str| {
            Writer::new()
                .message(1, parameter("thinking", thinking))
                .message(1, parameter("effort", effort))
                .string(11, slug)
        };
        let opus = Writer::new()
            .string(1, "claude-opus-5")
            .varint(15, 300_000)
            .string(17, "Claude Opus 5")
            .message(30, thinking("claude-opus-5-low", "false", "low"))
            .message(30, thinking("claude-opus-5-high", "false", "high"))
            .message(30, thinking("claude-opus-5-thinking-high", "true", "high").bool(5, true))
            .message(30, thinking("claude-opus-5-thinking-max", "true", "max"));
        let composer = Writer::new().string(1, "composer-2.5").bool(10, false).varint(15, 200_000);
        let hidden = Writer::new().string(1, "internal").bool(35, true);
        Writer::new().message(2, grok).message(2, opus).message(2, composer).message(2, hidden).finish()
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
        vec![model("claude-opus-5"), model("gpt-5.5")]
    }

    fn levels(model: &Model) -> Vec<&str> {
        let map = model.thinking_level_map.as_ref().unwrap();
        ThinkingLevel::ALL.iter().map(|level| level.as_str()).filter(|level| map[*level].is_some()).collect()
    }

    #[tokio::test]
    async fn variants_group_into_one_model_per_speed_with_thinking_levels() {
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
        assert_eq!(ids, ["grok-4.7", "grok-4.7-fast", "claude-opus-5", "composer-2.5"]);
        let names: Vec<&str> = models.iter().map(|model| model.name.as_str()).collect();
        assert_eq!(names, ["Grok 4.7", "Grok 4.7 Fast", "Claude Opus 5", "composer-2.5"]);

        let (grok, fast, opus, composer) = (&models[0], &models[1], &models[2], &models[3]);
        assert_eq!((grok.context_window, grok.reasoning), (256_000, true));
        assert_eq!(levels(grok), ["low", "high"]);
        let compat = grok.compat.as_ref().unwrap();
        assert_eq!(compat["cursorModel"], "grok-4.7");
        assert_eq!(compat["cursorSlug"], "grok-4.7-high");
        assert_eq!(compat["cursorLevels"]["low"]["slug"], "grok-4.7-low");
        assert_eq!(compat["cursorLevels"]["low"]["parameters"][1], json!({"id": "reasoning_effort", "value": "low"}));
        assert_eq!(fast.compat.as_ref().unwrap()["cursorSlug"], "grok-4.7-high-fast");

        assert_eq!(levels(opus), ["off", "high", "max"]);
        let opus_levels = &opus.compat.as_ref().unwrap()["cursorLevels"];
        assert_eq!(opus_levels["off"]["slug"], "claude-opus-5-high");
        assert_eq!(opus_levels["max"]["slug"], "claude-opus-5-thinking-max");
        assert_eq!(opus.context_window, 300_000);

        assert!(!composer.reasoning && composer.thinking_level_map.is_none());
        assert_eq!((composer.input.as_slice(), composer.context_window), (["text".to_string()].as_slice(), 200_000));
        assert_eq!(composer.compat.as_ref().unwrap()["cursorSlug"], "composer-2.5");

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
