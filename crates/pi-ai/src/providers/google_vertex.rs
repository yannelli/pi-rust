//! Google Vertex AI (`.../publishers/google/models/{id}:streamGenerateContent?alt=sse`) with an
//! API key (Vertex express mode) or Application Default Credentials.

mod auth;
#[cfg(test)]
mod tests;

use crate::http::pi_user_agent;
use crate::providers::common::{HttpRequest, header_map};
use crate::providers::google::{build_params, model_path, spawn_google, to_wire};
use crate::registry::{ApiProvider, StreamOptions};
use crate::request::apply_payload_hook;
use crate::simple_options::provider_env_value;
use crate::stream::{AssistantMessageEventStream, error_stream};
use crate::types::{Context, Model, StopReason};

const API_VERSION: &str = "v1";
/// Credential placeholder meaning "use Application Default Credentials".
const GCP_VERTEX_CREDENTIALS_MARKER: &str = "gcp-vertex-credentials";
const GLOBAL_ENDPOINT: &str = "https://aiplatform.googleapis.com";

pub struct GoogleVertex;

impl ApiProvider for GoogleVertex {
    fn stream(&self, model: &Model, context: &Context, options: StreamOptions) -> AssistantMessageEventStream {
        let api_key = resolve_api_key(&options);
        let scope = match &api_key {
            Some(_) => None,
            None => match resolve_project(&options).and_then(|project| Ok((project, resolve_location(&options)?))) {
                Ok(scope) => Some(scope),
                Err(error) => return error_stream(model, StopReason::Error, error),
            },
        };
        let params = match build_params(model, context, &options, true) {
            Ok(params) => params,
            Err(error) => return error_stream(model, StopReason::Error, error),
        };
        let prepare = {
            let (model, options) = (model.clone(), options.clone());
            async move {
                let params = apply_payload_hook(params, options.on_payload.as_ref(), &model);
                let (model_id, body) = to_wire(&params)?;
                let path = model_path(&model_id, true)?;
                let url = vertex_url(&model, scope.as_ref(), &path);
                let auth = match api_key {
                    Some(key) => vec![("user-agent", pi_user_agent()), ("x-goog-api-key", key)],
                    None => {
                        let token = auth::application_default_token(&options).await?;
                        let bearer = format!("Bearer {}", token.token);
                        let mut auth = vec![("user-agent", pi_user_agent()), ("authorization", bearer)];
                        if let Some(quota) = token.quota_project {
                            auth.push(("x-goog-user-project", quota));
                        }
                        auth
                    }
                };
                Ok(HttpRequest { url, headers: header_map(auth, &model, &options), body })
            }
        };
        spawn_google(model, options, "Google Vertex", prepare)
    }
}

/// The request API key, unless it is empty, the ADC marker, or a `<placeholder>`.
fn resolve_api_key(options: &StreamOptions) -> Option<String> {
    let key = options.api_key.as_deref()?.trim();
    let placeholder =
        key.len() > 2 && key.starts_with('<') && key.ends_with('>') && !key[1..key.len() - 1].contains('>');
    (!key.is_empty() && key != GCP_VERTEX_CREDENTIALS_MARKER && !placeholder).then(|| key.to_string())
}

fn resolve_project(options: &StreamOptions) -> Result<String, String> {
    let project = options.vertex.project.clone().filter(|project| !project.is_empty());
    project
        .or_else(|| provider_env_value("GOOGLE_CLOUD_PROJECT", &options.env))
        .or_else(|| provider_env_value("GCLOUD_PROJECT", &options.env))
        .ok_or_else(|| {
            "Vertex AI requires a project ID. Set GOOGLE_CLOUD_PROJECT/GCLOUD_PROJECT or pass project in options."
                .into()
        })
}

fn resolve_location(options: &StreamOptions) -> Result<String, String> {
    let location = options.vertex.location.clone().filter(|location| !location.is_empty());
    location
        .or_else(|| provider_env_value("GOOGLE_CLOUD_LOCATION", &options.env))
        .ok_or_else(|| "Vertex AI requires a location. Set GOOGLE_CLOUD_LOCATION or pass location in options.".into())
}

/// A model base URL used as-is, unless empty or a `{location}` template.
fn custom_base_url(model: &Model) -> Option<&str> {
    let base = model.base_url.trim();
    (!base.is_empty() && !base.contains("{location}")).then_some(base)
}

/// Whether a path segment of `base_url` is an API version such as `v1` or `v1beta1`.
fn includes_api_version(base_url: &str) -> bool {
    let after_scheme = base_url.split_once("://").map_or(base_url, |(_, rest)| rest);
    let path = after_scheme.split_once('/').map_or("", |(_, path)| path);
    let path = path.split(['?', '#']).next().unwrap_or_default();
    path.split('/').any(|segment| {
        let Some(rest) = segment.strip_prefix('v') else { return false };
        let digits = rest.chars().take_while(char::is_ascii_digit).count();
        let tail = &rest[digits..];
        digits > 0
            && (tail.is_empty() || tail.strip_prefix("beta").is_some_and(|n| n.chars().all(|c| c.is_ascii_digit())))
    })
}

/// Endpoint for the project's location, as the SDK chooses it.
fn location_endpoint(location: &str) -> String {
    match location {
        "global" => GLOBAL_ENDPOINT.to_string(),
        "us" | "eu" => format!("https://aiplatform.{location}.rep.googleapis.com"),
        _ => format!("https://{location}-aiplatform.googleapis.com"),
    }
}

/// Request URL. A custom base URL is a collection scope, so no project path is added.
fn vertex_url(model: &Model, scope: Option<&(String, String)>, path: &str) -> String {
    let mut parts: Vec<String> = Vec::new();
    match (custom_base_url(model), scope) {
        (Some(base), _) => {
            parts.push(base.trim_end_matches('/').to_string());
            if !includes_api_version(base) {
                parts.push(API_VERSION.into());
            }
        }
        (None, Some((project, location))) => {
            parts.push(location_endpoint(location));
            parts.push(API_VERSION.into());
            if !path.starts_with("projects/") {
                parts.push(format!("projects/{project}/locations/{location}"));
            }
        }
        (None, None) => parts.extend([GLOBAL_ENDPOINT.to_string(), API_VERSION.to_string()]),
    }
    parts.push(path.to_string());
    format!("{}:streamGenerateContent?alt=sse", parts.join("/"))
}

#[cfg(test)]
mod option_tests {
    use super::*;
    use crate::provider_options::VertexOptions;

    #[test]
    fn request_project_and_location_win_over_the_environment() {
        let mut options = StreamOptions::default();
        options.env.insert("GOOGLE_CLOUD_PROJECT".into(), "env-project".into());
        options.env.insert("GOOGLE_CLOUD_LOCATION".into(), "env-location".into());
        assert_eq!(resolve_project(&options).unwrap(), "env-project");
        assert_eq!(resolve_location(&options).unwrap(), "env-location");
        options.vertex = VertexOptions { project: Some("test-project".into()), location: Some("us-central1".into()) };
        assert_eq!(resolve_project(&options).unwrap(), "test-project");
        assert_eq!(resolve_location(&options).unwrap(), "us-central1");
        let scope = ("test-project".to_string(), "us-central1".to_string());
        assert_eq!(
            vertex_url(&Model::default(), Some(&scope), "publishers/google/models/gemini"),
            "https://us-central1-aiplatform.googleapis.com/v1/projects/test-project/locations/us-central1/publishers/google/models/gemini:streamGenerateContent?alt=sse"
        );
    }
}
