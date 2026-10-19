//! `pi auth check|print-api-key|print-bearer-token` (upstream `cli/auth-command.ts`,
//! `cli/auth-check.ts`, and `cli/credential-print.ts`).

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use pi_ai::auth::{
    AuthOverrides, AuthResolver, AuthType, Credential, CredentialStore, FileCredentialStore, OAuthLookup, ResolvedAuth,
};
use pi_ai::{Model, ModelRegistry};
use serde_json::{Map, Value, json};

use crate::app::configured_providers;
use crate::cli::{Args, parse_args};
use crate::config::{auth_path, models_path};
use crate::model_resolver::{
    ModelCatalog, ModelReferenceRequest, ModelReferenceResult, ThinkingSuffix, resolve_cli_model,
    resolve_model_reference,
};

const DEFAULT_BEARER_TOKEN_MIN_EXPIRY_MS: i64 = 30 * 60_000;

pub const AUTH_HELP: &str = "Usage:
  pi auth print-api-key [--provider <provider>] [--model <model>]
  pi auth print-bearer-token [--provider <provider>] [--model <model>] [--min-expiry <duration>]
  pi auth check [--provider <provider>] [--model <model>] [--json] [--credentials] [--no-refresh]

Auth commands require at least one of --provider or --model. Checks refresh expired OAuth credentials by default; --no-refresh prevents this. --credentials emits the credential, or includes it in JSON output.";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthCommandKind {
    Check,
    ApiKey,
    BearerToken,
}

impl AuthCommandKind {
    pub fn name(self) -> &'static str {
        match self {
            Self::Check => "auth check",
            Self::ApiKey => "auth print-api-key",
            Self::BearerToken => "auth print-bearer-token",
        }
    }

    pub fn usage(self) -> &'static str {
        match self {
            Self::Check => "pi auth check --provider <provider> [--json] [--credentials] [--no-refresh]",
            Self::ApiKey => "pi auth print-api-key --provider <provider> [--model <model>]",
            Self::BearerToken => {
                "pi auth print-bearer-token --provider <provider> [--model <model>] [--min-expiry <duration>]"
            }
        }
    }
}

/// A parsed `pi auth` command; `args` are the remaining flags for [`parse_args`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthCommand {
    pub kind: AuthCommandKind,
    pub args: Vec<String>,
    pub json: bool,
    pub credentials: bool,
    pub no_refresh: bool,
    pub min_expiry_ms: Option<i64>,
}

/// Exit code and output of an auth command.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AuthOutput {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl AuthOutput {
    fn error(code: i32, message: &str) -> Self {
        Self { code, stderr: format!("Error: {message}\n"), ..Self::default() }
    }
}

pub fn is_auth_command_help(args: &[String]) -> bool {
    args.first().is_some_and(|first| first == "auth")
        && (args.get(1).is_none_or(|second| second == "help") || args.iter().any(|arg| arg == "--help" || arg == "-h"))
}

/// `30m`, `1h`, `45s`, or `500ms` in milliseconds.
pub fn parse_min_expiry(value: &str) -> Option<i64> {
    let lower = value.to_ascii_lowercase();
    let split = lower.find(|c: char| !c.is_ascii_digit())?;
    let (amount, unit) = lower.split_at(split);
    let amount: i64 = amount.parse().ok()?;
    let scale = match unit {
        "ms" => 1,
        "s" => 1_000,
        "m" => 60_000,
        "h" => 3_600_000,
        _ => return None,
    };
    amount.checked_mul(scale)
}

/// `Ok(None)` when `args` is not an auth command.
pub fn parse_auth_command(args: &[String]) -> Result<Option<AuthCommand>, String> {
    if args.first().map(String::as_str) != Some("auth") {
        return Ok(None);
    }
    let kind = match args.get(1).map(String::as_str) {
        Some("check") => AuthCommandKind::Check,
        Some("print-api-key") => AuthCommandKind::ApiKey,
        Some("print-bearer-token") => AuthCommandKind::BearerToken,
        other => {
            return Err(format!(
                "Unknown auth command \"{}\". Use \"pi auth print-api-key\", \"pi auth print-bearer-token\", or \"pi auth check\".",
                other.unwrap_or_default()
            ));
        }
    };
    let mut command =
        AuthCommand { kind, args: Vec::new(), json: false, credentials: false, no_refresh: false, min_expiry_ms: None };
    let mut index = 2;
    while index < args.len() {
        let arg = args[index].as_str();
        match arg {
            "--min-expiry" => {
                if kind != AuthCommandKind::BearerToken {
                    return Err("--min-expiry is only supported by print-bearer-token".into());
                }
                index += 1;
                let value = args.get(index).and_then(|value| parse_min_expiry(value));
                command.min_expiry_ms =
                    Some(value.ok_or_else(|| "--min-expiry must use a duration such as 30m or 1h".to_string())?);
            }
            "--json" | "--credentials" | "--no-refresh" => {
                if kind != AuthCommandKind::Check {
                    return Err(format!("{arg} is only supported by auth check"));
                }
                match arg {
                    "--json" => command.json = true,
                    "--credentials" => command.credentials = true,
                    _ => command.no_refresh = true,
                }
            }
            _ => command.args.push(arg.to_string()),
        }
        index += 1;
    }
    Ok(Some(command))
}

