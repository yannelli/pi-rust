//! Azure endpoint resolution (upstream `azure-openai-config.ts`). Azure models ship without a
//! base URL: each user has a resource, resolved per request from the environment.

use reqwest::Url;

use crate::registry::StreamOptions;
use crate::simple_options::provider_env_value;
use crate::types::Model;

const DEFAULT_API_VERSION: &str = "v1";
const AZURE_HOST_SUFFIXES: [&str; 3] = [".openai.azure.com", ".cognitiveservices.azure.com", ".ai.azure.com"];

/// The deployment for `model`: its entry in `AZURE_OPENAI_DEPLOYMENT_NAME_MAP`
/// (`model=deployment,...`), else the model id.
pub(crate) fn resolve_deployment_name(model: &Model, options: &StreamOptions) -> String {
    let map = provider_env_value("AZURE_OPENAI_DEPLOYMENT_NAME_MAP", &options.env).unwrap_or_default();
    map.split(',')
        .filter_map(|entry| entry.trim().split_once('='))
        .filter(|(id, deployment)| !id.is_empty() && !deployment.is_empty())
        .find(|(id, _)| id.trim() == model.id)
        .map(|(_, deployment)| deployment.split('=').next().unwrap_or_default().trim().to_string())
        .filter(|deployment| !deployment.is_empty())
        .unwrap_or_else(|| model.id.clone())
}

/// Azure hosts get `/openai/v1` as their base path; other URLs are kept as given.
fn normalize_base_url(base_url: &str) -> Result<String, String> {
    let trimmed = base_url.trim().trim_end_matches('/');
    let mut url = Url::parse(trimmed).map_err(|_| format!("Invalid Azure OpenAI base URL: {base_url}"))?;
    let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
    let azure = AZURE_HOST_SUFFIXES.iter().any(|suffix| host.ends_with(suffix));
    let path = url.path().trim_end_matches('/').to_string();
    if azure && matches!(path.as_str(), "" | "/openai" | "/openai/v1/responses") {
        url.set_path("/openai/v1");
        url.set_query(None);
    }
    Ok(url.to_string().trim_end_matches('/').to_string())
}

/// Base URL from `AZURE_OPENAI_BASE_URL`, then `AZURE_OPENAI_RESOURCE_NAME`, then the model.
pub(crate) fn resolve_base_url(model: &Model, options: &StreamOptions) -> Result<String, String> {
    let explicit = provider_env_value("AZURE_OPENAI_BASE_URL", &options.env).filter(|url| !url.trim().is_empty());
    let from_resource = || {
        provider_env_value("AZURE_OPENAI_RESOURCE_NAME", &options.env)
            .map(|name| format!("https://{name}.openai.azure.com/openai/v1"))
    };
    let from_model = || (!model.base_url.is_empty()).then(|| model.base_url.clone());
    let base = explicit.or_else(from_resource).or_else(from_model).ok_or(
        "Azure OpenAI base URL is required. Set AZURE_OPENAI_BASE_URL or AZURE_OPENAI_RESOURCE_NAME, or pass azureBaseUrl, azureResourceName, or model.baseUrl.",
    )?;
    normalize_base_url(&base)
}

/// The Responses endpoint: `{base}/responses?api-version={AZURE_OPENAI_API_VERSION or v1}`.
pub(crate) fn responses_url(model: &Model, options: &StreamOptions) -> Result<String, String> {
    let base = resolve_base_url(model, options)?;
    let version = provider_env_value("AZURE_OPENAI_API_VERSION", &options.env).unwrap_or(DEFAULT_API_VERSION.into());
    let mut url = Url::parse(&base).map_err(|_| format!("Invalid Azure OpenAI base URL: {base}"))?;
    url.set_path(&format!("{}/responses", url.path().trim_end_matches('/')));
    url.query_pairs_mut().append_pair("api-version", &version);
    Ok(url.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(pairs: &[(&str, &str)]) -> StreamOptions {
        let mut options = StreamOptions::default();
        for (name, value) in pairs {
            options.env.insert(name.to_string(), value.to_string());
        }
        options
    }

    fn base(url: &str) -> String {
        resolve_base_url(&Model::default(), &options(&[("AZURE_OPENAI_BASE_URL", url)])).unwrap()
    }

    #[test]
    fn normalizes_azure_hosts_to_openai_v1() {
        for (input, expected) in [
            ("https://r.cognitiveservices.azure.com", "https://r.cognitiveservices.azure.com/openai/v1"),
            ("https://r.ai.azure.com", "https://r.ai.azure.com/openai/v1"),
            ("https://r.openai.azure.com/", "https://r.openai.azure.com/openai/v1"),
            ("https://r.cognitiveservices.azure.com/openai", "https://r.cognitiveservices.azure.com/openai/v1"),
            ("https://r.cognitiveservices.azure.com/openai/v1", "https://r.cognitiveservices.azure.com/openai/v1"),
            ("https://r.services.ai.azure.com/openai/v1/responses", "https://r.services.ai.azure.com/openai/v1"),
            ("https://r.openai.azure.com/openai?api-version=2024-12-01", "https://r.openai.azure.com/openai/v1"),
            ("https://my-proxy.example.com/v1", "https://my-proxy.example.com/v1"),
            ("https://my-proxy.example.com/v1?custom=true", "https://my-proxy.example.com/v1?custom=true"),
        ] {
            assert_eq!(base(input), expected, "{input}");
        }
        let error = resolve_base_url(&Model::default(), &options(&[("AZURE_OPENAI_BASE_URL", "not-a-url")]));
        assert!(error.unwrap_err().contains("Invalid Azure OpenAI base URL"));
    }

    #[test]
    fn resource_names_deployments_and_api_versions() {
        let env = options(&[
            ("AZURE_OPENAI_RESOURCE_NAME", "my-resource"),
            ("AZURE_OPENAI_DEPLOYMENT_NAME_MAP", " gpt-4o-mini = mini-deploy , other=x,broken"),
            ("AZURE_OPENAI_API_VERSION", "2025-04-01-preview"),
        ]);
        let model = Model { id: "gpt-4o-mini".into(), ..Model::default() };
        if std::env::var("AZURE_OPENAI_BASE_URL").is_err() {
            assert_eq!(resolve_base_url(&model, &env).unwrap(), "https://my-resource.openai.azure.com/openai/v1");
            assert_eq!(
                responses_url(&model, &env).unwrap(),
                "https://my-resource.openai.azure.com/openai/v1/responses?api-version=2025-04-01-preview"
            );
        }
        assert_eq!(resolve_deployment_name(&model, &env), "mini-deploy");
        let unmapped = Model { id: "gpt-5".into(), ..Model::default() };
        assert_eq!(resolve_deployment_name(&unmapped, &env), "gpt-5");
    }
}
