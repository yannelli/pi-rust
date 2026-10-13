//! Pluggable auth: an `external_account` whose subject token comes from a local executable.
//! Runs only when `GOOGLE_EXTERNAL_ACCOUNT_ALLOW_EXECUTABLES=1`.

use std::collections::BTreeMap;
use std::process::Stdio;
use std::time::Duration;

use serde_json::Value;

use super::external::option;
use crate::simple_options::provider_env_value;
use crate::types::now_ms;

const DEFAULT_TIMEOUT_MS: i64 = 30_000;
const MIN_TIMEOUT_MS: i64 = 5_000;
const MAX_TIMEOUT_MS: i64 = 120_000;
const MAX_VERSION: i64 = 1;
const SAML: &str = "urn:ietf:params:oauth:token-type:saml2";
const ID_TOKEN: &str = "urn:ietf:params:oauth:token-type:id_token";
const JWT: &str = "urn:ietf:params:oauth:token-type:jwt";

/// Values passed to the executable through `GOOGLE_EXTERNAL_ACCOUNT_*` variables.
pub(super) struct Context<'a> {
    pub audience: &'a str,
    pub subject_token_type: &'a str,
    pub impersonated_email: Option<String>,
}

/// A validated executable response.
struct Response {
    version: i64,
    success: bool,
    expiration_time: Option<i64>,
    subject_token: String,
    error: (String, String),
}

impl Response {
    fn parse(json: &Value) -> Result<Self, String> {
        let version = json["version"].as_i64().filter(|version| *version != 0);
        let version = version.ok_or("Executable response must contain a 'version' field.")?;
        let success = json["success"].as_bool().ok_or("Executable response must contain a 'success' field.")?;
        let text = |key: &str| json[key].as_str().filter(|text| !text.is_empty()).map(str::to_string);
        let mut response =
            Self { version, success, expiration_time: None, subject_token: String::new(), error: Default::default() };
        if success {
            response.expiration_time = json["expiration_time"].as_i64();
            let token_type = json["token_type"].as_str().unwrap_or_default();
            response.subject_token = match token_type {
                SAML => text("saml_response").ok_or_else(|| {
                    format!("Executable response must contain a 'saml_response' field when token_type={SAML}.")
                })?,
                ID_TOKEN | JWT => text("id_token").ok_or_else(|| {
                    format!("Executable response must contain a 'id_token' field when token_type={ID_TOKEN} or {JWT}.")
                })?,
                _ => {
                    return Err(format!(
                        "Executable response must contain a 'token_type' field when successful and it must be one of {ID_TOKEN}, {JWT}, or {SAML}."
                    ));
                }
            };
        } else {
            let code = text("code").ok_or("Executable response must contain a 'code' field when unsuccessful.")?;
            let message =
                text("message").ok_or("Executable response must contain a 'message' field when unsuccessful.")?;
            response.error = (code, message);
        }
        Ok(response)
    }

    fn is_expired(&self) -> bool {
        self.expiration_time.is_some_and(|expiration| expiration < (now_ms() as f64 / 1000.0).round() as i64)
    }
}

fn executable_error(message: &str, code: &str) -> String {
    format!("The executable failed with exit code: {code} and error message: {message}.")
}

