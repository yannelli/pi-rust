//! The models of an OpenAI or xAI API key, `GET /models` (pi-rust extension).

use serde_json::Value;

use super::reconcile::{AddRule, Listed, reconcile};
use super::{Fetched, ModelListContext, get_json};
use crate::types::Model;

pub(crate) const OPENAI_BASE_URL: &str = "https://api.openai.com/v1";
pub(crate) const XAI_BASE_URL: &str = "https://api.x.ai/v1";

/// The catalog narrowed to the key's models. The list also names embedding, audio, image, and
/// moderation models, so only ids of a catalog model family are added.
pub(crate) async fn personalize_openai_compatible_catalog(
    catalog: Vec<Model>,
    context: &ModelListContext,
    label: &str,
    default_base_url: &str,
) -> Result<Vec<Model>, String> {
    let Some(key) = context.key() else { return Ok(catalog) };
    let url = format!("{}/models", context.base(default_base_url));
    let headers = [("authorization", format!("Bearer {key}"))];
    let Fetched::Json(body) = get_json(context, label, &url, &headers).await? else { return Ok(catalog) };
    let Some(data) = body.get("data").and_then(Value::as_array) else { return Ok(catalog) };
    let ids = data.iter().filter_map(|entry| entry.get("id").and_then(Value::as_str)).filter(|id| !id.is_empty());
    let listed: Vec<Listed> = ids.map(Listed::new).collect();
    Ok(reconcile(catalog, &listed, AddRule::CatalogFamilies))
}
