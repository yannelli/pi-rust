//! Per-API request options that upstream keeps on each API's options type (`GoogleVertexOptions`,
//! `AzureEndpointOptions`, `BedrockOptions`, `OpenAIResponsesOptions`, `PiMessagesOptions`).
//! Rust carries them on [`crate::StreamOptions`]; APIs that do not use a group ignore it.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

// MFA_REEXPORT

/// Reasoning summary detail for OpenAI Responses, Azure, and Codex. `off` and `on` are Codex only.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningSummary {
    Auto,
    Detailed,
    Concise,
    Off,
    On,
}

impl ReasoningSummary {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Detailed => "detailed",
            Self::Concise => "concise",
            Self::Off => "off",
            Self::On => "on",
        }
    }
}

/// Vertex AI project and location; each falls back to `GOOGLE_CLOUD_PROJECT`/`GCLOUD_PROJECT`
/// and `GOOGLE_CLOUD_LOCATION`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VertexOptions {
    pub project: Option<String>,
    pub location: Option<String>,
}

/// Azure OpenAI endpoint (`azureBaseUrl`, `azureResourceName`, `azureDeploymentName`,
/// `azureApiVersion`); each wins over its `AZURE_OPENAI_*` environment variable.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AzureOptions {
    pub base_url: Option<String>,
    pub resource_name: Option<String>,
    pub deployment_name: Option<String>,
    pub api_version: Option<String>,
}

/// How Claude thinking content comes back from Bedrock. Default `summarized`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingDisplay {
    #[default]
    Summarized,
    Omitted,
}

impl ThinkingDisplay {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Summarized => "summarized",
            Self::Omitted => "omitted",
        }
    }
}

/// Amazon Bedrock request options (upstream `BedrockOptions`).
#[derive(Clone, Default)]
pub struct BedrockOptions {
    /// Wins over `AWS_REGION`/`AWS_DEFAULT_REGION`; an inference profile ARN's region still wins.
    pub region: Option<String>,
    /// Shared config profile; wins over `AWS_PROFILE` and ambient access keys.
    pub profile: Option<String>,
    /// Bedrock API key sent as `Authorization: Bearer`, bypassing SigV4.
    pub bearer_token: Option<String>,
    /// Interleaved thinking beta for budget-based Claude models. Default true.
    pub interleaved_thinking: Option<bool>,
    pub thinking_display: Option<ThinkingDisplay>,
    /// Cost allocation tags sent as `requestMetadata`.
    pub request_metadata: Option<BTreeMap<String, String>>,
    // MFA_FIELD
}

impl std::fmt::Debug for BedrockOptions {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BedrockOptions")
            .field("region", &self.region)
            .field("profile", &self.profile)
            .field("bearer_token", &self.bearer_token.as_ref().map(|_| "<redacted>"))
            .field("interleaved_thinking", &self.interleaved_thinking)
            .field("thinking_display", &self.thinking_display)
            .field("request_metadata", &self.request_metadata)
            // MFA_DEBUG
            .finish()
    }
}
