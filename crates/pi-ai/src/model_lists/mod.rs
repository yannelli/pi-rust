//! Account model lists: narrow a provider catalog to the models a key or account may use.
//! OpenRouter follows upstream `personalizeOpenRouterCatalog`; the other lists are pi-rust
//! extensions.

use std::time::Duration;

use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::types::Model;

mod anthropic;
mod codex;
mod copilot;
mod cursor;
mod google;
mod openai;
mod openrouter;
mod reconcile;

pub use openrouter::{OPENROUTER_BASE_URL, openrouter_cost, personalize_openrouter_catalog};

/// What a model list made of the catalog.
#[derive(Clone, Debug, PartialEq)]
pub enum Personalized {
    /// The models for this credential, to show and save.
    Models(Vec<Model>),
    /// The list names no catalog chat model, as an unrelated list behind a proxy can: show the
    /// catalog unchanged and save nothing.
    Unmatched(Vec<Model>),
}

impl Personalized {
    pub fn models(&self) -> &[Model] {
        match self {
            Self::Models(models) | Self::Unmatched(models) => models,
        }
    }

    pub fn into_models(self) -> Vec<Model> {
        match self {
            Self::Models(models) | Self::Unmatched(models) => models,
        }
    }
}

/// Time limit of one model list request, as upstream.
pub const MODEL_LIST_TIMEOUT: Duration = Duration::from_secs(15);

/// What a model list request needs. The key is only sent in a header; it is never part of a
/// URL, an error, or a returned model.
#[derive(Clone)]
pub struct ModelListContext {
    /// API key or OAuth access token. Without one the catalog stays as it is.
    pub key: Option<String>,
    /// The provider base URL set in `models.json`; the provider default when `None`.
    pub base_url: Option<String>,
    /// Must not follow redirects, which would carry the key to another host ([`list_client`]).
    pub client: reqwest::Client,
    /// The provider's default model. A list without it only enriches the catalog and drops
    /// nothing, since the list may be filtered for another client.
    pub default_model: Option<String>,
    pub user_agent: Option<String>,
    pub cancel: CancellationToken,
}

impl ModelListContext {
    pub fn new(key: Option<String>) -> Self {
        Self {
            key,
            base_url: None,
            client: list_client(),
            default_model: None,
            user_agent: Some(crate::identity::format_user_agent(None)),
            cancel: CancellationToken::new(),
        }
    }

    pub(crate) fn key(&self) -> Option<&str> {
        self.key.as_deref().filter(|key| !key.is_empty())
    }

    /// The configured base URL without trailing slashes, else `default`.
    pub(crate) fn base(&self, default: &str) -> String {
        let configured = self.base_url.as_deref().filter(|url| !url.trim().is_empty());
        configured.unwrap_or(default).trim_end_matches('/').to_string()
    }
}

/// The client for model list requests: it returns a redirect as a failing status.
pub fn list_client() -> reqwest::Client {
    crate::http::no_redirect_client()
}

/// Providers whose catalog follows the account's model list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccountModelList {
    /// Upstream: `GET /models/user`.
    OpenRouter,
    /// The ChatGPT account's Codex models.
    OpenAiCodex,
    /// The models of an Anthropic API key or subscription login.
    Anthropic,
    /// The chat models a GitHub Copilot login's model picker offers.
    GitHubCopilot,
    /// The models of an OpenAI API key.
    OpenAi,
    /// The models of an xAI API key.
    Xai,
    /// The `generateContent` models of a Gemini API key.
    Google,
    /// The model variants of a Cursor login, with their context limits.
    Cursor,
}

impl AccountModelList {
    pub fn for_provider(provider_id: &str) -> Option<Self> {
        match provider_id {
            "openrouter" => Some(Self::OpenRouter),
            "openai-codex" => Some(Self::OpenAiCodex),
            "anthropic" => Some(Self::Anthropic),
            "github-copilot" => Some(Self::GitHubCopilot),
            "openai" => Some(Self::OpenAi),
            "xai" => Some(Self::Xai),
            "google" => Some(Self::Google),
            "cursor" => Some(Self::Cursor),
            _ => None,
        }
    }

    /// Whether upstream pi has this list; the others are pi-rust extensions.
    pub fn is_upstream(self) -> bool {
        matches!(self, Self::OpenRouter)
    }

