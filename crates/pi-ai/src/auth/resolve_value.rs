//! Configuration values that may be shell commands, environment references, or literals
//! (upstream `coding-agent/src/core/resolve-config-value.ts`). Used for `auth.json` keys
//! and `models.json` keys and headers.

use std::collections::{BTreeMap, HashMap};
use std::process::{Command, Stdio};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use crate::auth::ProviderEnv;

const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);
static COMMAND_CACHE: LazyLock<Mutex<HashMap<String, Option<String>>>> = LazyLock::new(Default::default);

#[derive(Clone, Debug, PartialEq, Eq)]
enum Part {
    Literal(String),
    Env(String),
}

fn is_name_start(character: char) -> bool {
    character.is_ascii_alphabetic() || character == '_'
}

fn is_name_char(character: char) -> bool {
    character.is_ascii_alphanumeric() || character == '_'
}

fn push_literal(parts: &mut Vec<Part>, text: &str) {
    if text.is_empty() {
        return;
    }
    match parts.last_mut() {
        Some(Part::Literal(previous)) => previous.push_str(text),
        _ => parts.push(Part::Literal(text.to_string())),
    }
}

/// `$NAME`, `${NAME}`, `$$` (literal `$`), and `$!` (literal `!`).
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
                Some(end) => {
                    let name = &after[1..end];
                    let valid = name.chars().next().is_some_and(is_name_start) && name.chars().all(is_name_char);
                    if valid {
                        parts.push(Part::Env(name.to_string()));
                    } else {
                        push_literal(&mut parts, &rest[dollar..dollar + end + 2]);
                    }
                    rest = &after[end + 1..];
                }
                None => {
                    push_literal(&mut parts, "$");
                    rest = after;
                }
            },
            Some(first) if is_name_start(first) => {
                let length = after.find(|character: char| !is_name_char(character)).unwrap_or(after.len());
                parts.push(Part::Env(after[..length].to_string()));
                rest = &after[length..];
            }
            _ => {
                push_literal(&mut parts, "$");
                rest = after;
            }
        }
    }
    push_literal(&mut parts, rest);
    parts
}

fn env_value(name: &str, env: Option<&ProviderEnv>) -> Option<String> {
    let scoped = env.and_then(|env| env.get(name)).filter(|value| !value.is_empty()).cloned();
    scoped.or_else(|| std::env::var(name).ok().filter(|value| !value.is_empty()))
}

fn resolve_template(parts: &[Part], env: Option<&ProviderEnv>) -> Option<String> {
    let mut resolved = String::new();
    for part in parts {
        match part {
            Part::Literal(text) => resolved.push_str(text),
            Part::Env(name) => resolved.push_str(&env_value(name, env)?),
        }
    }
    Some(resolved)
}

/// Whether `config` runs a shell command (`!command`).
pub fn is_command_config_value(config: &str) -> bool {
    config.starts_with('!')
}

/// Environment variable names referenced by a template value, in order, without duplicates.
pub fn config_value_env_var_names(config: &str) -> Vec<String> {
    if is_command_config_value(config) {
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

/// The single variable a value consists of (`$NAME` or `${NAME}`), if it is exactly one.
pub fn config_value_env_var_name(config: &str) -> Option<String> {
    if is_command_config_value(config) {
        return None;
    }
    match parse_template(config).as_slice() {
        [Part::Env(name)] => Some(name.clone()),
        _ => None,
    }
}

/// Referenced variables that are unset.
pub fn missing_config_value_env_var_names(config: &str, env: Option<&ProviderEnv>) -> Vec<String> {
    config_value_env_var_names(config).into_iter().filter(|name| env_value(name, env).is_none()).collect()
}

/// Whether every referenced variable is set. Commands count as configured.
pub fn is_config_value_configured(config: &str, env: Option<&ProviderEnv>) -> bool {
    missing_config_value_env_var_names(config, env).is_empty()
}

fn shell_command(command: &str) -> Command {
    #[cfg(windows)]
    let mut process = {
        let mut process = Command::new("cmd");
        process.arg("/C").arg(command);
        process
    };
    #[cfg(not(windows))]
    let mut process = {
        let mut process = Command::new("sh");
        process.arg("-c").arg(command);
        process
    };
    process.envs(crate::env::child_process_env());
    process
}

/// Run `command` with a 10 second limit; trimmed stdout, or `None` on failure or empty output.
fn execute_command_uncached(command: &str) -> Option<String> {
    let mut child =
        shell_command(command).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().ok()?;
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut output = Vec::new();
        let _ = std::io::Read::read_to_end(&mut stdout, &mut output);
        output
    });
    let deadline = Instant::now() + COMMAND_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    let output = reader.join().unwrap_or_default();
    let text = String::from_utf8_lossy(&output).trim().to_string();
    (status?.success() && !text.is_empty()).then_some(text)
}

fn execute_command(config: &str) -> Option<String> {
    if let Some(cached) = COMMAND_CACHE.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).get(config) {
        return cached.clone();
    }
    let result = execute_command_uncached(&config[1..]);
    COMMAND_CACHE.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).insert(config.to_string(), result.clone());
    result
}

