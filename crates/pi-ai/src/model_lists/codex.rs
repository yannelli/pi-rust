//! The model list of a ChatGPT account from the Codex backend, `GET {base}/codex/models`
//! (pi-rust extension). The response is not a public contract, so it is read tolerantly.

use serde_json::Value;

use super::reconcile::{AddRule, Listed, positive, reconcile};
use super::{Fetched, ModelListContext, Personalized, get_json};
use crate::providers::openai_codex::{DEFAULT_CODEX_BASE_URL, chatgpt_account_id};
use crate::types::Model;

/// The backend lists only the models of this client version, so pi sends a Codex CLI release
/// (2026-10-29) in place of its own version.
const CODEX_CLIENT_VERSION: &str = "0.166.0";

/// `{base}/codex/models`, accepting bases that already end in `/codex` or `/codex/responses`.
fn models_url(base_url: &str) -> String {
    let base = base_url.trim_end_matches('/');
    let base = base.strip_suffix("/responses").filter(|base| base.ends_with("/codex")).unwrap_or(base);
    match base.ends_with("/codex") {
        true => format!("{base}/models"),
        false => format!("{base}/codex/models"),
    }
}

/// A listed or visible model; hidden ones are skipped. `slug` is the id, `id` its fallback.
fn listed(entry: &Value) -> Option<Listed> {
    let text = |name: &str| entry.get(name).and_then(Value::as_str).filter(|text| !text.is_empty());
    let id = text("slug").or_else(|| text("id"))?;
    let visibility = text("visibility").map(str::to_ascii_lowercase);
    if visibility.is_some_and(|visibility| matches!(visibility.as_str(), "hide" | "hidden" | "none")) {
        return None;
    }
    let context_window = ["/context_window", "/max_context_window", "/contextWindow"]
        .iter()
        .find_map(|pointer| positive(entry, pointer));
    let service_tiers = entry.get("service_tiers").and_then(Value::as_array).map(|tiers| {
        tiers.iter().filter_map(|tier| tier.get("id").and_then(Value::as_str)).map(String::from).collect()
    });
    Some(Listed { name: text("display_name").map(String::from), context_window, service_tiers, ..Listed::new(id) })
}

/// The catalog narrowed to the account's Codex models. A token without a ChatGPT account, a
/// 401, 403, or 404, and an unreadable body leave the catalog unchanged.
pub(crate) async fn personalize_codex_catalog(
    catalog: Vec<Model>,
    context: &ModelListContext,
) -> Result<Personalized, String> {
    let Some(token) = context.key() else { return Ok(Personalized::Models(catalog)) };
    let Some(account) = chatgpt_account_id(token) else { return Ok(Personalized::Models(catalog)) };
    let url = format!("{}?client_version={CODEX_CLIENT_VERSION}", models_url(&context.base(DEFAULT_CODEX_BASE_URL)));
    let headers =
        [("authorization", format!("Bearer {token}")), ("chatgpt-account-id", account), ("originator", "pi".into())];
    let Fetched::Json(body) = get_json(context, "OpenAI Codex", &url, &headers).await? else {
        return Ok(Personalized::Models(catalog));
    };
    let Some(entries) = body.get("models").and_then(Value::as_array) else { return Ok(Personalized::Models(catalog)) };
    let listed: Vec<Listed> = entries.iter().filter_map(listed).collect();
    Ok(reconcile(catalog, &listed, AddRule::Any, context.default_model.as_deref()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn models_url_accepts_codex_bases() {
        assert_eq!(models_url("https://chatgpt.com/backend-api/"), "https://chatgpt.com/backend-api/codex/models");
        assert_eq!(models_url("http://proxy/codex"), "http://proxy/codex/models");
        assert_eq!(models_url("http://proxy/codex/responses"), "http://proxy/codex/models");
    }
}
