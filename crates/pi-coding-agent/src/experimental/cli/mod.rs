//! Port of upstream `cli/experimental`: the `server` and `client` commands of `pi experimental`, their
//! options (`command-options.ts`), and the command group (`cli.ts`).

pub mod command;

use command::{CommandOption, ParsedCommandInput, flag_option, parse_options, string_option, value_option};
use pi_protocol::is_server_id;

/// Upstream `AuthInput`: a Radius token, or a file holding one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AuthInput {
    Token(String),
    File(String),
}

/// Upstream `TransportAddress` of `--connect`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TransportAddress {
    Unix { path: String },
    Radius { server_id: String },
}

/// Options of `pi experimental server`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ServerCommand {
    pub server_id: Option<String>,
    pub session_dir: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub plugin_packages: Option<Vec<String>>,
    /// Explicit Radius credential. Stored Radius auth is used when omitted.
    pub auth: Option<AuthInput>,
}

/// Options of `pi experimental client`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ClientCommand {
    pub auth: Option<AuthInput>,
    pub connect: Option<TransportAddress>,
    pub session_id: Option<String>,
    pub continue_session: bool,
    pub resume: bool,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub plugin_packages: Option<Vec<String>>,
    pub prompt: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExperimentalCommand {
    Server(ServerCommand),
    Client(ClientCommand),
}

const AUTH_TOKEN: CommandOption = string_option("--auth-token", false);
const AUTH_TOKEN_FILE: CommandOption = string_option("--auth-token-file", false);
const CONNECT: CommandOption = value_option("--connect", check_transport_address);
const SERVER_ID: CommandOption = value_option("--server-id", check_server_id);
const SESSION_DIR: CommandOption = string_option("--session-dir", false);
const PROVIDER: CommandOption = string_option("--provider", false);
const MODEL: CommandOption = string_option("--model", false);
const PLUGIN_PACKAGE: CommandOption = string_option("-e", true);
const SESSION_ID: CommandOption = string_option("--session-id", false);
const CONTINUE: CommandOption = flag_option("--continue");
const CONTINUE_SHORT: CommandOption = flag_option("-c");
const RESUME: CommandOption = flag_option("--resume");
const RESUME_SHORT: CommandOption = flag_option("-r");

fn check_server_id(value: &str) -> Result<(), String> {
    match is_server_id(value) {
        true => Ok(()),
        false => Err(format!("Invalid --server-id \"{value}\"; expected a lowercase UUIDv4")),
    }
}

fn check_transport_address(value: &str) -> Result<(), String> {
    parse_transport_address(value).map(drop)
}

/// Upstream `parseTransportAddress`: `unix:///absolute/path.sock` or `radius://<server ID>`.
pub fn parse_transport_address(value: &str) -> Result<TransportAddress, String> {
    let invalid = || format!("Invalid --connect address \"{value}\"");
    let url = url::Url::parse(value).map_err(|_| invalid())?;
    let host = url.host_str().unwrap_or_default();
    if url.scheme() == "radius" {
        let path = url.path();
        if !url.username().is_empty()
            || url.password().is_some()
            || url.port().is_some()
            || (!path.is_empty() && path != "/")
            || url.query().is_some()
            || url.fragment().is_some()
            || value != format!("radius://{host}{path}")
        {
            return Err(invalid());
        }
        if !is_server_id(host) {
            return Err("Radius transport address requires a lowercase UUIDv4 server ID".into());
        }
        return Ok(TransportAddress::Radius { server_id: host.to_string() });
    }
    if url.scheme() != "unix" {
        return Err(format!("Unsupported --connect transport \"{}:\"", url.scheme()));
    }
    if !host.is_empty() || url.port().is_some() || !url.username().is_empty() || url.password().is_some() {
        return Err("Unix transport address must not include an authority".into());
    }
    if !value.starts_with("unix:///")
        || value.starts_with("unix:////")
        || value.contains('?')
        || value.contains('#')
        || url.as_str() != value
    {
        return Err(invalid());
    }
    let path = percent_decode(url.path()).ok_or_else(invalid)?;
    if path.contains('\0') {
        return Err(invalid());
    }
    if !path.starts_with('/') {
        return Err("Unix transport address requires an absolute path".into());
    }
    Ok(TransportAddress::Unix { path })
}

/// `decodeURIComponent`: `None` for a malformed escape or invalid UTF-8.
fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let (mut decoded, mut index) = (Vec::with_capacity(bytes.len()), 0);
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = text.get(index + 1..index + 3)?;
            decoded.push(u8::from_str_radix(hex, 16).ok()?);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).ok()
}