/// Resolve a value: `!command` runs once per process and caches its trimmed stdout (failures
/// too); `$NAME` and `${NAME}` interpolate, with `env` winning over the process environment;
/// `$$` and `$!` escape. Returns `None` when a command fails or a variable is unset.
pub fn resolve_config_value(config: &str, env: Option<&ProviderEnv>) -> Option<String> {
    if is_command_config_value(config) {
        return execute_command(config);
    }
    resolve_template(&parse_template(config), env)
}

/// Like [`resolve_config_value`], but runs commands on every call.
pub fn resolve_config_value_uncached(config: &str, env: Option<&ProviderEnv>) -> Option<String> {
    if is_command_config_value(config) {
        return execute_command_uncached(&config[1..]);
    }
    resolve_template(&parse_template(config), env)
}

/// Resolve a value or explain why it could not be resolved. Commands run uncached.
pub fn resolve_config_value_or_err(
    config: &str,
    description: &str,
    env: Option<&ProviderEnv>,
) -> Result<String, String> {
    if let Some(value) = resolve_config_value_uncached(config, env) {
        return Ok(value);
    }
    if let Some(command) = config.strip_prefix('!') {
        return Err(format!("Failed to resolve {description} from shell command: {command}"));
    }
    match missing_config_value_env_var_names(config, env).as_slice() {
        [name] => Err(format!("Failed to resolve {description} from environment variable: {name}")),
        [] => Err(format!("Failed to resolve {description}")),
        names => Err(format!("Failed to resolve {description} from environment variables: {}", names.join(", "))),
    }
}

/// Resolve header values; headers that resolve to nothing are dropped.
pub fn resolve_headers(headers: &BTreeMap<String, String>, env: Option<&ProviderEnv>) -> BTreeMap<String, String> {
    headers
        .iter()
        .filter_map(|(name, value)| {
            resolve_config_value(value, env).filter(|value| !value.is_empty()).map(|value| (name.clone(), value))
        })
        .collect()
}

/// Resolve header values, failing on the first header that cannot be resolved.
pub fn resolve_headers_or_err(
    headers: &BTreeMap<String, String>,
    description: &str,
    env: Option<&ProviderEnv>,
) -> Result<BTreeMap<String, String>, String> {
    headers
        .iter()
        .map(|(name, value)| {
            resolve_config_value_or_err(value, &format!("{description} header \"{name}\""), env)
                .map(|value| (name.clone(), value))
        })
        .collect()
}

/// Forget cached command results.
pub fn clear_config_value_cache() {
    COMMAND_CACHE.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clear();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_literals_templates_and_escapes() {
        let mut env = ProviderEnv::new();
        env.insert("PI_TEST_LEFT".into(), "left".into());
        env.insert("PI_TEST_RIGHT".into(), "right".into());
        let env = Some(&env);
        assert_eq!(resolve_config_value("literal-key", env).as_deref(), Some("literal-key"));
        assert_eq!(resolve_config_value("$PI_TEST_LEFT", env).as_deref(), Some("left"));
        assert_eq!(resolve_config_value("${PI_TEST_LEFT}_$PI_TEST_RIGHT", env).as_deref(), Some("left_right"));
        assert_eq!(resolve_config_value("$$PI_TEST_LEFT", env).as_deref(), Some("$PI_TEST_LEFT"));
        assert_eq!(resolve_config_value("$!literal-$PI_TEST_RIGHT", env).as_deref(), Some("!literal-right"));
        assert_eq!(resolve_config_value("cost $5 ${not valid}", env).as_deref(), Some("cost $5 ${not valid}"));
        assert_eq!(resolve_config_value("$PI_RUST_SURELY_UNSET_VARIABLE", None), None);
        assert_eq!(config_value_env_var_name("${PI_TEST_LEFT}").as_deref(), Some("PI_TEST_LEFT"));
        assert_eq!(config_value_env_var_names("$A-$B-$A"), ["A", "B"]);
    }

    #[cfg(unix)]
    #[test]
    fn commands_trim_output_cache_results_and_report_failures() {
        assert_eq!(resolve_config_value("!echo '  spaced-key  '", None).as_deref(), Some("spaced-key"));
        assert_eq!(resolve_config_value("!printf 'line1\\nline2'", None).as_deref(), Some("line1\nline2"));
        for failing in ["!exit 1", "!nonexistent-command-12345", "!printf ''"] {
            assert_eq!(resolve_config_value(failing, None), None, "{failing}");
        }
        let dir = std::env::temp_dir().join(format!("pi-config-value-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let counter = dir.join("counter");
        std::fs::write(&counter, "0").unwrap();
        let command = format!("!count=$(cat '{0}'); echo $((count + 1)) > '{0}'; echo value", counter.display());
        assert_eq!(resolve_config_value(&command, None).as_deref(), Some("value"));
        assert_eq!(resolve_config_value(&command, None).as_deref(), Some("value"));
        assert_eq!(std::fs::read_to_string(&counter).unwrap().trim(), "1");
        assert_eq!(resolve_config_value_uncached(&command, None).as_deref(), Some("value"));
        assert_eq!(std::fs::read_to_string(&counter).unwrap().trim(), "2");
        let error = resolve_config_value_or_err("$PI_RUST_UNSET_A-$PI_RUST_UNSET_B", "API key", None).unwrap_err();
        assert!(error.contains("environment variables: PI_RUST_UNSET_A, PI_RUST_UNSET_B"), "{error}");
        let _ = std::fs::remove_dir_all(dir);
    }
}
