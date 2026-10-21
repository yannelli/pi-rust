//! The models of a GitHub Copilot login, `GET /models` (pi-rust extension).

use serde_json::Value;

use super::reconcile::{AddRule, Listed, positive, reconcile};
use super::{Fetched, ModelListContext, get_json};
use crate::auth::oauth::{
    COPILOT_API_VERSION, COPILOT_HEADERS, COPILOT_INDIVIDUAL_BASE_URL, github_copilot_base_url, parse_copilot_catalog,
};
use crate::types::Model;

/// The token's proxy endpoint, which requests use too, else the configured base URL.
fn base_url(key: &str, context: &ModelListContext) -> String {
    if key.contains("proxy-ep=") {
        github_copilot_base_url(Some(key), None)
    } else {
        context.base(COPILOT_INDIVIDUAL_BASE_URL)
    }
}

fn account_model(entry: &Value) -> Option<Listed> {
    let id = entry.get("id").and_then(Value::as_str).filter(|id| !id.is_empty())?;
    Some(Listed {
        name: entry.get("name").and_then(Value::as_str).map(String::from),
        context_window: positive(entry, "/capabilities/limits/max_context_window_tokens"),
        max_tokens: positive(entry, "/capabilities/limits/max_output_tokens"),
        ..Listed::new(id)
    })
}

/// The catalog narrowed to the chat models the login's picker offers, chosen as `/login` does.
/// A 401, 403, or 404 and an unreadable body leave the catalog unchanged.
pub(crate) async fn personalize_copilot_catalog(
    catalog: Vec<Model>,
    context: &ModelListContext,
) -> Result<Vec<Model>, String> {
    let Some(key) = context.key() else { return Ok(catalog) };
    let base = base_url(key, context);
    let mut headers: Vec<(&str, String)> =
        COPILOT_HEADERS.iter().map(|(name, value)| (*name, value.to_string())).collect();
    headers.push(("authorization", format!("Bearer {key}")));
    headers.push(("X-GitHub-Api-Version", COPILOT_API_VERSION.to_string()));
    let url = format!("{base}/models");
    let Fetched::Json(body) = get_json(context, "GitHub Copilot", &url, &headers).await? else { return Ok(catalog) };
    let allow_policy_fallback = base == COPILOT_INDIVIDUAL_BASE_URL;
    let Ok(account) = parse_copilot_catalog(&body, allow_policy_fallback, &|_| false) else { return Ok(catalog) };
    let data = body.get("data").and_then(Value::as_array).map_or(&[][..], Vec::as_slice);
    let is_chat = |entry: &&Value| entry.pointer("/capabilities/type").and_then(Value::as_str) == Some("chat");
    let listed: Vec<Listed> = data
        .iter()
        .filter(is_chat)
        .filter_map(account_model)
        .filter(|model| account.available_model_ids.contains(&model.id))
        .collect();
    Ok(reconcile(catalog, &listed, AddRule::Any))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tokens_proxy_endpoint_comes_before_the_configured_base_url() {
        let context = ModelListContext { base_url: Some("http://127.0.0.1:9/".into()), ..ModelListContext::new(None) };
        let token = "tid=1;exp=2;proxy-ep=proxy.business.githubcopilot.com;st=x";
        assert_eq!(base_url(token, &context), "https://api.business.githubcopilot.com");
        assert_eq!(base_url("plain-token", &context), "http://127.0.0.1:9");
    }
}