/// The API key, or the token of an `Authorization: Bearer` header.
pub fn auth_credential(auth: Option<&ResolvedAuth>) -> Option<String> {
    let auth = &auth?.auth;
    if let Some(key) = &auth.api_key {
        return Some(key.clone());
    }
    let (_, value) = auth.headers.iter().find(|(name, _)| name.eq_ignore_ascii_case("authorization"))?;
    let (scheme, token) = value.split_once(char::is_whitespace)?;
    let token = token.trim();
    (scheme.eq_ignore_ascii_case("bearer") && !token.is_empty()).then(|| token.to_string())
}

fn validate(args: &Args, kind: AuthCommandKind) -> Result<(Option<String>, Option<String>), String> {
    let trimmed =
        |value: &Option<String>| value.as_deref().map(str::trim).filter(|value| !value.is_empty()).map(String::from);
    let (provider, model) = (trimmed(&args.provider), trimmed(&args.model));
    if let Some((name, _)) = args.unknown_flags.first() {
        return Err(format!("Unknown option --{name} for \"{}\".", kind.name()));
    }
    if args.api_key.is_some() || !args.messages.is_empty() || !args.file_args.is_empty() {
        return Err("Auth commands only accept --provider and --model".into());
    }
    if provider.is_none() && model.is_none() {
        return Err(match kind {
            AuthCommandKind::Check => "Auth checks require --provider <provider> or --model <model>".into(),
            _ => "Credential printing requires --provider <provider> or --model <model>".into(),
        });
    }
    Ok((provider, model))
}

struct AuthContext {
    registry: ModelRegistry,
    models_error: bool,
    resolver: AuthResolver,
}

async fn auth_context(agent_dir: &Path, store: Arc<dyn CredentialStore>, oauth: OAuthLookup) -> AuthContext {
    let (mut registry, error) = ModelRegistry::load_lenient(&models_path(agent_dir));
    let resolver = AuthResolver::new(store, oauth);
    resolver.set_provider_configs(registry.auth_configs());
    registry.set_configured_providers(configured_providers(&resolver, &registry).await);
    AuthContext { registry, models_error: error.is_some(), resolver }
}

struct CheckResult {
    status: &'static str,
    provider: String,
    reason: Option<&'static str>,
    auth_type: Option<AuthType>,
}

impl CheckResult {
    fn not_ready(provider: String, reason: &'static str) -> Self {
        Self { status: "not_ready", provider, reason: Some(reason), auth_type: None }
    }

    fn invalid(provider: String) -> Self {
        Self { status: "invalid", provider, reason: Some("invalid_state"), auth_type: None }
    }
}

async fn check_provider(
    context: &AuthContext,
    cli_provider: Option<&str>,
    cli_model: Option<&str>,
    refresh: bool,
) -> Result<CheckResult, String> {
    let mut provider = cli_provider.map(String::from);
    if let Some(pattern) = cli_model {
        let resolved = resolve_cli_model(cli_provider, Some(pattern), None, false, &context.registry);
        match (resolved.model, resolved.error) {
            (Some(model), None) => provider = Some(model.provider),
            (_, error) => return Err(error.unwrap_or_else(|| format!("Unable to resolve model \"{pattern}\""))),
        }
    }
    let provider = provider.ok_or("Unable to resolve an auth provider")?;
    if context.models_error {
        return Ok(CheckResult::invalid(provider));
    }
    if !context.registry.has_provider(&provider) {
        return Ok(CheckResult::not_ready(provider, "provider_not_found"));
    }
    let Some(check) = context.resolver.check(&provider).await.map_err(|error| error.to_string())? else {
        return Ok(CheckResult::not_ready(provider, "credentials_not_configured"));
    };
    let overrides = AuthOverrides::default();
    if refresh && context.resolver.resolve(&provider, &overrides).await.map_err(|error| error.to_string())?.is_none() {
        return Ok(CheckResult::not_ready(provider, "credentials_not_configured"));
    }
    Ok(CheckResult { status: "ready", provider, reason: None, auth_type: Some(check.auth_type) })
}

