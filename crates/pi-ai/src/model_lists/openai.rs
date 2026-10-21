//! The models of an OpenAI or xAI API key, or of a ChatGPT login for OpenAI, `GET /models`
//! (pi-rust extension).

use serde_json::Value;

use super::reconcile::{AddRule, Listed, positive, reconcile};
use super::{Fetched, ModelListContext, Personalized, get_json};
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
) -> Result<Personalized, String> {
    let Some(key) = context.key() else { return Ok(Personalized::Models(catalog)) };
    let url = format!("{}/models", context.base(default_base_url));
    let headers = [("authorization", format!("Bearer {key}"))];
    let Fetched::Json(body) = get_json(context, label, &url, &headers).await? else {
        return Ok(Personalized::Models(catalog));
    };
    let listed: Vec<Listed> = if let Some(models) = body.get("models").and_then(Value::as_array) {
        models.iter().filter_map(chatgpt_listed).collect()
    } else if let Some(data) = body.get("data").and_then(Value::as_array) {
        let ids = data.iter().filter_map(|entry| entry.get("id").and_then(Value::as_str)).filter(|id| !id.is_empty());
        ids.map(Listed::new).collect()
    } else {
        return Ok(Personalized::Models(catalog));
    };
    Ok(reconcile(catalog, &listed, AddRule::CatalogFamilies, context.default_model.as_deref()))
}

/// An entry of a ChatGPT login's list: `slug` is the id and `context_window` the account's
/// default context size. Its larger `max_context_window` is opt-in, so it is not used.
fn chatgpt_listed(entry: &Value) -> Option<Listed> {
    let id = entry.get("slug").and_then(Value::as_str).filter(|id| !id.is_empty())?;
    Some(Listed { context_window: positive(entry, "/context_window"), ..Listed::new(id) })
}
