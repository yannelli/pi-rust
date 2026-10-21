//! The models of an Anthropic API key or Claude subscription login, `GET /v1/models`
//! (pi-rust extension).

use serde_json::Value;

use super::reconcile::{AddRule, Listed, positive, reconcile};
use super::{ModelListContext, Personalized, get_pages, with_query};
use crate::providers::anthropic::OAUTH_API_BETA;
use crate::types::Model;

const ANTHROPIC_BASE_URL: &str = "https://api.anthropic.com";

/// Request headers like the provider's: subscription tokens use Bearer auth and the OAuth beta.
fn headers(key: &str) -> Vec<(&'static str, String)> {
    let mut headers = vec![("anthropic-version", "2023-06-01".to_string())];
    if key.contains("sk-ant-oat") {
        headers.push(("authorization", format!("Bearer {key}")));
        headers.push(("anthropic-beta", OAUTH_API_BETA.to_string()));
    } else {
        headers.push(("x-api-key", key.to_string()));
    }
    headers
}

fn account_model(entry: &Value) -> Option<Listed> {
    let id = entry.get("id").and_then(Value::as_str).filter(|id| !id.is_empty())?;
    Some(Listed {
        name: entry.get("display_name").and_then(Value::as_str).map(String::from),
        context_window: positive(entry, "/max_input_tokens"),
        max_tokens: positive(entry, "/max_tokens"),
        ..Listed::new(id)
    })
}

/// The catalog narrowed to the key's models, following `after_id` pages. A 401, 403, or 404,
/// and an unreadable page leave the catalog unchanged.
pub(crate) async fn personalize_anthropic_catalog(
    catalog: Vec<Model>,
    context: &ModelListContext,
) -> Result<Personalized, String> {
    let Some(key) = context.key() else { return Ok(Personalized::Models(catalog)) };
    let url = format!("{}/v1/models", context.base(ANTHROPIC_BASE_URL));
    let first = with_query(&url, &[("limit", "1000")])?;
    let next = |body: &Value| match (body.get("has_more"), body.get("last_id").and_then(Value::as_str)) {
        (Some(Value::Bool(true)), Some(last)) => with_query(&url, &[("limit", "1000"), ("after_id", last)]).map(Some),
        _ => Ok(None),
    };
    let Some(pages) = get_pages(context, "Anthropic", first, &headers(key), next).await? else {
        return Ok(Personalized::Models(catalog));
    };
    let mut listed = Vec::new();
    for page in &pages {
        let Some(data) = page.get("data").and_then(Value::as_array) else { return Ok(Personalized::Models(catalog)) };
        listed.extend(data.iter().filter_map(account_model));
    }
    Ok(reconcile(catalog, &listed, AddRule::Any, context.default_model.as_deref()))
}
