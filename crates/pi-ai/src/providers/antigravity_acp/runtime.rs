//! One Antigravity ACP session per pi session: the server process, its connection, the tool bridge,
//! and what the session has seen. Sessions outlive a pi call so parked tool calls can finish.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex, RwLock};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use super::bridge::{BRIDGE_NAME, Bridge, BridgeTool, ParkedCall};
use super::connection::{Connection, RpcError};
use super::process::{self, NOT_INSTALLED, Server};
use super::prompt::PromptCapabilities;
use crate::auth::oauth::interact;
use crate::auth::{AuthEvent, AuthInteraction, AuthPrompt};

const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(30);
const SESSION_TIMEOUT: Duration = Duration::from_secs(120);
/// The server waits 300 seconds for the Google redirect.
const AUTHENTICATE_TIMEOUT: Duration = Duration::from_secs(330);
const SIGN_IN_INSTRUCTIONS: &str = "Open this link to sign in with Google.";
const PASTE_MESSAGE: &str =
    "If the browser cannot open the 127.0.0.1 page after sign-in (a remote machine), paste that page's address here:";
const THOUGHT_LEVEL: [&str; 3] = ["thought_level", "thinking_level", "reasoning_effort"];
const ROUTING: &str = "Use the pi_ tools from the pi-bridge MCP server for files, shell commands, and edits. They run in the user's pi session.";

/// A user MCP server that Antigravity connects to itself, with the prefix of pi's own copies of its
/// tools (`mcp__<server>__`), which the bridge then leaves out.
#[derive(Clone, Debug, PartialEq)]
pub struct PassthroughMcpServer {
    pub acp: Value,
    pub tool_prefix: String,
}

static PASSTHROUGH: LazyLock<RwLock<Vec<PassthroughMcpServer>>> = LazyLock::new(RwLock::default);

/// Set the user MCP servers that new Antigravity sessions receive in `session/new`.
pub fn set_passthrough_mcp_servers(servers: Vec<PassthroughMcpServer>) {
    *PASSTHROUGH.write().unwrap_or_else(|poisoned| poisoned.into_inner()) = servers;
}

pub(crate) fn passthrough() -> Vec<PassthroughMcpServer> {
    PASSTHROUGH.read().unwrap_or_else(|poisoned| poisoned.into_inner()).clone()
}

/// Starts the agent for tests in place of the installed server.
pub(crate) type AgentFactory = Arc<dyn Fn() -> Arc<Connection> + Send + Sync>;
static TEST_AGENT: LazyLock<RwLock<Option<AgentFactory>>> = LazyLock::new(RwLock::default);

#[cfg(test)]
pub(crate) fn set_test_agent(factory: Option<AgentFactory>) {
    *TEST_AGENT.write().unwrap_or_else(|poisoned| poisoned.into_inner()) = factory;
}

pub(crate) fn rpc(error: RpcError) -> String {
    format!("Antigravity ACP: {error}")
}

/// Starts the installed server (or the test agent) and runs `initialize`.
pub(crate) async fn connect(cwd: &std::path::Path) -> Result<(Arc<Connection>, Option<Server>, Value), String> {
    let factory = TEST_AGENT.read().unwrap_or_else(|poisoned| poisoned.into_inner()).clone();
    let (connection, server) = match factory {
        Some(factory) => (factory(), None),
        None => {
            let binary = process::locate(&|name| std::env::var(name).ok()).ok_or(NOT_INSTALLED)?;
            let server =
                process::spawn(&binary, cwd).map_err(|error| format!("Cannot start {}: {error}", binary.display()))?;
            (server.connection.clone(), Some(server))
        }
    };
    let params = json!({"protocolVersion": 1,
        "clientCapabilities": {"fs": {"readTextFile": false, "writeTextFile": false}, "terminal": false},
        "clientInfo": {"name": "pi-rust", "title": "pi", "version": env!("CARGO_PKG_VERSION")}});
    let initialize = connection.request("initialize", params, INITIALIZE_TIMEOUT).await.map_err(rpc)?;
    if initialize["protocolVersion"] != 1 {
        return Err(format!("Antigravity ACP speaks protocol {}; pi needs 1", initialize["protocolVersion"]));
    }
    Ok((connection, server, initialize))
}