async fn provider_credential(context: &AuthContext, provider: &str, refresh: bool) -> Result<Option<String>, String> {
    let stored = context.resolver.store().read(provider).await.map_err(|error| error.to_string())?;
    if !refresh && let Some(Credential::OAuth(oauth)) = stored {
        return Ok(Some(oauth.access));
    }
    let auth =
        context.resolver.resolve(provider, &AuthOverrides::default()).await.map_err(|error| error.to_string())?;
    Ok(auth_credential(auth.as_ref()))
}

async fn run_check(
    args: &Args,
    command: &AuthCommand,
    agent_dir: &Path,
    oauth: OAuthLookup,
) -> Result<AuthOutput, String> {
    let (provider, model) = validate(args, command.kind)?;
    let requested = provider.clone().or(model.clone()).unwrap_or_default();
    let path = auth_path(agent_dir);
    let store: Arc<dyn CredentialStore> = if command.no_refresh {
        Arc::new(FileCredentialStore::read_only(path))
    } else {
        Arc::new(FileCredentialStore::new(path))
    };
    let context = auth_context(agent_dir, store, oauth).await;
    let refresh = !command.no_refresh;
    let outcome = async {
        let mut result = check_provider(&context, provider.as_deref(), model.as_deref(), refresh).await?;
        let mut credential = None;
        if command.credentials && result.status == "ready" {
            credential = provider_credential(&context, &result.provider, refresh).await?;
            if credential.is_none() {
                result = CheckResult::not_ready(result.provider, "credential_not_available");
            }
        }
        Ok::<_, String>((result, credential))
    };
    let (result, credential) = outcome.await.unwrap_or_else(|_| (CheckResult::invalid(requested), None));
    let stdout = if command.json {
        let mut object = Map::new();
        object.insert("status".into(), json!(result.status));
        object.insert("provider".into(), json!(result.provider));
        if let Some(reason) = result.reason {
            object.insert("reason".into(), json!(reason));
        }
        if let Some(auth_type) = result.auth_type {
            object.insert("authType".into(), serde_json::to_value(auth_type).unwrap_or(Value::Null));
        }
        if let Some(credential) = &credential {
            object.insert("credentials".into(), json!(credential));
        }
        Value::Object(object).to_string()
    } else {
        credential.unwrap_or_else(|| result.status.to_string())
    };
    let code = match result.status {
        "ready" => 0,
        "not_ready" => 1,
        _ => 2,
    };
    Ok(AuthOutput { code, stdout: format!("{stdout}\n"), stderr: String::new() })
}

/// Errors from validation and lookups keep their text; credential resolution failures do not.
enum PrintError {
    Command(String),
    Resolve,
}

