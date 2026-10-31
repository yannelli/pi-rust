//! One ACP session per pi session and agent: the process, its connection, and the selectors
//! (modes, models, and other `configOptions`) the agent offers.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use serde_json::{Value, json};

use super::checkpoints::Checkpoints;
use super::client::ClientContext;
use super::connection::{Connection, RpcError};
use super::host::{AgentCommand, AgentLaunch, Choice, Selector, SessionInfo, host};
use super::process::{self, Process};
use super::prompt::PromptCapabilities;
use super::terminal::Terminals;

const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(60);
const SESSION_TIMEOUT: Duration = Duration::from_secs(120);
pub(crate) const AUTH_REQUIRED: i64 = -32000;

pub(crate) fn rpc(name: &str, error: RpcError) -> String {
    match error.code {
        AUTH_REQUIRED => format!("{name} needs a login. Run /acp, select the agent, and choose Log in."),
        _ => format!("{name}: {error}"),
    }
}

/// A started agent after `initialize`.
pub(crate) struct Connected {
    pub connection: Arc<Connection>,
    pub process: Process,
    pub initialize: Value,
}

pub(crate) async fn connect(agent: &AgentLaunch, cwd: &Path) -> Result<Connected, String> {
    let process = process::spawn(agent, cwd)?;
    let connection = process.connection.clone();
    let params = json!({"protocolVersion": 1,
        "clientCapabilities": {"fs": {"readTextFile": true, "writeTextFile": true}, "terminal": true,
            "auth": {"terminal": true}},
        "clientInfo": {"name": "pi-rust", "title": "pi", "version": env!("CARGO_PKG_VERSION")}});
    let initialize = match connection.request("initialize", params, INITIALIZE_TIMEOUT).await {
        Ok(initialize) => initialize,
        Err(error) => {
            let stderr = process.stderr_tail();
            let detail = if stderr.is_empty() { String::new() } else { format!("\n{stderr}") };
            return Err(format!("{}{detail}", rpc(&agent.name, error)));
        }
    };
    if initialize["protocolVersion"] != 1 {
        return Err(format!("{} speaks ACP protocol {}; pi needs 1", agent.name, initialize["protocolVersion"]));
    }
    Ok(Connected { connection, process, initialize })
}

fn choices(options: &Value) -> Vec<Choice> {
    let flat = options.as_array().into_iter().flatten().flat_map(|option| match option["options"].as_array() {
        Some(group) => group.clone(),
        None => vec![option.clone()],
    });
    let text = |value: &Value, key: &str| value[key].as_str().map(str::to_string);
    flat.filter_map(|option| {
        let id = text(&option, "value")?;
        Some(Choice {
            name: text(&option, "name").unwrap_or_else(|| id.clone()),
            description: text(&option, "description"),
            id,
        })
    })
    .collect()
}

/// The selectors of a `session/new` or `session/load` reply, or of a `config_option_update`.
pub(crate) fn selectors(reply: &Value, config: Option<&[Value]>) -> Vec<Selector> {
    let text = |value: &Value, key: &str| value[key].as_str().map(str::to_string);
    let config =
        config.map(<[Value]>::to_vec).or_else(|| reply["configOptions"].as_array().cloned()).unwrap_or_default();
    let mut selectors: Vec<Selector> = config
        .iter()
        .filter(|option| option["type"] == "select")
        .map(|option| Selector {
            id: text(option, "id").unwrap_or_default(),
            name: text(option, "name").or_else(|| text(option, "id")).unwrap_or_default(),
            category: text(option, "category").unwrap_or_else(|| "_custom".into()),
            current: text(option, "currentValue"),
            choices: choices(&option["options"]),
            legacy_mode: false,
        })
        .collect();
    let (has_mode, has_model) = ["mode", "model"].map(|c| selectors.iter().any(|s| s.category == c)).into();
    if let Some(modes) = reply.get("modes").filter(|modes| modes.is_object()).filter(|_| !has_mode) {
        let available = modes["availableModes"].as_array().into_iter().flatten();
        let choices = available.filter_map(|mode| {
            let id = text(mode, "id")?;
            Some(Choice {
                name: text(mode, "name").unwrap_or_else(|| id.clone()),
                description: text(mode, "description"),
                id,
            })
        });
        let (current, choices) = (text(modes, "currentModeId"), choices.collect());
        selectors.push(Selector {
            id: "mode".into(),
            name: "Mode".into(),
            category: "mode".into(),
            current,
            choices,
            legacy_mode: true,
        });
    }
    if let Some(models) = reply.get("models").filter(|models| models.is_object()).filter(|_| !has_model) {
        let available = models["availableModels"].as_array().into_iter().flatten();
        let choices = available.filter_map(|model| {
            let id = text(model, "modelId")?;
            Some(Choice {
                name: text(model, "name").unwrap_or_else(|| id.clone()),
                description: text(model, "description"),
                id,
            })
        });
        let (current, choices) = (text(models, "currentModelId"), choices.collect());
        selectors.push(Selector {
            id: "model".into(),
            name: "Model".into(),
            category: "model".into(),
            current,
            choices,
            legacy_mode: true,
        });
    }
    selectors
}

