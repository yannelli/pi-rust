//! Terminal Auth (upstream `auth.ts`): the client relaunches pi with `--terminal-login`, which
//! opens interactive pi so the user can log in or configure API keys.

use std::collections::HashMap;

use agent_client_protocol_schema::{AuthMethod, AuthMethodTerminal};
use serde_json::{Map, Value, json};

pub const PI_SETUP_METHOD_ID: &str = "pi_terminal_login";
pub const TERMINAL_LOGIN_FLAG: &str = "--terminal-login";

/// The command a client runs for the `terminal-auth` banner.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TerminalLaunch {
    pub command: String,
    pub args: Vec<String>,
}

/// This executable with `--terminal-login`, or `pi` from `PATH` when its path is unknown.
pub fn default_launch() -> TerminalLaunch {
    let command = std::env::current_exe()
        .ok()
        .map(|path| path.display().to_string())
        .filter(|path| !path.is_empty())
        .unwrap_or_else(|| "pi".into());
    TerminalLaunch { command, args: vec![TERMINAL_LOGIN_FLAG.into()] }
}

/// The advertised methods: the registry `type: terminal` shape, plus Zed's
/// `_meta["terminal-auth"]` launch spec when the client supports it.
pub fn auth_methods(supports_terminal_auth_meta: bool, launch: &TerminalLaunch) -> Vec<AuthMethod> {
    let mut method = AuthMethodTerminal::new(PI_SETUP_METHOD_ID, "Launch pi in the terminal")
        .description("Start pi in an interactive terminal to configure API keys or login")
        .args(vec![TERMINAL_LOGIN_FLAG.into()])
        .env(HashMap::new());
    if supports_terminal_auth_meta {
        let mut meta = Map::new();
        let spec = json!({"command": launch.command, "args": launch.args, "label": "Launch pi"});
        meta.insert("terminal-auth".into(), spec);
        method = method.meta(meta);
    }
    vec![AuthMethod::Terminal(method)]
}

/// [`auth_methods`] as JSON, for error data.
pub fn auth_methods_json(supports_terminal_auth_meta: bool, launch: &TerminalLaunch) -> Value {
    serde_json::to_value(auth_methods(supports_terminal_auth_meta, launch)).unwrap_or(Value::Null)
}

/// Zed sets `clientCapabilities._meta["terminal-auth"] = true` when it renders the banner.
pub fn client_supports_terminal_auth(client_meta: Option<&Map<String, Value>>) -> bool {
    client_meta.and_then(|meta| meta.get("terminal-auth")).and_then(Value::as_bool) == Some(true)
}