pub(crate) struct Runtime {
    pub connection: Arc<Connection>,
    _server: Option<Server>,
    _bridge: Bridge,
    pub calls: mpsc::UnboundedReceiver<ParkedCall>,
    pub session_id: String,
    pub capabilities: PromptCapabilities,
    /// pi carries the system prompt in `_meta.antigravity.systemPrompt.append`.
    pub system_in_meta: bool,
    config: Vec<Value>,
    legacy_models: Vec<String>,
    /// What the session was built for; a change starts a new session.
    pub fingerprint: String,
    /// Messages of the pi transcript the session has seen.
    pub seen: usize,
    pub prompt: Option<oneshot::Receiver<Result<Value, RpcError>>>,
    pub parked: Vec<ParkedCall>,
    pub context_tokens: Option<u64>,
}

/// Selectors of `configOptions`: a `select` whose category or id is `kind`.
fn selector<'a>(config: &'a [Value], kinds: &[&str]) -> Option<&'a Value> {
    config.iter().find(|option| {
        option["type"] == "select"
            && kinds.iter().any(|kind| option["category"].as_str() == Some(kind) || option["id"].as_str() == Some(kind))
    })
}

/// The `(value, name)` of each option of `selector`, with grouped options flattened.
fn named_values(selector: &Value) -> Vec<(String, String)> {
    let options = selector["options"].as_array().into_iter().flatten();
    let items = options.flat_map(|option| match option["options"].as_array() {
        Some(group) => group.iter().collect::<Vec<_>>(),
        None => vec![option],
    });
    items
        .filter_map(|item| {
            let value = item["value"].as_str()?;
            Some((
                value.to_string(),
                item["name"].as_str().filter(|name| !name.is_empty()).unwrap_or(value).to_string(),
            ))
        })
        .collect()
}

fn values(selector: &Value) -> Vec<String> {
    named_values(selector).into_iter().map(|(value, _)| value).collect()
}

impl Runtime {
    pub async fn start(
        cwd: PathBuf,
        system: Option<&str>,
        tools: Vec<BridgeTool>,
        fingerprint: String,
    ) -> Result<Self, String> {
        let (connection, server, initialize) = connect(&cwd).await?;
        let capabilities = &initialize["agentCapabilities"];
        let (sender, calls) = mpsc::unbounded_channel();
        let bridge =
            Bridge::start(sender).await.map_err(|error| format!("Cannot start the pi tool bridge: {error}"))?;
        bridge.set_tools(tools);
        let mut servers = vec![json!({"type": "http", "name": BRIDGE_NAME, "url": bridge.url,
            "headers": [{"name": "Authorization", "value": format!("Bearer {}", bridge.token)}]})];
        servers.extend(passthrough().into_iter().map(|server| server.acp));
        let mut params = json!({"cwd": cwd, "mcpServers": servers});
        let system_in_meta = capabilities.pointer("/_meta/antigravity/systemPrompt/append") == Some(&json!(true));
        if system_in_meta {
            let append = match system.filter(|system| !system.trim().is_empty()) {
                Some(system) => format!("{system}\n\n{ROUTING}"),
                None => ROUTING.to_string(),
            };
            params["_meta"] = json!({"antigravity": {"systemPrompt": {"append": append},
                "eagerMcpServers": [BRIDGE_NAME], "enabledTools": []}});
        }
        let session =
            connection.request("session/new", params, SESSION_TIMEOUT).await.map_err(|error| match error.code {
                -32000 => "Antigravity needs a login. Run /login google-antigravity.".to_string(),
                _ => rpc(error),
            })?;
        let session_id = session["sessionId"]
            .as_str()
            .filter(|id| !id.is_empty())
            .ok_or("Antigravity ACP returned no session id")?;
        let legacy = session.pointer("/models/availableModels").and_then(Value::as_array).into_iter().flatten();
        Ok(Self {
            connection,
            _server: server,
            _bridge: bridge,
            calls,
            session_id: session_id.to_string(),
            capabilities: PromptCapabilities {
                embedded_context: capabilities.pointer("/promptCapabilities/embeddedContext") == Some(&json!(true)),
                images: capabilities.pointer("/promptCapabilities/image") == Some(&json!(true)),
            },
            system_in_meta,
            config: session["configOptions"].as_array().cloned().unwrap_or_default(),
            legacy_models: legacy.filter_map(|model| model["modelId"].as_str()).map(str::to_string).collect(),
            fingerprint,
            seen: 0,
            prompt: None,
            parked: Vec::new(),
            context_tokens: None,
        })
    }