async fn run_print(
    args: &Args,
    command: &AuthCommand,
    agent_dir: &Path,
    oauth: OAuthLookup,
) -> Result<String, PrintError> {
    let kind = command.kind;
    let (cli_provider, cli_model) = validate(args, kind).map_err(PrintError::Command)?;
    let context = auth_context(agent_dir, Arc::new(FileCredentialStore::new(auth_path(agent_dir))), oauth).await;
    let stored = context.resolver.store().list().await.map_err(|_| PrintError::Resolve)?;
    let types: HashMap<String, AuthType> = stored.into_iter().map(|info| (info.provider_id, info.auth_type)).collect();
    let mut providers: Vec<(String, Option<Model>)> = Vec::new();
    if let Some(name) = &cli_provider {
        if !context.registry.has_provider(name) {
            return Err(PrintError::Command(format!(
                "Unknown provider \"{name}\". Use --list-models to see available providers."
            )));
        }
        let model = match &cli_model {
            Some(pattern) => {
                let resolved = resolve_cli_model(Some(name), Some(pattern), None, false, &context.registry);
                match (resolved.model, resolved.error) {
                    (Some(model), None) => Some(model),
                    (_, error) => {
                        let message = error.unwrap_or_else(|| "Unable to resolve the requested provider/model".into());
                        return Err(PrintError::Command(message));
                    }
                }
            }
            None => None,
        };
        providers.push((name.clone(), model));
    } else {
        let pattern = cli_model.as_deref().unwrap_or_default();
        let models = context.registry.all_chat_models();
        for provider in context.registry.providers().into_iter().filter(|provider| types.contains_key(provider)) {
            let request = ModelReferenceRequest {
                text: pattern,
                provider: Some(&provider),
                thinking_suffix: ThinkingSuffix::Strict,
                fuzzy: true,
            };
            if let ModelReferenceResult::Resolved { model, .. } = resolve_model_reference(&request, &models, &|_| true)
            {
                providers.push((provider, Some(model)));
            }
        }
        if providers.is_empty() {
            return Err(PrintError::Command(format!(
                "Model \"{pattern}\" not found. Use --list-models to see available models."
            )));
        }
    }
    let mut found: Vec<(String, String)> = Vec::new();
    for (provider, model) in &providers {
        let is_oauth = types.get(provider) == Some(&AuthType::OAuth);
        if (kind == AuthCommandKind::ApiKey && is_oauth) || (kind == AuthCommandKind::BearerToken && !is_oauth) {
            continue;
        }
        let min_validity = command.min_expiry_ms.unwrap_or(DEFAULT_BEARER_TOKEN_MIN_EXPIRY_MS);
        let overrides = AuthOverrides {
            min_oauth_validity_ms: (kind == AuthCommandKind::BearerToken).then_some(min_validity),
            ..AuthOverrides::default()
        };
        let auth = match model {
            Some(model) => context.resolver.resolve_for_model(provider, &model.id, &overrides).await,
            None => context.resolver.resolve(provider, &overrides).await,
        }
        .map_err(|_| PrintError::Resolve)?;
        if let Some(value) = auth_credential(auth.as_ref()) {
            found.push((provider.clone(), value));
        }
    }
    match found.len() {
        1 => Ok(found.remove(0).1),
        0 => {
            let provider = providers.first().map(|(provider, _)| provider.clone()).unwrap_or_default();
            let is_oauth = types.get(&provider) == Some(&AuthType::OAuth);
            Err(PrintError::Command(match (cli_provider.is_some(), kind) {
                (true, AuthCommandKind::ApiKey) if is_oauth => {
                    format!("Provider \"{provider}\" is configured with OAuth, not an API key")
                }
                (true, AuthCommandKind::BearerToken) if !is_oauth => {
                    format!("Provider \"{provider}\" is not configured with an OAuth bearer token")
                }
                (_, AuthCommandKind::BearerToken) => "No usable OAuth bearer token is configured".into(),
                _ => "No usable API key is configured".into(),
            }))
        }
        _ => {
            let names: Vec<&str> = found.iter().map(|(provider, _)| provider.as_str()).collect();
            Err(PrintError::Command(format!(
                "Multiple configured providers matched ({}). Specify --provider.",
                names.join(", ")
            )))
        }
    }
}

/// Run `pi auth ...`; `None` when `args` is not an auth command.
pub async fn run_auth_command(args: &[String], agent_dir: &Path, oauth: OAuthLookup) -> Option<AuthOutput> {
    if args.first().map(String::as_str) != Some("auth") {
        return None;
    }
    if is_auth_command_help(args) {
        return Some(AuthOutput { code: 0, stdout: format!("{AUTH_HELP}\n"), stderr: String::new() });
    }
    let command = match parse_auth_command(args) {
        Ok(command) => command?,
        Err(message) => return Some(AuthOutput::error(1, &message)),
    };
    let parsed = parse_args(&command.args);
    if let Some((name, _)) = parsed.unknown_flags.first() {
        let kind = command.kind;
        let stderr =
            format!("Unknown option --{name} for \"{}\".\nUse \"pi --help\" or \"{}\".\n", kind.name(), kind.usage());
        return Some(AuthOutput { code: 1, stdout: String::new(), stderr });
    }
    let failure_code = if command.kind == AuthCommandKind::Check { 2 } else { 1 };
    let diagnostics: Vec<String> = parsed.errors.iter().chain(&parsed.warnings).cloned().collect();
    if !diagnostics.is_empty() {
        return Some(AuthOutput::error(failure_code, &diagnostics.join("\n")));
    }
    Some(match command.kind {
        AuthCommandKind::Check => match run_check(&parsed, &command, agent_dir, oauth).await {
            Ok(output) => output,
            Err(message) => AuthOutput::error(failure_code, &message),
        },
        _ => match run_print(&parsed, &command, agent_dir, oauth).await {
            Ok(credential) => AuthOutput { code: 0, stdout: format!("{credential}\n"), stderr: String::new() },
            Err(PrintError::Command(message)) => AuthOutput::error(1, &message),
            Err(PrintError::Resolve) => AuthOutput::error(1, "Failed to resolve credential"),
        },
    })
}

#[cfg(test)]
#[path = "auth_cli_tests.rs"]
mod tests;
