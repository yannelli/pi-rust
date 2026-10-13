//! Provider-specific API key logins (upstream `providers/amazon-bedrock.ts` and
//! `providers/google-vertex.ts` `login`): a bearer token or key, or a pointer to credentials
//! that live outside the store, such as an AWS profile or Google Application Default Credentials.

use pi_ai::auth::{
    ApiKeyCredential, AuthError, AuthEvent, AuthInfoLink, AuthInteraction, AuthPrompt, AuthResult, PromptOption,
};
use std::collections::BTreeMap;
use tokio_util::sync::CancellationToken;

fn option(id: &str, label: &str) -> PromptOption {
    PromptOption { id: id.into(), label: label.into(), description: None }
}

fn select(message: &str, options: &[(&str, &str)]) -> AuthPrompt {
    let options = options.iter().map(|(id, label)| option(id, label)).collect();
    AuthPrompt::Select { message: message.into(), options }
}

fn text(message: &str) -> AuthPrompt {
    AuthPrompt::Text { message: message.into(), placeholder: None }
}

fn secret(message: &str) -> AuthPrompt {
    AuthPrompt::Secret { message: message.into(), placeholder: None }
}

fn info(interaction: &dyn AuthInteraction, message: &str, label: &str, url: &str) {
    let links = vec![AuthInfoLink { url: url.into(), label: Some(label.into()) }];
    interaction.notify(AuthEvent::Info { message: message.into(), links });
}

fn env(pairs: &[(&str, String)]) -> Option<BTreeMap<String, String>> {
    Some(pairs.iter().map(|(name, value)| (name.to_string(), value.clone())).collect())
}

/// Whether `provider` has its own API key login.
pub(super) fn has_custom_api_key_login(provider: &str) -> bool {
    matches!(provider, "amazon-bedrock" | "google-vertex")
}

/// Run the login of `provider`; `None` when it has no custom flow.
pub(super) async fn custom_api_key_login(
    provider: &str,
    interaction: &dyn AuthInteraction,
    cancel: &CancellationToken,
) -> AuthResult<Option<ApiKeyCredential>> {
    let ask = |prompt: AuthPrompt| interaction.prompt(prompt, cancel.clone());
    let credential = match provider {
        "amazon-bedrock" => {
            let methods = [
                ("bearer-token", "Bearer token"),
                ("aws-profile", "AWS profile"),
                ("credential-chain", "Existing AWS credential chain"),
            ];
            let method = ask(select("Select Amazon Bedrock authentication method:", &methods)).await?;
            if method == "bearer-token" {
                let key = ask(secret("Enter Amazon Bedrock bearer token")).await?;
                return Ok(Some(ApiKeyCredential { key: Some(key), env: None }));
            }
            info(
                interaction,
                "Amazon Bedrock supports AWS profiles, IAM credentials, and role-based credentials.",
                "AWS credential provider chain",
                "https://docs.aws.amazon.com/sdkref/latest/guide/standardized-credentials.html",
            );
            match method.as_str() {
                "aws-profile" => {
                    let profile = ask(text("Enter AWS profile name")).await?;
                    ApiKeyCredential { key: None, env: env(&[("AWS_PROFILE", profile)]) }
                }
                "credential-chain" => {
                    ask(text("Configure AWS credentials, then press Enter to continue")).await?;
                    ApiKeyCredential::default()
                }
                other => return Err(AuthError::other(format!("Unknown Amazon Bedrock auth method: {other}"))),
            }
        }
        "google-vertex" => {
            let methods = [
                ("api-key", "Google Cloud API key"),
                ("adc", "Application Default Credentials"),
                ("service-account", "Service account credentials file"),
            ];
            let method = ask(select("Select Google Vertex AI authentication method:", &methods)).await?;
            if method == "api-key" {
                let key = ask(secret("Enter Google Cloud API key")).await?;
                return Ok(Some(ApiKeyCredential { key: Some(key), env: None }));
            }
            let message = match method.as_str() {
                "adc" => "Run `gcloud auth application-default login`, then provide the project and location.",
                "service-account" => "Provide a service account credentials file, project, and location.",
                other => return Err(AuthError::other(format!("Unknown Google Vertex AI auth method: {other}"))),
            };
            let adc_url = "https://cloud.google.com/docs/authentication/provide-credentials-adc";
            info(interaction, message, "Application Default Credentials", adc_url);
            let project = ask(text("Enter Google Cloud project ID")).await?;
            let location = ask(text("Enter Google Cloud location")).await?;
            let mut pairs = vec![("GOOGLE_CLOUD_PROJECT", project), ("GOOGLE_CLOUD_LOCATION", location)];
            if method == "service-account" {
                pairs.push((
                    "GOOGLE_APPLICATION_CREDENTIALS",
                    ask(text("Enter service account credentials file path")).await?,
                ));
            }
            ApiKeyCredential { key: None, env: env(&pairs) }
        }
        _ => return Ok(None),
    };
    Ok(Some(credential))
}