    /// The catalog narrowed to the account's models. An error means the list could not be read
    /// and nothing should replace the last saved list.
    pub async fn personalize(self, catalog: Vec<Model>, context: &ModelListContext) -> Result<Personalized, String> {
        match self {
            Self::OpenRouter => personalize_openrouter_catalog(catalog, context).await.map(Personalized::Models),
            Self::OpenAiCodex => codex::personalize_codex_catalog(catalog, context).await,
            Self::Anthropic => anthropic::personalize_anthropic_catalog(catalog, context).await,
            Self::GitHubCopilot => copilot::personalize_copilot_catalog(catalog, context).await,
            Self::OpenAi => {
                openai::personalize_openai_compatible_catalog(catalog, context, "OpenAI", openai::OPENAI_BASE_URL).await
            }
            Self::Xai => {
                openai::personalize_openai_compatible_catalog(catalog, context, "xAI", openai::XAI_BASE_URL).await
            }
            Self::Google => google::personalize_google_catalog(catalog, context).await,
            Self::Cursor => cursor::personalize_cursor_catalog(catalog, context).await,
        }
    }
}

/// The outcome of one model list request.
pub(crate) enum Fetched {
    Json(Value),
    /// 401, 403, or 404: the endpoint rejects the key or does not exist (for example a proxy).
    Rejected,
    /// A success status with a body that is not JSON.
    Invalid,
}

/// `GET url` with the context's user agent and `headers`. Other failing statuses, redirects
/// included, transport errors, and the time limit are errors labeled with `label`.
pub(crate) async fn get_json(
    context: &ModelListContext,
    label: &str,
    url: &str,
    headers: &[(&str, String)],
) -> Result<Fetched, String> {
    let mut request = context.client.get(url).header("accept", "application/json");
    let own_agent = headers.iter().any(|(name, _)| name.eq_ignore_ascii_case("user-agent"));
    if let Some(agent) = context.user_agent.as_ref().filter(|_| !own_agent) {
        request = request.header("user-agent", agent);
    }
    for (name, value) in headers {
        request = request.header(*name, value);
    }
    let exchange = async {
        let response = request.send().await?;
        let status = response.status().as_u16();
        Ok::<_, reqwest::Error>((status, response.bytes().await?))
    };
    let failed = |reason: String| format!("{label} model list request failed: {reason}");
    let (status, body) = tokio::select! {
        () = context.cancel.cancelled() => return Err(failed("cancelled".into())),
        result = tokio::time::timeout(MODEL_LIST_TIMEOUT, exchange) => match result {
            Err(_) => return Err(failed("timed out".into())),
            Ok(result) => result.map_err(|error| failed(error.to_string()))?,
        },
    };
    if matches!(status, 401 | 403 | 404) {
        return Ok(Fetched::Rejected);
    }
    if !(200..300).contains(&status) {
        return Err(failed(status.to_string()));
    }
    Ok(serde_json::from_slice(&body).map_or(Fetched::Invalid, Fetched::Json))
}

/// Most pages read from one paginated list; a longer list leaves the catalog unchanged.
const MAX_PAGES: usize = 20;

/// `url` with `pairs` appended to its query.
pub(crate) fn with_query(url: &str, pairs: &[(&str, &str)]) -> Result<String, String> {
    let mut url = reqwest::Url::parse(url).map_err(|error| format!("Invalid model list URL {url}: {error}"))?;
    url.query_pairs_mut().extend_pairs(pairs);
    Ok(url.into())
}

/// The bodies of every page, where `next` gives the URL after a page. `None` when a page is
/// rejected or unreadable, or the pages do not end.
pub(crate) async fn get_pages(
    context: &ModelListContext,
    label: &str,
    first: String,
    headers: &[(&str, String)],
    next: impl Fn(&Value) -> Result<Option<String>, String>,
) -> Result<Option<Vec<Value>>, String> {
    let mut bodies = Vec::new();
    let mut url = Some(first);
    while let Some(current) = url.take() {
        if bodies.len() == MAX_PAGES {
            return Ok(None);
        }
        let Fetched::Json(body) = get_json(context, label, &current, headers).await? else { return Ok(None) };
        url = next(&body)?;
        bodies.push(body);
    }
    Ok(Some(bodies))
}

#[cfg(test)]
mod tests_accounts;
#[cfg(test)]
mod tests_guards;
#[cfg(test)]
mod tests_openrouter;