/// Split a command line on whitespace, keeping double-quoted runs together.
pub(super) fn parse_command(command: &str) -> Result<Vec<String>, String> {
    let mut components = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    for character in command.chars() {
        match character {
            '"' => {
                quoted = !quoted;
                current.push(character);
            }
            _ if character.is_whitespace() && !quoted => {
                if !current.is_empty() {
                    components.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(character),
        }
    }
    if !current.is_empty() {
        components.push(current);
    }
    if components.is_empty() {
        return Err(format!("Provided command: \"{command}\" could not be parsed."));
    }
    for component in &mut components {
        if component.len() >= 2 && component.starts_with('"') && component.ends_with('"') {
            *component = component[1..component.len() - 1].to_string();
        }
    }
    Ok(components)
}

/// Run the configured executable, or reuse its unexpired `output_file`, for a subject token.
pub(super) async fn subject_token(
    source: &Value,
    context: &Context<'_>,
    env: &BTreeMap<String, String>,
) -> Result<String, String> {
    let executable = option(source, "executable").unwrap_or(&Value::Null);
    let command = executable["command"].as_str().filter(|command| !command.is_empty());
    let command = command.ok_or("No valid Pluggable Auth \"credential_source\" provided.")?;
    let timeout_ms = match &executable["timeout_millis"] {
        Value::Null => DEFAULT_TIMEOUT_MS,
        value => value.as_i64().unwrap_or_default(),
    };
    if !(MIN_TIMEOUT_MS..=MAX_TIMEOUT_MS).contains(&timeout_ms) {
        return Err(format!("Timeout must be between {MIN_TIMEOUT_MS} and {MAX_TIMEOUT_MS} milliseconds."));
    }
    let components = parse_command(command)?;
    let output_file = executable["output_file"].as_str().filter(|path| !path.is_empty());
    if provider_env_value("GOOGLE_EXTERNAL_ACCOUNT_ALLOW_EXECUTABLES", env).as_deref() != Some("1") {
        return Err("Pluggable Auth executables need to be explicitly allowed to run by setting the GOOGLE_EXTERNAL_ACCOUNT_ALLOW_EXECUTABLES environment Variable to 1.".into());
    }
    let cached = match output_file {
        Some(path) => cached_response(path).await?,
        None => None,
    };
    let response = match cached {
        Some(response) => response,
        None => run(&components, timeout_ms, output_file, context, env).await?,
    };
    if response.version > MAX_VERSION {
        return Err(format!(
            "Version of executable is not currently supported, maximum supported version is {MAX_VERSION}."
        ));
    }
    if !response.success {
        return Err(executable_error(&response.error.1, &response.error.0));
    }
    if output_file.is_some() && response.expiration_time.is_none() {
        return Err("The executable response must contain the `expiration_time` field for successful responses when an output_file has been specified in the configuration.".into());
    }
    if response.is_expired() {
        return Err("Executable response is expired.".into());
    }
    Ok(response.subject_token)
}

async fn cached_response(path: &str) -> Result<Option<Response>, String> {
    let Ok(resolved) = tokio::fs::canonicalize(path).await else { return Ok(None) };
    if !tokio::fs::symlink_metadata(&resolved).await.is_ok_and(|metadata| metadata.is_file()) {
        return Ok(None);
    }
    let text = tokio::fs::read_to_string(&resolved).await.map_err(|error| error.to_string())?;
    if text.is_empty() {
        return Ok(None);
    }
    let invalid = || format!("The output file contained an invalid response: {text}");
    let json: Value = serde_json::from_str(&text).map_err(|_| invalid())?;
    let response = Response::parse(&json)?;
    Ok((response.success && !response.is_expired()).then_some(response))
}

async fn run(
    components: &[String],
    timeout_ms: i64,
    output_file: Option<&str>,
    context: &Context<'_>,
    env: &BTreeMap<String, String>,
) -> Result<Response, String> {
    let mut command = tokio::process::Command::new(&components[0]);
    command.args(&components[1..]).envs(env);
    command.env("GOOGLE_EXTERNAL_ACCOUNT_AUDIENCE", context.audience);
    command.env("GOOGLE_EXTERNAL_ACCOUNT_TOKEN_TYPE", context.subject_token_type);
    command.env("GOOGLE_EXTERNAL_ACCOUNT_INTERACTIVE", "0");
    if let Some(path) = output_file {
        command.env("GOOGLE_EXTERNAL_ACCOUNT_OUTPUT_FILE", path);
    }
    if let Some(email) = &context.impersonated_email {
        command.env("GOOGLE_EXTERNAL_ACCOUNT_IMPERSONATED_EMAIL", email);
    }
    command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
    let child = command.spawn().map_err(|error| format!("Failed to run {}: {error}", components[0]))?;
    let timeout = Duration::from_millis(timeout_ms as u64);
    let output = tokio::time::timeout(timeout, child.wait_with_output())
        .await
        .map_err(|_| "The executable failed to finish within the timeout specified.".to_string())?
        .map_err(|error| error.to_string())?;
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    if !output.status.success() {
        let code = output.status.code().map_or_else(|| "null".to_string(), |code| code.to_string());
        return Err(executable_error(&text, &code));
    }
    let json: Value =
        serde_json::from_str(&text).map_err(|_| format!("The executable returned an invalid response: {text}"))?;
    Response::parse(&json)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_split_on_whitespace_outside_quotes() {
        assert_eq!(
            parse_command("/bin/run --flag \"a b\" x\"y z\"").unwrap(),
            ["/bin/run", "--flag", "a b", "x\"y z\""]
        );
        assert!(parse_command("   ").is_err());
        let failure =
            Response::parse(&serde_json::json!({"version": 1, "success": false, "code": "401", "message": "no"}));
        assert_eq!(failure.unwrap().error, ("401".to_string(), "no".to_string()));
        let missing = Response::parse(&serde_json::json!({"version": 1, "success": true, "token_type": JWT}));
        assert!(missing.err().unwrap().contains("'id_token'"));
    }
}
