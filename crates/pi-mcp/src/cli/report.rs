//! `pi mcp list`, `login`, and `logout`: they connect to the configured servers like a session does.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::Serialize;
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;
use url::Url;

use super::{LineFn, McpCommandOptions, ReadRedirectFn};
use crate::config::{LoadedMcpConfig, McpServerEntry, get_mcp_tool_exposure};
use crate::error::OAuthError;
use crate::log::McpServerLog;
use crate::oauth::LoopbackCallbackFactory;
use crate::runtime::signin::APP_NAME;
use crate::runtime::{
    ConnectionOptions, McpOAuthCredentialStore, McpServerConnection, McpSignInPrompt, ServerState, SignInOptions,
    sign_in_mcp_server,
};

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ServerReport {
    name: String,
    scope: String,
    source: String,
    /// Project `mcp.json` that overrides `enabled`, `exposure`, or `toolExposure` of this global server.
    #[serde(rename = "override", skip_serializing_if = "Option::is_none")]
    override_path: Option<String>,
    enabled: bool,
    exposure: String,
    transport: String,
    state: String,
    tools: Vec<String>,
    /// Tools whose exposure differs from the server's.
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_exposure: Option<Map<String, Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    resources: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    resource_templates: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

fn credentials(options: &McpCommandOptions) -> McpOAuthCredentialStore {
    options.credentials.clone().unwrap_or_else(|| McpOAuthCredentialStore::new(&options.agent_dir))
}

fn connect(
    entry: &McpServerEntry,
    options: &McpCommandOptions,
    credentials: &McpOAuthCredentialStore,
) -> McpServerConnection {
    let mut connection = ConnectionOptions::new(entry.clone(), options.cwd.clone(), credentials.clone());
    if let Some(factory) = &options.create_transport {
        connection.create_transport = factory.clone();
    }
    connection.log = Some(Arc::new(McpServerLog::new(options.agent_dir.join("mcp.log"))));
    McpServerConnection::new(connection)
}

async fn report(
    entry: &McpServerEntry,
    options: &McpCommandOptions,
    credentials: &McpOAuthCredentialStore,
) -> ServerReport {
    let config = &entry.config;
    let mut report = ServerReport {
        name: entry.name.clone(),
        scope: entry.scope.map_or_else(|| "global".into(), |scope| scope.to_string()),
        source: entry.source.clone(),
        override_path: entry.override_path.clone(),
        enabled: config.is_enabled(),
        exposure: config.exposure().to_string(),
        transport: config.describe_transport(),
        state: "disabled".into(),
        tools: Vec::new(),
        tool_exposure: None,
        resources: None,
        resource_templates: None,
        error: None,
    };
    if !report.enabled {
        return report;
    }
    let connection = connect(entry, options, credentials);
    // The connection records the state and error.
    let _ = connection.get_client().await;
    report.state = connection.state().to_string();
    let tools = connection.tools();
    report.tools = tools.iter().map(|tool| tool.name.clone()).collect();
    let overrides: Map<String, Value> = tools
        .iter()
        .map(|tool| (tool.name.clone(), get_mcp_tool_exposure(config, &tool.name).to_string()))
        .filter(|(_, exposure)| *exposure != report.exposure)
        .map(|(name, exposure)| (name, json!(exposure)))
        .collect();
    report.tool_exposure = (!overrides.is_empty()).then_some(overrides);
    if connection.has_resources() {
        report.resources = Some(connection.resources().len());
        report.resource_templates = Some(connection.resource_templates().len());
    }
    if connection.state() != ServerState::Connected {
        report.error = connection.error();
    }
    connection.close().await;
    report
}

pub(super) async fn list(loaded: &LoadedMcpConfig, json: bool, note: Option<&str>, options: &McpCommandOptions) -> i32 {
    let log = &options.log;
    let credentials = credentials(options);
    let reports =
        futures::future::join_all(loaded.servers.iter().map(|entry| report(entry, options, &credentials))).await;
    let failed =
        !loaded.errors.is_empty() || reports.iter().any(|report| report.enabled && report.state != "connected");
    let code = i32::from(failed);
    if json {
        let mut output = json!({ "servers": reports, "errors": loaded.errors });
        if let Some(note) = note {
            output["note"] = json!(note);
        }
        log(&serde_json::to_string_pretty(&output).unwrap_or_default());
        return code;
    }
    if reports.is_empty() && loaded.errors.is_empty() {
        let path = options.agent_dir.join("mcp.json");
        log(&format!("No MCP servers configured. Add them to {} or .pi/mcp.json.", path.display()));
    }
    for report in &reports {
        let state = match report.state.as_str() {
            "connected" => {
                let count = report.tools.len();
                format!("connected, {count} tool{}", if count == 1 { "" } else { "s" })
            }
            "needs-auth" => "needs sign-in".into(),
            state => state.into(),
        };
        log(&format!("{}: {state} ({}, {})", report.name, report.exposure, report.scope));
        log(&format!("  {}", report.transport));
        if let Some(path) = &report.override_path {
            log(&format!("  project override: {path}"));
        }
        if report.state == "needs-auth" {
            log(&format!("  sign in with: {APP_NAME} mcp login {}", report.name));
        }
        if !report.tools.is_empty() {
            let tools: Vec<String> = report
                .tools
                .iter()
                .map(|tool| match report.tool_exposure.as_ref().and_then(|exposures| exposures.get(tool)?.as_str()) {
                    Some(exposure) => format!("{tool} [{exposure}]"),
                    None => tool.clone(),
                })
                .collect();
            log(&format!("  tools: {}", tools.join(", ")));
        }
        if let Some(resources) = report.resources {
            log(&format!("  resources: {resources}, URI templates: {}", report.resource_templates.unwrap_or(0)));
        }
        if let Some(error) = &report.error {
            log(&format!("  {}", error.split('\n').collect::<Vec<_>>().join("\n  ")));
        }
    }
    for error in &loaded.errors {
        log(&format!("config error: {error}"));
    }
    if let Some(note) = note {
        log(note);
    }
    code
}

struct TerminalPrompt {
    name: String,
    log: LineFn,
    open_url: Option<LineFn>,
    read_redirect_url: Option<ReadRedirectFn>,
}

#[async_trait]
impl McpSignInPrompt for TerminalPrompt {
    fn show_authorization_url(&self, url: &Url) {
        (self.log)(&format!("Sign in to MCP server \"{}\" in your browser:\n{url}", self.name));
        if let Some(open_url) = &self.open_url {
            open_url(url.as_str());
        }
    }

    /// The pasted redirect URL in a terminal; otherwise only the browser callback finishes the sign-in.
    async fn prompt_for_redirect_url(&self, cancel: CancellationToken) -> Option<String> {
        match &self.read_redirect_url {
            Some(read) => read(cancel).await,
            None => {
                cancel.cancelled().await;
                None
            }
        }
    }
}

pub(super) async fn login_or_logout(
    entry: &McpServerEntry,
    login: bool,
    timeout: f64,
    options: &McpCommandOptions,
) -> i32 {
    let (log, error, name) = (&options.log, &options.error, &entry.name);
    let credentials = credentials(options);
    let connection = connect(entry, options, &credentials);
    let Some(url) = connection.oauth_url() else {
        error(&format!(
            "MCP server \"{name}\" does not use OAuth. Only HTTP servers without an Authorization header do."
        ));
        return 1;
    };
    if !login {
        let removed = credentials.remove(name, &url);
        log(&if removed {
            format!("Signed out of MCP server \"{name}\".")
        } else {
            format!("No stored credentials for MCP server \"{name}\".")
        });
        return 0;
    }
    let code = sign_in(&connection, &url, timeout, options, &credentials).await;
    connection.close().await;
    code
}

async fn sign_in(
    connection: &McpServerConnection,
    url: &str,
    timeout: f64,
    options: &McpCommandOptions,
    credentials: &McpOAuthCredentialStore,
) -> i32 {
    let (log, error, name) = (&options.log, &options.error, connection.name());
    // Connecting first answers whether a sign-in is needed and records the server's challenge.
    if connection.get_client().await.is_ok() {
        log(&format!("Already signed in to MCP server \"{name}\" ({} tools).", connection.tools().len()));
        return 0;
    }
    if connection.state() != ServerState::NeedsAuth {
        let reason = connection.error().unwrap_or_else(|| "unknown error".into());
        error(&format!("MCP server \"{name}\" failed to connect: {reason}"));
        return 1;
    }
    let cancel = CancellationToken::new();
    let timer = cancel.clone();
    let duration = Duration::from_secs_f64(timeout);
    let deadline = tokio::spawn(async move {
        tokio::time::sleep(duration).await;
        timer.cancel();
    });
    let prompt = TerminalPrompt {
        name: name.into(),
        log: log.clone(),
        open_url: options.open_url.clone(),
        read_redirect_url: options.read_redirect_url.clone(),
    };
    let result = match (connection.oauth_settings(), Url::parse(url)) {
        (Ok(settings), Ok(server_url)) => {
            sign_in_mcp_server(SignInOptions {
                server_url,
                store: credentials.for_server(name, url),
                settings,
                challenge: connection.challenge(),
                prompt: Arc::new(prompt),
                cancel,
                callback_factory: options.callback_factory.clone().unwrap_or_else(|| Arc::new(LoopbackCallbackFactory)),
                client: pi_ai::http_client().clone(),
            })
            .await
        }
        (Err(message), _) => Err(OAuthError::Other(message)),
        (_, Err(parse)) => Err(OAuthError::other(parse.to_string())),
    };
    deadline.abort();
    match result {
        Ok(()) => {}
        Err(OAuthError::Cancelled) => {
            let seconds = timeout.round();
            error(&format!(
                "Sign-in to MCP server \"{name}\" was cancelled or not completed within {seconds} seconds."
            ));
            return 1;
        }
        Err(failure) => {
            error(&format!("Sign-in to MCP server \"{name}\" failed: {failure}"));
            return 1;
        }
    }
    connection.clear_challenge();
    if let Err(failure) = connection.reconnect().await {
        error(&format!("Signed in, but {failure}"));
        return 1;
    }
    log(&format!("Signed in to MCP server \"{name}\" ({} tools).", connection.tools().len()));
    0
}
