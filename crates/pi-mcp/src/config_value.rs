//! Config values that reference environment variables (`$NAME`, `${NAME}`) or commands (`!cmd`).
//! In other values, `$$` is a literal `$` and `$!` a literal `!`.

use std::collections::HashMap;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Debug, PartialEq, Eq)]
enum Part {
    Literal(String),
    Env(String),
}

fn is_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && chars.all(|character| character.is_ascii_alphanumeric() || character == '_')
}

fn push_literal(parts: &mut Vec<Part>, text: &str) {
    if text.is_empty() {
        return;
    }
    if let Some(Part::Literal(previous)) = parts.last_mut() {
        previous.push_str(text);
    } else {
        parts.push(Part::Literal(text.to_string()));
    }
}

fn parse_template(config: &str) -> Vec<Part> {
    let mut parts = Vec::new();
    let mut rest = config;
    while let Some(dollar) = rest.find('$') {
        push_literal(&mut parts, &rest[..dollar]);
        let after = &rest[dollar + 1..];
        match after.chars().next() {
            Some(escaped @ ('$' | '!')) => {
                push_literal(&mut parts, &escaped.to_string());
                rest = &after[1..];
            }
            Some('{') => match after.find('}') {
                None => {
                    push_literal(&mut parts, "$");
                    rest = after;
                }
                Some(end) => {
                    let name = &after[1..end];
                    if is_name(name) {
                        parts.push(Part::Env(name.to_string()));
                    } else {
                        push_literal(&mut parts, &rest[dollar..dollar + 1 + end + 1]);
                    }
                    rest = &after[end + 1..];
                }
            },
            _ => {
                let length = after
                    .char_indices()
                    .take_while(|(index, character)| {
                        character.is_ascii_alphanumeric() && (*index > 0 || !character.is_ascii_digit())
                            || *character == '_'
                    })
                    .count();
                if length > 0 {
                    parts.push(Part::Env(after[..length].to_string()));
                    rest = &after[length..];
                } else {
                    push_literal(&mut parts, "$");
                    rest = after;
                }
            }
        }
    }
    push_literal(&mut parts, rest);
    parts
}

fn env_value(name: &str, env: Option<&HashMap<String, String>>) -> Option<String> {
    env.and_then(|env| env.get(name).filter(|value| !value.is_empty()).cloned())
        .or_else(|| std::env::var(name).ok().filter(|value| !value.is_empty()))
}

fn resolve_template(parts: &[Part], env: Option<&HashMap<String, String>>) -> Option<String> {
    let mut resolved = String::new();
    for part in parts {
        match part {
            Part::Literal(text) => resolved.push_str(text),
            Part::Env(name) => resolved.push_str(&env_value(name, env)?),
        }
    }
    Some(resolved)
}

/// Run `command` in the shell and return its trimmed output, or `None` when it fails.
fn run_command(command: &str) -> Option<String> {
    let mut process = if cfg!(windows) {
        let mut process = Command::new("cmd");
        process.args(["/d", "/s", "/c", command]);
        process
    } else {
        let mut process = Command::new("sh");
        process.args(["-c", command]);
        process
    };
    let mut child = process.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().ok()?;
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut output = Vec::new();
        let _ = std::io::Read::read_to_end(&mut stdout, &mut output);
        output
    });
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() < COMMAND_TIMEOUT => std::thread::sleep(Duration::from_millis(10)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    let output = reader.join().ok()?;
    let text = String::from_utf8_lossy(&output).trim().to_string();
    (status.success() && !text.is_empty()).then_some(text)
}

/// Resolve a value: `!command` runs the command, other values interpolate environment variables.
/// `None` when a variable is unset or the command fails.
pub fn resolve_config_value(config: &str, env: Option<&HashMap<String, String>>) -> Option<String> {
    match config.strip_prefix('!') {
        Some(command) => run_command(command),
        None => resolve_template(&parse_template(config), env),
    }
}

/// Environment variables a value references, in order.
pub fn config_value_env_names(config: &str) -> Vec<String> {
    if config.starts_with('!') {
        return Vec::new();
    }
    let mut names: Vec<String> = Vec::new();
    for part in parse_template(config) {
        if let Part::Env(name) = part
            && !names.contains(&name)
        {
            names.push(name);
        }
    }
    names
}

/// Resolve a value or explain what is missing, naming the value with `description`.
pub fn resolve_config_value_or_err(
    config: &str,
    description: &str,
    env: Option<&HashMap<String, String>>,
) -> Result<String, String> {
    if let Some(value) = resolve_config_value(config, env) {
        return Ok(value);
    }
    if let Some(command) = config.strip_prefix('!') {
        return Err(format!("Failed to resolve {description} from shell command: {command}"));
    }
    let missing: Vec<String> =
        config_value_env_names(config).into_iter().filter(|name| env_value(name, env).is_none()).collect();
    match missing.len() {
        0 => Err(format!("Failed to resolve {description}")),
        1 => Err(format!("Failed to resolve {description} from environment variable: {}", missing[0])),
        _ => Err(format!("Failed to resolve {description} from environment variables: {}", missing.join(", "))),
    }
}

/// Resolve every header value, failing on the first that cannot be resolved.
pub fn resolve_headers_or_err<'a>(
    headers: impl IntoIterator<Item = (&'a str, &'a str)>,
    description: &str,
) -> Result<Vec<(String, String)>, String> {
    headers
        .into_iter()
        .map(|(name, value)| {
            resolve_config_value_or_err(value, &format!("{description} header \"{name}\""), None)
                .map(|resolved| (name.to_string(), resolved))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs.iter().map(|(name, value)| (name.to_string(), value.to_string())).collect()
    }

    #[test]
    fn interpolates_variables_and_escapes() {
        let env = env(&[("TOKEN", "secret"), ("A", "1")]);
        assert_eq!(resolve_config_value("Bearer ${TOKEN}", Some(&env)).as_deref(), Some("Bearer secret"));
        assert_eq!(resolve_config_value("$A-$A", Some(&env)).as_deref(), Some("1-1"));
        assert_eq!(resolve_config_value("cost $$5 $!", Some(&env)).as_deref(), Some("cost $5 !"));
        assert_eq!(
            resolve_config_value("${not a name} $ ${open", Some(&env)).as_deref(),
            Some("${not a name} $ ${open")
        );
        assert_eq!(resolve_config_value("$PI_MCP_SURELY_UNSET", Some(&env)), None);
        assert_eq!(config_value_env_names("${A}:$B:${A}"), vec!["A".to_string(), "B".to_string()]);
    }

    #[cfg(unix)]
    #[test]
    fn runs_commands_and_explains_failures() {
        assert_eq!(resolve_config_value("!printf ' secret '", None).as_deref(), Some("secret"));
        assert_eq!(
            resolve_config_value_or_err("!exit 1", "oauth.clientSecret", None).unwrap_err(),
            "Failed to resolve oauth.clientSecret from shell command: exit 1"
        );
        assert_eq!(
            resolve_config_value_or_err("${PI_MCP_UNSET_A}${PI_MCP_UNSET_B}", "x", None).unwrap_err(),
            "Failed to resolve x from environment variables: PI_MCP_UNSET_A, PI_MCP_UNSET_B"
        );
    }
}