pub(crate) struct Runtime {
    pub agent: AgentLaunch,
    pub connection: Arc<Connection>,
    pub process: Process,
    pub session_id: String,
    pub capabilities: PromptCapabilities,
    pub selectors: Vec<Selector>,
    pub client: ClientContext,
    /// Messages of the pi transcript the session has seen.
    pub seen: usize,
    /// The session cost in USD from the last `usage_update`.
    pub cost_usd: Option<f64>,
    pub commands: Vec<AgentCommand>,
    /// The `size` of the last `usage_update`.
    pub context_window: Option<u64>,
    /// The pi model ID of the last turn.
    pub pi_model: String,
    /// The timestamp of the last reply of this session, to find it in a compacted transcript.
    pub last_reply: Option<i64>,
    /// Shown once in the next reply, for example MCP servers the agent did not get.
    pub notices: Vec<String>,
    pub proxy: Option<super::proxy::Proxy>,
}

impl Runtime {
    /// Starts the agent and resumes the session of `pi_session` with `session/load` when the agent
    /// can, else starts a new session. Returns whether the session was resumed. A `read_only`
    /// session gets no MCP servers and does not resume.
    pub async fn start(
        agent: AgentLaunch,
        cwd: PathBuf,
        pi_session: &str,
        read_only: bool,
    ) -> Result<(Self, bool), String> {
        let Connected { connection, process, initialize } = connect(&agent, &cwd).await?;
        let capabilities = &initialize["agentCapabilities"];
        let (mut servers, mut notices) = match read_only {
            true => (Vec::new(), Vec::new()),
            false => super::mcp::servers(capabilities, &agent.name),
        };
        let mut proxy = None;
        if !read_only {
            match super::proxy::start(capabilities, &agent.name, pi_session).await {
                Ok(Some((server, started))) => {
                    servers.push(server);
                    proxy = Some(started);
                }
                Ok(None) => {}
                Err(notice) => notices.push(notice),
            }
        }
        let resume = host().filter(|_| !read_only).and_then(|host| host.recall_session(&agent.provider, pi_session));
        let can_load = capabilities["loadSession"] == json!(true);
        let mut reply = None;
        if let Some(id) = resume.filter(|_| can_load) {
            let params = json!({"sessionId": id, "cwd": cwd, "mcpServers": servers});
            if let Ok(mut loaded) = connection.request("session/load", params, SESSION_TIMEOUT).await {
                loaded["sessionId"] = json!(id);
                reply = Some(loaded);
            }
        }
        let resumed = reply.is_some();
        let reply = match reply {
            Some(reply) => reply,
            None => {
                let params = json!({"cwd": cwd, "mcpServers": servers});
                connection.request("session/new", params, SESSION_TIMEOUT).await.map_err(|e| rpc(&agent.name, e))?
            }
        };
        let session_id =
            reply["sessionId"].as_str().filter(|id| !id.is_empty()).ok_or("The agent returned no session id")?;
        let client = ClientContext {
            provider: agent.provider.clone(),
            agent_name: agent.name.clone(),
            pi_session: pi_session.to_string(),
            cwd,
            terminals: Terminals::default(),
            read_only,
            checkpoints: Arc::new(Mutex::new(Checkpoints::default())),
        };
        let runtime = Self {
            connection,
            process,
            session_id: session_id.to_string(),
            capabilities: PromptCapabilities {
                embedded_context: capabilities.pointer("/promptCapabilities/embeddedContext") == Some(&json!(true)),
                images: capabilities.pointer("/promptCapabilities/image") == Some(&json!(true)),
            },
            selectors: selectors(&reply, None),
            client,
            agent,
            seen: 0,
            cost_usd: None,
            commands: Vec::new(),
            context_window: None,
            pi_model: super::DEFAULT_MODEL.into(),
            last_reply: None,
            notices,
            proxy,
        };
        Ok((runtime, resumed))
    }

