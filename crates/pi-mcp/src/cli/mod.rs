//! `pi mcp`: add, remove, and check MCP servers and sign in to them outside a session. Agents run it
//! through bash to configure servers, check an `mcp.json` they wrote, and start an OAuth sign-in.

mod report;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures::future::BoxFuture;
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use crate::config::{
    CONFIG_DIR_NAME, McpConfigScope, add_mcp_server_config, load_mcp_config, remove_mcp_server_config,
    validate_mcp_server_config,
};
use crate::oauth::CallbackServerFactory;
use crate::runtime::signin::APP_NAME;
use crate::runtime::{McpOAuthCredentialStore, TransportFactory};

pub const HELP: &str = r#"Usage:
  pi mcp add <server> [options] -- <command> [args...]
  pi mcp add <server> [options] --url <url>
  pi mcp remove <server> [-l]
  pi mcp list [--json]
  pi mcp login <server> [--timeout <seconds>]
  pi mcp logout <server>

Configure and check MCP servers and sign in to OAuth servers without starting a session.
Reads ~/.pi/agent/mcp.json and, in trusted projects, .pi/mcp.json.

Commands:
  add <server>            Add or replace a server in mcp.json
  remove <server>         Remove a server from mcp.json
  list                    Show state, tools, and errors (exits 1 on failure)
  login <server>          Sign in through the browser
  logout <server>         Delete the stored OAuth credentials

Options for add and remove:
  -l, --local             Use .pi/mcp.json in the current project instead of the global file

Options for add:
  --url <url>             Streamable HTTP server URL (instead of a command)
  --env <KEY=VALUE>       Environment variable for a stdio server (repeatable)
  --cwd <dir>             Working directory for a stdio server
  --header <KEY=VALUE>    HTTP header (repeatable)
  --bearer-token-env-var <NAME>
                          Send "Authorization: Bearer ${NAME}"
  --oauth-client-id <id>  Pre-registered OAuth client id
  --oauth-client-secret <secret>
                          OAuth client secret (may be ${NAME} or !command)
  --oauth-callback-port <port>
                          Fixed OAuth callback port
  --oauth-client-name <name>
                          Client name sent when registering with the OAuth server
  --exposure <mode>       codemode (default), deferred, direct, or hidden
  --description <text>    What the server offers, shown in the system prompt

Other options:
  --json                  Print the list as JSON
  --timeout <seconds>     How long login waits for the browser (default: 300)"#;

const HELP_HINT: &str = "Use \"pi mcp --help\" for usage.";

pub type LineFn = Arc<dyn Fn(&str) + Send + Sync>;
/// Asks for the redirect URL from the browser address bar; resolves to `None` once cancelled.
pub type ReadRedirectFn = Arc<dyn Fn(CancellationToken) -> BoxFuture<'static, Option<String>> + Send + Sync>;

pub struct McpCommandOptions {
    pub cwd: PathBuf,
    pub agent_dir: PathBuf,
    /// Whether the user trusts the project, so its `.pi/mcp.json` is read.
    pub project_trusted: bool,
    /// Default: `mcp-auth.json` in the agent directory.
    pub credentials: Option<McpOAuthCredentialStore>,
    /// Opens the authorization URL in a browser. Without it the URL is only printed.
    pub open_url: Option<LineFn>,
    /// Reads a pasted redirect URL in a terminal. Without it only the browser callback finishes.
    pub read_redirect_url: Option<ReadRedirectFn>,
    pub callback_factory: Option<Arc<dyn CallbackServerFactory>>,
    pub create_transport: Option<TransportFactory>,
    /// Output lines. Default: stdout.
    pub log: LineFn,
    /// Error lines. Default: stderr.
    pub error: LineFn,
}

