//! Extensions in the CLI runtime factory (upstream `main.ts` and `agent-session-services.ts`):
//! loading for each session, extension CLI flags, and the `project_trust` pass over the
//! extensions that load without project trust.

use std::path::Path;
use std::sync::Arc;

use serde_json::{Value, json};

use super::AppOptions;
use crate::agent_session::{
    AgentSessionServices, CreateServicesOptions, RunLimits, RuntimeDiagnostic, create_agent_session_services,
};
use crate::cli::FlagValue;
use crate::extensions::{ExtensionBindings, ExtensionRunner, LoadOptions, LoadedExtensions, load_extensions};
use crate::output_schema::OutputSchema;
use crate::resources::trust::ExtensionTrustDecision;

/// Load the extensions of `services` with the program's inline extensions.
pub(super) async fn load(options: &AppOptions, services: &AgentSessionServices) -> LoadedExtensions {
    let load_options = LoadOptions { inline: options.inline_extensions.clone(), ..LoadOptions::default() };
    let mut loaded = load_extensions(services, &load_options).await;
    if let Some(runner) = &loaded.runner {
        let listed = services.resources().resources().extensions.clone();
        let registry = runner.registry();
        for entry in listed {
            let path = entry.path.to_string_lossy().to_string();
            let Some(builtin) = path.strip_prefix(crate::resources::source_info::BUILTIN_PATH_PREFIX) else { continue };
            if let Some(replacement) = registry.builtin_replacement(builtin) {
                loaded.diagnostics.push(RuntimeDiagnostic::warning(replacement.warning(builtin)));
            }
        }
    }
    loaded
}

/// Apply `--name [value]` flags extensions registered; the rest are unknown options.
pub(super) async fn apply_flags(
    runner: Option<&Arc<ExtensionRunner>>,
    flags: &[(String, FlagValue)],
) -> Vec<RuntimeDiagnostic> {
    let mut diagnostics = Vec::new();
    let mut unknown = Vec::new();
    for (name, value) in flags {
        let registered = runner.and_then(|runner| runner.registry().flag(name).map(|flag| flag.kind.clone()));
        let Some(kind) = registered else {
            unknown.push((name.clone(), value.clone()));
            continue;
        };
        let value = match (kind.as_str(), value) {
            ("boolean", _) => json!(true),
            (_, FlagValue::String(text)) => json!(text),
            _ => {
                diagnostics.push(RuntimeDiagnostic::error(format!("Extension flag \"--{name}\" requires a value")));
                continue;
            }
        };
        if let Some(runner) = runner {
            let _ = runner.call_json("setFlag", vec![json!(name), value]).await;
        }
    }
    diagnostics.extend(crate::cli::unknown_flags_error(&unknown).map(RuntimeDiagnostic::error));
    diagnostics
}

/// Give a pi-rust flag that was set to the extension that registered a flag of that name, as
/// upstream pi would; returns whether one did, which turns the pi-rust option off.
async fn claim_flag(runner: &ExtensionRunner, name: &str, value: Option<String>) -> bool {
    let Some(value) = value else { return false };
    let Some(kind) = runner.registry().flag(name).map(|flag| flag.kind.clone()) else { return false };
    let value = if kind == "boolean" { json!(true) } else { json!(value) };
    let _ = runner.call_json("setFlag", vec![json!(name), value]).await;
    true
}

/// An extension that registers `--max-turns` or `--max-cost` gets the value as with upstream pi,
/// and the pi-rust run limit of that name stays off.
pub(super) async fn claim_run_limit_flags(runner: Option<&Arc<ExtensionRunner>>, limits: RunLimits) -> RunLimits {
    let Some(runner) = runner else { return limits };
    let mut kept = limits;
    if claim_flag(runner, "max-turns", limits.max_turns.map(|turns| turns.to_string())).await {
        kept.max_turns = None;
    }
    if claim_flag(runner, "max-cost", limits.max_cost.map(|cost| cost.to_string())).await {
        kept.max_cost = None;
    }
    kept
}

/// [`claim_flag`] for `--output-retries` and `--output-schema`; returns whether an extension
/// registered `--output-schema`.
pub(super) async fn claim_output_schema_flags(
    runner: Option<&Arc<ExtensionRunner>>,
    schema: Option<&OutputSchema>,
) -> bool {
    let (Some(runner), Some(schema)) = (runner, schema) else { return false };
    claim_flag(runner, "output-retries", schema.retries_flag.map(|retries| retries.to_string())).await;
    claim_flag(runner, "output-schema", Some(schema.path.clone())).await
}

/// `project_trust` handlers of the extensions that load without project trust: the first
/// `yes` or `no` decides. Handler failures become warnings.
pub(super) async fn trust_decision(
    options: &AppOptions,
    cwd: &Path,
    agent_dir: &Path,
) -> (Option<ExtensionTrustDecision>, Vec<RuntimeDiagnostic>) {
    let services = create_agent_session_services(CreateServicesOptions {
        cwd: cwd.to_path_buf(),
        agent_dir: agent_dir.to_path_buf(),
        settings: None,
        project_trusted: Some(false),
        model_runtime: None,
        credentials: options.credentials.clone(),
        oauth: options.oauth.clone(),
        credential_env: options.credential_env.clone(),
        resources: options.resources.clone(),
    })
    .await;
    let inline: Vec<_> = options.inline_extensions.iter().filter(|extension| !extension.builtin()).cloned().collect();
    let loaded = load_extensions(&services, &LoadOptions { inline, ..LoadOptions::default() }).await;
    let Some(runner) = loaded.runner else { return (None, Vec::new()) };
    if !runner.has_handlers("project_trust") {
        runner.shutdown();
        return (None, Vec::new());
    }
    let (ui, mode) = options.project_trust_ui.clone().unwrap_or_else(|| {
        let bindings = ExtensionBindings::default();
        (bindings.ui, options.extension_mode.unwrap_or(bindings.mode))
    });
    runner.set_bindings(ExtensionBindings { ui, mode, ..ExtensionBindings::default() });
    let result = runner.call_json("emitProjectTrust", vec![json!(cwd.display().to_string())]).await;
    runner.shutdown();
    let result = result.unwrap_or(Value::Null);
    let diagnostics = result["errors"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|error| {
            let path = error["extensionPath"].as_str().unwrap_or_default();
            let message = error["error"].as_str().unwrap_or_default();
            RuntimeDiagnostic::warning(format!("Extension \"{path}\" project_trust error: {message}"))
        })
        .collect();
    let decision = result["result"]["trusted"].as_str().and_then(|trusted| match trusted {
        "yes" => Some(true),
        "no" => Some(false),
        _ => None,
    });
    let remember = result["result"]["remember"].as_bool() == Some(true);
    (decision.map(|trusted| ExtensionTrustDecision { trusted, remember }), diagnostics)
}