    pub fn info(&self) -> SessionInfo {
        SessionInfo {
            provider: self.agent.provider.clone(),
            pi_session: self.client.pi_session.clone(),
            acp_session: self.session_id.clone(),
            selectors: self.selectors.clone(),
            commands: self.commands.clone(),
            pi_model: self.pi_model.clone(),
            context_window: self.context_window,
            images: self.capabilities.images,
        }
    }

    /// Tells the host of the session state; a read-only session stays out of it.
    pub fn report(&self) {
        if self.client.read_only {
            return;
        }
        if let Some(host) = host() {
            host.session_changed(&self.info());
        }
    }

    /// Sets selector `id` (or the first selector of category `id`) to `value` when it differs.
    pub async fn set(&mut self, id: &str, value: &str) -> Result<(), String> {
        let Some(selector) =
            self.selectors.iter().find(|s| s.id == id).or_else(|| self.selectors.iter().find(|s| s.category == id))
        else {
            return Err(format!("{} has no setting {id}", self.agent.name));
        };
        if selector.current.as_deref() == Some(value) {
            return Ok(());
        }
        if !selector.choices.iter().any(|choice| choice.id == value) {
            let offered: Vec<&str> = selector.choices.iter().map(|choice| choice.id.as_str()).collect();
            return Err(format!(
                "{} does not offer {} {value}; it offers: {}",
                self.agent.name,
                selector.name,
                offered.join(", ")
            ));
        }
        let (method, params) = match (selector.legacy_mode, selector.category.as_str()) {
            (true, "mode") => ("session/set_mode", json!({"sessionId": self.session_id, "modeId": value})),
            (true, _) => ("session/set_model", json!({"sessionId": self.session_id, "modelId": value})),
            _ => (
                "session/set_config_option",
                json!({"sessionId": self.session_id, "configId": selector.id, "value": value}),
            ),
        };
        let selector_id = selector.id.clone();
        let reply =
            self.connection.request(method, params, SESSION_TIMEOUT).await.map_err(|e| rpc(&self.agent.name, e))?;
        match reply["configOptions"].as_array() {
            Some(config) => self.apply_config(config),
            None => self.set_current(&selector_id, value),
        }
        self.report();
        Ok(())
    }

    pub fn set_current(&mut self, id: &str, value: &str) {
        if let Some(selector) = self.selectors.iter_mut().find(|selector| selector.id == id) {
            selector.current = Some(value.to_string());
        }
    }

    /// Replaces the `configOptions` selectors and keeps the `modes` and `models` ones.
    pub fn apply_config(&mut self, config: &[Value]) {
        let mut next = selectors(&Value::Null, Some(config));
        let categories: Vec<String> = next.iter().map(|selector| selector.category.clone()).collect();
        next.extend(self.selectors.drain(..).filter(|s| s.legacy_mode && !categories.contains(&s.category)));
        self.selectors = next;
    }
}

pub(crate) type Shared = Arc<tokio::sync::Mutex<Option<Runtime>>>;
static RUNTIMES: LazyLock<Mutex<HashMap<(String, String), Shared>>> = LazyLock::new(Mutex::default);

/// The runtime slot of a pi session and agent provider.
pub(crate) fn slot(provider: &str, pi_session: &str) -> Shared {
    let mut runtimes = RUNTIMES.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    runtimes.entry((provider.to_string(), pi_session.to_string())).or_default().clone()
}