fn parse_auth(input: &ParsedCommandInput) -> Result<Option<AuthInput>, String> {
    match (input.value(&AUTH_TOKEN), input.value(&AUTH_TOKEN_FILE)) {
        (Some(_), Some(_)) => Err("--auth-token and --auth-token-file are mutually exclusive".into()),
        (Some(token), None) => Ok(Some(AuthInput::Token(token))),
        (None, Some(path)) => Ok(Some(AuthInput::File(path))),
        (None, None) => Ok(None),
    }
}

fn unsupported_options(command: &str, input: &ParsedCommandInput) -> Option<String> {
    let message = format!("The experimental {command} command does not support existing CLI options yet");
    (!input.remaining_args.is_empty()).then_some(message)
}

fn non_empty(values: Vec<String>) -> Option<Vec<String>> {
    (!values.is_empty()).then_some(values)
}

fn finish<T>(scan_errors: Vec<String>, errors: Vec<String>, command: T) -> Result<T, Vec<String>> {
    let errors: Vec<String> = scan_errors.into_iter().chain(errors).collect();
    if errors.is_empty() { Ok(command) } else { Err(errors) }
}

fn parse_server(argv: &[String]) -> Result<ServerCommand, Vec<String>> {
    let options = [SERVER_ID, SESSION_DIR, PROVIDER, MODEL, PLUGIN_PACKAGE, AUTH_TOKEN, AUTH_TOKEN_FILE];
    let (input, scan_errors) = parse_options(&options, argv);
    let mut errors = Vec::new();
    let auth = parse_auth(&input).unwrap_or_else(|error| {
        errors.push(error);
        None
    });
    let (provider, model) = (input.value(&PROVIDER), input.value(&MODEL));
    if provider.is_some() && model.is_none() {
        errors.push("--provider requires --model".into());
    }
    errors.extend(unsupported_options("server", &input));
    let command = ServerCommand {
        server_id: input.value(&SERVER_ID),
        session_dir: input.value(&SESSION_DIR),
        provider,
        model,
        plugin_packages: non_empty(input.values(&PLUGIN_PACKAGE)),
        auth,
    };
    finish(scan_errors, errors, command)
}

fn parse_client(argv: &[String]) -> Result<ClientCommand, Vec<String>> {
    let options = [
        CONNECT,
        SESSION_ID,
        CONTINUE,
        CONTINUE_SHORT,
        RESUME,
        RESUME_SHORT,
        PROVIDER,
        MODEL,
        PLUGIN_PACKAGE,
        AUTH_TOKEN,
        AUTH_TOKEN_FILE,
    ];
    let (input, scan_errors) = parse_options(&options, argv);
    let mut errors = Vec::new();
    let auth = parse_auth(&input).unwrap_or_else(|error| {
        errors.push(error);
        None
    });
    let session_id = input.value(&SESSION_ID);
    let continue_session = input.flag(&CONTINUE) || input.flag(&CONTINUE_SHORT);
    let resume = input.flag(&RESUME) || input.flag(&RESUME_SHORT);
    let (provider, model) = (input.value(&PROVIDER), input.value(&MODEL));
    let remaining = &input.remaining_args;
    let separated = remaining.first().is_some_and(|first| first == "--");
    let prompt_args = if separated { &remaining[1..] } else { &remaining[..] };
    let prompt = match prompt_args {
        [prompt] if (separated || !prompt.starts_with('-')) && !prompt.is_empty() => Some(prompt.clone()),
        _ => None,
    };
    if provider.is_some() && model.is_none() {
        errors.push("--provider requires --model".into());
    }
    if [session_id.is_some(), continue_session, resume].into_iter().filter(|selected| *selected).count() > 1 {
        errors.push("--session-id, --continue, and --resume are mutually exclusive".into());
    }
    if prompt.is_none() {
        errors.extend(unsupported_options("client", &input));
    }
    let command = ClientCommand {
        auth,
        connect: input.value(&CONNECT).and_then(|value| parse_transport_address(&value).ok()),
        session_id,
        continue_session,
        resume,
        provider,
        model,
        plugin_packages: non_empty(input.values(&PLUGIN_PACKAGE)),
        prompt,
    };
    finish(scan_errors, errors, command)
}

/// Upstream `cli.parse`: the arguments after `pi experimental`.
pub fn parse(argv: &[String]) -> Result<ExperimentalCommand, Vec<String>> {
    match argv.first().map(String::as_str) {
        Some("server") => parse_server(&argv[1..]).map(ExperimentalCommand::Server),
        Some("client") => parse_client(&argv[1..]).map(ExperimentalCommand::Client),
        _ => Err(vec!["Expected experimental command: server or client".into()]),
    }
}
