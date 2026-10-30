//! The models of a Google Antigravity login, from the installed Antigravity ACP server (pi-rust
//! extension): the server's `model` selector, with the `thought_level` values of each model.

use std::time::Duration;

use super::{ModelListContext, Personalized};
use crate::models::is_chat_model;
use crate::providers::antigravity_acp::{self as acp, OfferedModel};
use crate::types::{Model, ThinkingLevel};

/// Starting the server and a session takes longer than one HTTP model list request.
const LIST_TIMEOUT: Duration = Duration::from_secs(90);

fn level(value: &str) -> Option<ThinkingLevel> {
    match value {
        "extra_high" | "extra-high" => Some(ThinkingLevel::Xhigh),
        other => other.parse().ok(),
    }
}

/// A catalog model for `offered`, with the catalog model of the same id (else the first one) as
/// the template for limits and base URL.
fn model_for(offered: &OfferedModel, catalog: &[Model]) -> Option<Model> {
    let chats: Vec<&Model> = catalog.iter().filter(|model| is_chat_model(model)).collect();
    let template = chats.iter().find(|model| model.id == offered.id).or_else(|| chats.first())?;
    let mut model = Model { id: offered.id.clone(), name: offered.name.clone(), ..(*template).clone() };
    let levels: Vec<(ThinkingLevel, &str)> =
        offered.levels.iter().filter_map(|value| Some((level(value)?, value.as_str()))).collect();
    model.reasoning = !levels.is_empty();
    model.thinking_level_map = model.reasoning.then(|| {
        ThinkingLevel::ALL
            .into_iter()
            .map(|level| {
                let value = levels.iter().find(|(known, _)| *known == level).map(|(_, value)| value.to_string());
                (level.as_str().to_string(), value)
            })
            .collect()
    });
    Some(model)
}

/// The catalog replaced by the models that the server offers. Without a login or an installed
/// server, the catalog stays unchanged.
pub(crate) async fn personalize_antigravity_catalog(
    catalog: Vec<Model>,
    context: &ModelListContext,
) -> Result<Personalized, String> {
    if context.key().is_none() {
        return Ok(Personalized::Models(catalog));
    }
    let failed = |reason: &str| format!("Antigravity model list failed: {reason}");
    let offered = tokio::select! {
        () = context.cancel.cancelled() => return Err(failed("cancelled")),
        result = tokio::time::timeout(LIST_TIMEOUT, acp::offered_models()) => match result {
            Err(_) => return Err(failed("timed out")),
            Ok(Err(error)) if error == acp::NOT_INSTALLED => return Ok(Personalized::Models(catalog)),
            Ok(result) => result.map_err(|error| failed(&error))?,
        },
    };
    let models: Vec<Model> = offered.iter().filter_map(|offered| model_for(offered, &catalog)).collect();
    Ok(if models.is_empty() { Personalized::Unmatched(catalog) } else { Personalized::Models(models) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offered_levels_become_the_thinking_level_map() {
        let template = Model {
            id: "gemini-3.1-pro".into(),
            provider: "google-antigravity".into(),
            api: "antigravity-acp".into(),
            context_window: 1_048_576,
            model_type: Some("chat".into()),
            ..Model::default()
        };
        let offered = |id: &str, levels: &[&str]| OfferedModel {
            id: id.into(),
            name: format!("{id} name"),
            levels: levels.iter().map(|level| level.to_string()).collect(),
        };
        let pro =
            model_for(&offered("gemini-3.1-pro", &["low", "extra_high", "unknown"]), std::slice::from_ref(&template))
                .unwrap();
        let map = pro.thinking_level_map.as_ref().unwrap();
        assert!(pro.reasoning);
        assert_eq!(
            (map["low"].as_deref(), map["xhigh"].as_deref(), map["high"].as_deref()),
            (Some("low"), Some("extra_high"), None)
        );
        let other = model_for(&offered("gemini-4-flash", &[]), &[template]).unwrap();
        assert_eq!(
            (other.id.as_str(), other.name.as_str(), other.context_window),
            ("gemini-4-flash", "gemini-4-flash name", 1_048_576)
        );
        assert!(!other.reasoning && other.thinking_level_map.is_none());
    }
}
