//! The models of a Gemini API key, `GET /models` (pi-rust extension).

use serde_json::Value;

use super::reconcile::{AddRule, Listed, positive, reconcile};
use super::{ModelListContext, Personalized, get_pages, with_query};
use crate::types::Model;

const GOOGLE_BASE_URL: &str = "https://generativelanguage.googleapis.com/v1beta";

/// An entry that supports `generateContent`, with the `models/` prefix removed from its name.
fn account_model(entry: &Value) -> Option<Listed> {
    let methods = entry.get("supportedGenerationMethods").and_then(Value::as_array)?;
    if !methods.iter().any(|method| method == "generateContent") {
        return None;
    }
    let name = entry.get("name").and_then(Value::as_str)?;
    let id = name.strip_prefix("models/").unwrap_or(name);
    (!id.is_empty()).then(|| Listed {
        name: entry.get("displayName").and_then(Value::as_str).map(String::from),
        context_window: positive(entry, "/inputTokenLimit"),
        max_tokens: positive(entry, "/outputTokenLimit"),
        ..Listed::new(id)
    })
}

/// The catalog narrowed to the key's `generateContent` models, following `nextPageToken`.
/// A 401, 403, or 404 and an unreadable page leave the catalog unchanged.
pub(crate) async fn personalize_google_catalog(
    catalog: Vec<Model>,
    context: &ModelListContext,
) -> Result<Personalized, String> {
    let Some(key) = context.key() else { return Ok(Personalized::Models(catalog)) };
    let url = format!("{}/models", context.base(GOOGLE_BASE_URL));
    let first = with_query(&url, &[("pageSize", "1000")])?;
    let next = |body: &Value| match body.get("nextPageToken").and_then(Value::as_str).filter(|token| !token.is_empty())
    {
        Some(token) => with_query(&url, &[("pageSize", "1000"), ("pageToken", token)]).map(Some),
        None => Ok(None),
    };
    let headers = [("x-goog-api-key", key.to_string())];
    let Some(pages) = get_pages(context, "Google", first, &headers, next).await? else {
        return Ok(Personalized::Models(catalog));
    };
    let mut listed = Vec::new();
    for page in &pages {
        let Some(models) = page.get("models").and_then(Value::as_array) else {
            return Ok(Personalized::Models(catalog));
        };
        listed.extend(models.iter().filter_map(account_model));
    }
    Ok(reconcile(catalog, &listed, AddRule::CatalogFamilies, context.default_model.as_deref()))
}