    async fn set_option(&mut self, kinds: &[&str], value: &str) -> Result<(), String> {
        let Some(option) = selector(&self.config, kinds) else { return Ok(()) };
        if option["currentValue"].as_str() == Some(value) {
            return Ok(());
        }
        let available = values(option);
        if !available.iter().any(|known| known == value) {
            return Err(format!(
                "Antigravity does not offer {} {value}; it offers: {}",
                kinds[0],
                available.join(", ")
            ));
        }
        let params = json!({"sessionId": self.session_id, "configId": option["id"], "value": value});
        let reply = self.connection.request("session/set_config_option", params, SESSION_TIMEOUT).await.map_err(rpc)?;
        if let Some(config) = reply["configOptions"].as_array() {
            self.config = config.clone();
        }
        Ok(())
    }

    /// Selects `model` and, when the model offers one, the `effort` thinking level.
    pub async fn select(&mut self, model: &str, effort: Option<&str>) -> Result<(), String> {
        if selector(&self.config, &["model"]).is_some() {
            self.set_option(&["model"], model).await?;
            if let Some(effort) = effort {
                self.set_option(&THOUGHT_LEVEL, effort).await?;
            }
            return Ok(());
        }
        let routed = effort.map(|effort| format!("{model}-{effort}")).filter(|id| self.legacy_models.contains(id));
        let model_id = routed.unwrap_or_else(|| model.to_string());
        if !self.legacy_models.is_empty() && !self.legacy_models.contains(&model_id) {
            return Err(format!(
                "Antigravity does not offer model {model_id}; it offers: {}",
                self.legacy_models.join(", ")
            ));
        }
        let params = json!({"sessionId": self.session_id, "modelId": model_id});
        self.connection.request("session/set_model", params, SESSION_TIMEOUT).await.map(|_| ()).map_err(rpc)
    }

    /// Absorbs a `config_option_update`.
    pub fn update_config(&mut self, config: &[Value]) {
        self.config = config.to_vec();
    }
}

pub(crate) type Shared = Arc<tokio::sync::Mutex<Option<Runtime>>>;
static RUNTIMES: LazyLock<Mutex<HashMap<String, Shared>>> = LazyLock::new(Mutex::default);

/// The runtime slot of a pi session.
pub(crate) fn slot(session: &str) -> Shared {
    let mut runtimes = RUNTIMES.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    runtimes.entry(session.to_string()).or_default().clone()
}

/// Whether an `authMethods` entry is the Google account login.
fn google_method(method: &Value) -> bool {
    let text = format!("{} {}", method["id"].as_str().unwrap_or_default(), method["name"].as_str().unwrap_or_default())
        .to_lowercase();
    ["oauth-personal", "google account", "log in with google", "login with google"]
        .iter()
        .any(|needle| text.contains(needle))
}

/// Runs the server's own Google login, which keeps the credential, then checks that a session
/// starts. `GEMINI_API_KEY` in the environment replaces the login.
pub(crate) async fn login(interaction: &dyn AuthInteraction, cancel: &CancellationToken) -> Result<(), String> {
    let cwd = std::env::current_dir().map_err(|error| format!("Cannot read the working directory: {error}"))?;
    let (connection, mut server, initialize) = connect(&cwd).await?;
    if std::env::var("GEMINI_API_KEY").map_or(true, |key| key.is_empty()) {
        let methods = initialize["authMethods"].as_array().cloned().unwrap_or_default();
        let method = methods
            .iter()
            .find(|method| google_method(method))
            .ok_or("Antigravity ACP does not offer a Google login")?;
        let request = connection.request("authenticate", json!({"methodId": method["id"]}), AUTHENTICATE_TIMEOUT);
        let sign_in = server.as_mut().map(|server| &mut server.sign_in);
        authenticate(interaction, request, sign_in, cancel).await?;
    }
    let params = json!({"cwd": cwd, "mcpServers": []});
    connection.request("session/new", params, SESSION_TIMEOUT).await.map(|_| ()).map_err(|error| match error.code {
        -32000 => "The Antigravity login did not complete".to_string(),
        _ => rpc(error),
    })
}

