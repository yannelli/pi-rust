//! Agent login: the `authMethods` of `initialize`, `authenticate` for agent methods, and the
//! command line of terminal methods, which the app runs in a terminal.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use serde_json::{Value, json};

use super::host::AgentLaunch;
use super::session::{AUTH_REQUIRED, connect, rpc};

/// A login can wait for a browser sign-in.
const AUTHENTICATE_TIMEOUT: Duration = Duration::from_secs(600);
const CHECK_TIMEOUT: Duration = Duration::from_secs(120);

/// A program that the user runs in a terminal to log in.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TerminalLogin {
    pub command: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct AuthMethod {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    /// Set for `type: "terminal"` and `_meta["terminal-auth"]` methods.
    pub terminal: Option<TerminalLogin>,
}

fn strings(value: &Value) -> Vec<String> {
    value.as_array().into_iter().flatten().filter_map(Value::as_str).map(str::to_string).collect()
}

fn env(value: &Value) -> BTreeMap<String, String> {
    let object = value.as_object().into_iter().flatten();
    object.filter_map(|(key, value)| Some((key.clone(), value.as_str()?.to_string()))).collect()
}

/// The auth methods of an `initialize` reply.
pub fn auth_methods_of(agent: &AgentLaunch, initialize: &Value) -> Vec<AuthMethod> {
    let methods = initialize["authMethods"].as_array().into_iter().flatten();
    methods
        .filter_map(|method| {
            let id = method["id"].as_str()?.to_string();
            let legacy = method.pointer("/_meta/terminal-auth").filter(|meta| meta.is_object());
            let terminal = match (method["type"].as_str(), legacy) {
                (Some("terminal"), _) => Some(TerminalLogin {
                    command: agent.command.clone(),
                    args: agent.args.iter().cloned().chain(strings(&method["args"])).collect(),
                    env: agent.env.clone().into_iter().chain(env(&method["env"])).collect(),
                }),
                (_, Some(meta)) => Some(TerminalLogin {
                    command: meta["command"].as_str().map_or_else(|| agent.command.clone(), str::to_string),
                    args: strings(&meta["args"]),
                    env: agent.env.clone().into_iter().chain(env(&meta["env"])).collect(),
                }),
                _ => None,
            };
            Some(AuthMethod {
                name: method["name"].as_str().map_or_else(|| id.clone(), str::to_string),
                description: method["description"].as_str().map(str::to_string),
                id,
                terminal,
            })
        })
        .collect()
}

/// Starts the agent and returns its auth methods.
pub async fn auth_methods(agent: &AgentLaunch, cwd: &Path) -> Result<Vec<AuthMethod>, String> {
    let connected = connect(agent, cwd).await?;
    Ok(auth_methods_of(agent, &connected.initialize))
}

/// Sends `authenticate` with `method` (`None` skips it, after a terminal login), then checks that
/// a session starts.
pub async fn authenticate(agent: &AgentLaunch, cwd: &Path, method: Option<&str>) -> Result<(), String> {
    let connected = connect(agent, cwd).await?;
    if let Some(method) = method {
        let params = json!({"methodId": method});
        connected
            .connection
            .request("authenticate", params, AUTHENTICATE_TIMEOUT)
            .await
            .map_err(|e| rpc(&agent.name, e))?;
    }
    let params = json!({"cwd": super::client::agent_path(cwd), "mcpServers": []});
    match connected.connection.request("session/new", params, CHECK_TIMEOUT).await {
        Ok(_) => Ok(()),
        Err(error) if error.code == AUTH_REQUIRED => Err(format!("The {} login did not complete", agent.name)),
        Err(error) => Err(rpc(&agent.name, error)),
    }
}