impl McpCommandOptions {
    pub fn new(cwd: impl Into<PathBuf>, agent_dir: impl Into<PathBuf>) -> Self {
        Self {
            cwd: cwd.into(),
            agent_dir: agent_dir.into(),
            project_trusted: false,
            credentials: None,
            open_url: None,
            read_redirect_url: None,
            callback_factory: None,
            create_transport: None,
            log: Arc::new(|line| println!("{line}")),
            error: Arc::new(|line| eprintln!("{line}")),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Flag,
    Value,
    List,
}

#[derive(Default)]
struct Parsed {
    positional: Vec<String>,
    values: HashMap<String, Option<String>>,
    lists: HashMap<String, Vec<String>>,
}

impl Parsed {
    fn value(&self, name: &str) -> Option<&str> {
        self.values.get(name).and_then(|value| value.as_deref())
    }

    fn has(&self, name: &str) -> bool {
        self.values.contains_key(name) || self.lists.contains_key(name)
    }
}

/// Parse `--name value` options. `--` ends them, as does reaching `max_positionals` positional
/// arguments, so a command's own options (`add <server> <command> --flag`) pass through.
fn parse_options(args: &[String], known: &[(&str, Kind)], error: &LineFn, max_positionals: usize) -> Option<Parsed> {
    let mut parsed = Parsed::default();
    let mut index = 0;
    while index < args.len() {
        let arg = if args[index] == "-l" { "--local" } else { args[index].as_str() };
        if arg == "--" || parsed.positional.len() >= max_positionals {
            let start = if arg == "--" { index + 1 } else { index };
            parsed.positional.extend(args[start..].iter().cloned());
            break;
        }
        let Some(name) = arg.strip_prefix("--") else {
            parsed.positional.push(arg.into());
            index += 1;
            continue;
        };
        let Some((_, kind)) = known.iter().find(|(known, _)| *known == name) else {
            error(&format!("Unknown option {arg}.\n{HELP_HINT}"));
            return None;
        };
        if *kind == Kind::Flag {
            parsed.values.insert(name.into(), None);
            index += 1;
            continue;
        }
        let Some(value) = args.get(index + 1) else {
            error(&format!("{arg} needs a value."));
            return None;
        };
        if *kind == Kind::List {
            parsed.lists.entry(name.into()).or_default().push(value.clone());
        } else {
            parsed.values.insert(name.into(), Some(value.clone()));
        }
        index += 2;
    }
    Some(parsed)
}

/// Run `pi mcp <args>` and return the exit code.
pub async fn run_mcp_command(args: &[String], options: &McpCommandOptions) -> i32 {
    let (log, error) = (&options.log, &options.error);
    let Some(command) = args.first().map(String::as_str) else {
        log(HELP);
        return 0;
    };
    if command == "help" || args.iter().any(|arg| arg == "--help" || arg == "-h") {
        log(HELP);
        return 0;
    }
    let rest = &args[1..];
    let project_config = options.cwd.join(CONFIG_DIR_NAME).join("mcp.json");
    match command {
        "add" => return add(rest, &project_config, options),
        "remove" => return remove(rest, &project_config, options),
        _ => {}
    }
    let loaded = load_mcp_config(&options.agent_dir, &options.cwd, options.project_trusted);
    let untrusted_note = (!options.project_trusted && project_config.exists()).then(|| {
        format!(
            "{} is ignored because the project is not trusted. Start {APP_NAME} in the project to trust it.",
            project_config.display()
        )
    });
    match command {
        "list" => {
            let Some(parsed) = parse_options(rest, &[("json", Kind::Flag)], error, usize::MAX) else { return 1 };
            if !parsed.positional.is_empty() {
                error(&format!("Usage: {APP_NAME} mcp list [--json]\n{HELP_HINT}"));
                return 1;
            }
            report::list(&loaded, parsed.has("json"), untrusted_note.as_deref(), options).await
        }
        "login" | "logout" => {
            let known: &[(&str, Kind)] = if command == "login" { &[("timeout", Kind::Value)] } else { &[] };
            let Some(parsed) = parse_options(rest, known, error, usize::MAX) else { return 1 };
            let [name] = parsed.positional.as_slice() else {
                error(&format!("Usage: {APP_NAME} mcp {command} <server>\n{HELP_HINT}"));
                return 1;
            };
            let Some(entry) = loaded.servers.iter().find(|server| server.name == *name) else {
                let names: Vec<&str> = loaded.servers.iter().map(|server| server.name.as_str()).collect();
                let configured = if names.is_empty() { "none".to_string() } else { names.join(", ") };
                let note = untrusted_note.map(|note| format!(" {note}")).unwrap_or_default();
                error(&format!("No MCP server named \"{name}\".{note} Configured: {configured}."));
                return 1;
            };
            let timeout = match parsed.value("timeout") {
                None => 300.0,
                Some(value) => value.trim().parse::<f64>().unwrap_or(f64::NAN),
            };
            if command == "login" && (!timeout.is_finite() || timeout <= 0.0) {
                error("--timeout must be a positive number of seconds.");
                return 1;
            }
            report::login_or_logout(entry, command == "login", timeout, options).await
        }
        _ => {
            error(&format!("Unknown mcp command \"{command}\".\n{HELP_HINT}"));
            1
        }
    }
}

/// `KEY=VALUE` pairs of a repeatable option.
fn parse_pairs(option: &str, pairs: Option<&Vec<String>>, error: &LineFn) -> Option<Map<String, Value>> {
    let mut record = Map::new();
    for pair in pairs.into_iter().flatten() {
        match pair.split_once('=') {
            Some((key, value)) if !key.is_empty() => {
                record.insert(key.into(), json!(value));
            }
            _ => {
                error(&format!("--{option} expects KEY=VALUE, got \"{pair}\"."));
                return None;
            }
        }
    }
    Some(record)
}

/// A number as JavaScript's `Number()` reads it, for `--oauth-callback-port`.
fn number_value(text: &str) -> Value {
    match text.trim().parse::<f64>() {
        Ok(number) if number.fract() == 0.0 && number.abs() < 1e15 => json!(number as i64),
        Ok(number) => json!(number),
        Err(_) => Value::Null,
    }
}

const ADD_OPTIONS: [(&str, Kind); 12] = [
    ("local", Kind::Flag),
    ("url", Kind::Value),
    ("env", Kind::List),
    ("cwd", Kind::Value),
    ("header", Kind::List),
    ("bearer-token-env-var", Kind::Value),
    ("oauth-client-id", Kind::Value),
    ("oauth-client-secret", Kind::Value),
    ("oauth-callback-port", Kind::Value),
    ("oauth-client-name", Kind::Value),
    ("exposure", Kind::Value),
    ("description", Kind::Value),
];

fn build_config(parsed: &Parsed, command: &[String], error: &LineFn) -> Option<Map<String, Value>> {
    let mut config = Map::new();
    if let Some(url) = parsed.value("url") {
        let mut headers = parse_pairs("header", parsed.lists.get("header"), error)?;
        if let Some(variable) = parsed.value("bearer-token-env-var") {
            headers.insert("Authorization".into(), json!(format!("Bearer ${{{variable}}}")));
        }
        let mut oauth = Map::new();
        for (option, key) in [("oauth-client-id", "clientId"), ("oauth-client-secret", "clientSecret")] {
            if let Some(value) = parsed.value(option) {
                oauth.insert(key.into(), json!(value));
            }
        }
        if let Some(port) = parsed.value("oauth-callback-port") {
            oauth.insert("callbackPort".into(), number_value(port));
        }
        if let Some(name) = parsed.value("oauth-client-name") {
            oauth.insert("clientName".into(), json!(name));
        }
        config.insert("url".into(), json!(url));
        if !headers.is_empty() {
            config.insert("headers".into(), Value::Object(headers));
        }
        if !oauth.is_empty() {
            config.insert("oauth".into(), Value::Object(oauth));
        }
    } else {
        let env = parse_pairs("env", parsed.lists.get("env"), error)?;
        config.insert("command".into(), json!(command[0]));
        if command.len() > 1 {
            config.insert("args".into(), json!(command[1..]));
        }
        if !env.is_empty() {
            config.insert("env".into(), Value::Object(env));
        }
        if let Some(cwd) = parsed.value("cwd") {
            config.insert("cwd".into(), json!(cwd));
        }
    }
    for option in ["exposure", "description"] {
        if let Some(value) = parsed.value(option) {
            config.insert(option.into(), json!(value));
        }
    }
    Some(config)
}

fn add(args: &[String], project_config: &Path, options: &McpCommandOptions) -> i32 {
    let (log, error) = (&options.log, &options.error);
    let usage =
        format!("Usage: {APP_NAME} mcp add <server> [options] (--url <url> | -- <command> [args...])\n{HELP_HINT}");
    let Some(parsed) = parse_options(args, &ADD_OPTIONS, error, 2) else { return 1 };
    let Some((name, command)) = parsed.positional.split_first() else {
        error(&usage);
        return 1;
    };
    let url = parsed.value("url");
    if url.is_some() == !command.is_empty() {
        error(&usage);
        return 1;
    }
    let http_only = [
        "header",
        "bearer-token-env-var",
        "oauth-client-id",
        "oauth-client-secret",
        "oauth-callback-port",
        "oauth-client-name",
    ];
    let (misplaced, applies) =
        if url.is_none() { (&http_only[..], "HTTP servers (--url)") } else { (&["env", "cwd"][..], "stdio servers") };
    if let Some(option) = misplaced.iter().find(|option| parsed.has(option)) {
        error(&format!("--{option} only applies to {applies}."));
        return 1;
    }
    let Some(config) = build_config(&parsed, command, error) else { return 1 };
    let validated = match validate_mcp_server_config(name, &Value::Object(config)) {
        Ok(validated) => validated,
        Err(message) => {
            error(&message);
            return 1;
        }
    };
    let project = parsed.has("local");
    let path = if project { project_config.to_path_buf() } else { options.agent_dir.join("mcp.json") };
    let scope = if project { McpConfigScope::Project } else { McpConfigScope::Global };
    let value = serde_json::to_value(&validated).unwrap_or(Value::Null);
    let replaced = match add_mcp_server_config(&path, name, &value) {
        Ok(replaced) => replaced,
        Err(message) => {
            error(&format!("Could not update {}: {message}", path.display()));
            return 1;
        }
    };
    let verb = if replaced { "Replaced" } else { "Added" };
    log(&format!("{verb} {scope} MCP server \"{name}\" in {}.", path.display()));
    if project && !options.project_trusted {
        let shown = path.display();
        log(&format!(
            "The project is not trusted, so {shown} is ignored until you start {APP_NAME} in the project and trust it."
        ));
    }
    // HTTP servers without an Authorization header may use OAuth.
    let authorization = validated.header_pairs().iter().any(|(header, _)| header.eq_ignore_ascii_case("authorization"));
    let sign_in = if validated.is_http() && !authorization {
        format!(". If it requires sign-in: {APP_NAME} mcp login {name}")
    } else {
        String::new()
    };
    log(&format!("Check it with: {APP_NAME} mcp list{sign_in}"));
    0
}

fn remove(args: &[String], project_config: &Path, options: &McpCommandOptions) -> i32 {
    let error = &options.error;
    let Some(parsed) = parse_options(args, &[("local", Kind::Flag)], error, usize::MAX) else { return 1 };
    let [name] = parsed.positional.as_slice() else {
        error(&format!("Usage: {APP_NAME} mcp remove <server> [-l]\n{HELP_HINT}"));
        return 1;
    };
    let project = parsed.has("local");
    let path = if project { project_config.to_path_buf() } else { options.agent_dir.join("mcp.json") };
    let scope = if project { McpConfigScope::Project } else { McpConfigScope::Global };
    match remove_mcp_server_config(&path, name) {
        Ok(true) => {
            (options.log)(&format!("Removed {scope} MCP server \"{name}\" from {}.", path.display()));
            return 0;
        }
        Ok(false) => {}
        Err(message) => {
            error(&format!("Could not update {}: {message}", path.display()));
            return 1;
        }
    }
    let all = load_mcp_config(&options.agent_dir, &options.cwd, true);
    let other = all.servers.iter().find(|server| server.name == *name && server.scope != Some(scope));
    let hint = other
        .map(|other| {
            let flag = if other.scope == Some(McpConfigScope::Project) { "use --local" } else { "omit --local" };
            format!(" It is defined in {}; {flag}.", other.source)
        })
        .unwrap_or_default();
    error(&format!("No {scope} MCP server named \"{name}\" in {}.{hint}", path.display()));
    1
}