/// Waits for `authenticate` and shows the sign-in URL that the server prints. The user can paste the
/// redirect address instead, for a browser on another machine.
async fn authenticate(
    interaction: &dyn AuthInteraction,
    request: impl Future<Output = Result<Value, RpcError>>,
    sign_in: Option<&mut mpsc::UnboundedReceiver<String>>,
    cancel: &CancellationToken,
) -> Result<(), String> {
    let done = |result: Result<Value, RpcError>| result.map(drop).map_err(rpc);
    let mut request = std::pin::pin!(request);
    let url = match sign_in {
        Some(sign_in) => tokio::select! {
            () = cancel.cancelled() => return Err("Login cancelled".into()),
            result = &mut request => return done(result),
            url = sign_in.recv() => url,
        },
        None => None,
    };
    if let Some(url) = url {
        interaction.notify(AuthEvent::AuthUrl { url: url.clone(), instructions: Some(SIGN_IN_INSTRUCTIONS.into()) });
        let prompt = AuthPrompt::ManualCode {
            message: PASTE_MESSAGE.into(),
            placeholder: Some("http://127.0.0.1:<port>/?state=...&code=...".into()),
        };
        let pasted = tokio::select! {
            () = cancel.cancelled() => return Err("Login cancelled".into()),
            result = &mut request => return done(result),
            input = interact::prompt(interaction, prompt, cancel) => input.map_err(|error| error.to_string())?,
        };
        deliver_redirect(&url, &pasted).await?;
    }
    tokio::select! {
        () = cancel.cancelled() => Err("Login cancelled".into()),
        result = request => done(result),
    }
}

/// The server's loopback callback (the `redirect_uri` of `sign_in`) with the query of `pasted`.
pub(super) fn redirect_target(sign_in: &str, pasted: &str) -> Result<reqwest::Url, String> {
    let mut target = reqwest::Url::parse(sign_in)
        .ok()
        .and_then(|url| url.query_pairs().find(|(key, _)| key == "redirect_uri").map(|(_, value)| value.into_owned()))
        .and_then(|redirect| reqwest::Url::parse(&redirect).ok())
        .ok_or("The Google sign-in URL has no redirect address")?;
    let pasted = pasted.trim();
    let query = pasted.split_once('?').map_or(pasted, |(_, query)| query);
    let query = query.split('#').next().unwrap_or_default();
    if !query.split('&').any(|pair| pair.starts_with("code=") || pair.starts_with("error=")) {
        return Err("Paste the full address of the 127.0.0.1 page, with its code parameter".into());
    }
    target.set_query(Some(query));
    Ok(target)
}

/// Sends the pasted redirect to the server's loopback callback on this machine.
async fn deliver_redirect(sign_in: &str, pasted: &str) -> Result<(), String> {
    let target = redirect_target(sign_in, pasted)?;
    let client = reqwest::Client::builder().no_proxy().build().map_err(|error| error.to_string())?;
    client
        .get(target)
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .map_err(|error| format!("Cannot reach the Antigravity login callback: {error}"))?;
    Ok(())
}

/// A model of the server's `model` selector, with the `thought_level` values it offers.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct OfferedModel {
    pub id: String,
    pub name: String,
    pub levels: Vec<String>,
}

/// The models of the login: the `model` selector of a new session and, for each model, its
/// `thought_level` values. A server without config options gives `models.availableModels`.
pub(crate) async fn offered_models() -> Result<Vec<OfferedModel>, String> {
    let cwd = std::env::temp_dir();
    let (connection, _server, _) = connect(&cwd).await?;
    let params = json!({"cwd": cwd, "mcpServers": []});
    let session = connection.request("session/new", params, SESSION_TIMEOUT).await.map_err(rpc)?;
    let session_id = session["sessionId"].as_str().ok_or("Antigravity ACP returned no session id")?;
    let mut config = session["configOptions"].as_array().cloned().unwrap_or_default();
    let Some(models) = selector(&config, &["model"]).cloned() else {
        let legacy = session.pointer("/models/availableModels").and_then(Value::as_array).into_iter().flatten();
        let offered = legacy.filter_map(|model| {
            let id = model["modelId"].as_str()?.to_string();
            let name = model["name"].as_str().map_or_else(|| id.clone(), str::to_string);
            Some(OfferedModel { id, name, levels: Vec::new() })
        });
        return Ok(offered.collect());
    };
    let mut offered = Vec::new();
    for (id, name) in named_values(&models) {
        let current = selector(&config, &["model"]).and_then(|option| option["currentValue"].as_str());
        if current != Some(id.as_str()) {
            let params = json!({"sessionId": session_id, "configId": models["id"], "value": id});
            let reply = connection.request("session/set_config_option", params, SESSION_TIMEOUT).await.map_err(rpc)?;
            if let Some(updated) = reply["configOptions"].as_array() {
                config = updated.clone();
            }
        }
        let levels = selector(&config, &THOUGHT_LEVEL).map(values).unwrap_or_default();
        offered.push(OfferedModel { id, name, levels });
    }
    Ok(offered)
}
