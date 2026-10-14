//! Host keys: scanning the key a host presents, and trusting or forgetting keys in the application's known-hosts file.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};

use tokio::process::Command;

use super::{SshError, SshTarget, run_ssh, ssh_arguments};
use crate::connection::lock;
use crate::session::random_hex;

/// Known-hosts lines and fingerprints of the key a host presented.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScannedHostKey {
    pub lines: Vec<String>,
    pub fingerprints: Vec<String>,
}

fn io_error(error: std::io::Error) -> SshError {
    SshError::Other(error.to_string())
}

/// Connect once with `accept-new` against a temporary known-hosts file to capture the key the host presents, through
/// the same route as real connections. Showing a fingerprint is not authentication: compare it with one obtained out
/// of band before accepting it.
pub async fn scan_host_key(target: &SshTarget) -> Result<ScannedHostKey, SshError> {
    let directory = std::env::temp_dir().join(format!("pi-env-hostkey-{}", random_hex(6)));
    tokio::fs::create_dir_all(&directory).await.map_err(io_error)?;
    let result = scan_into(target, &directory).await;
    let _ = tokio::fs::remove_dir_all(&directory).await;
    result
}

async fn scan_into(target: &SshTarget, directory: &Path) -> Result<ScannedHostKey, SshError> {
    let scanned = directory.join("known_hosts");
    let scanned_text = scanned.to_string_lossy().into_owned();
    // Authentication may fail without the key; the key is recorded before authentication.
    let args = ssh_arguments(target, false, Some(&scanned_text))?;
    match run_ssh(target, "exit 0", None, Some(args), None).await {
        Ok(_) | Err(SshError::Ssh { .. } | SshError::HostKeyUnknown(_)) => {}
        Err(error) => return Err(error),
    }
    let text = tokio::fs::read_to_string(&scanned).await.unwrap_or_default();
    let lines: Vec<String> = text.lines().filter(|line| !line.trim().is_empty()).map(str::to_string).collect();
    if lines.is_empty() {
        return Err(SshError::Other(format!("No host key received from {}", target.host)));
    }
    let output = Command::new("ssh-keygen").arg("-lf").arg(&scanned).output().await.map_err(io_error)?;
    let fingerprints = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(str::to_string)
        .collect();
    Ok(ScannedHostKey { lines, fingerprints })
}

fn valid_key_type(kind: &str) -> bool {
    let tail = |prefix: &str, extra: &str| {
        kind.strip_prefix(prefix).is_some_and(|rest| {
            !rest.is_empty()
                && rest.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || extra.contains(c))
        })
    };
    tail("ssh-", "") || tail("ecdsa-sha2-", "") || tail("sk-", "@.")
}

fn valid_key(key: &str) -> bool {
    let body = key.trim_end_matches('=');
    key.len() - body.len() <= 2
        && !body.is_empty()
        && body.chars().all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '/')
}

/// A known-hosts line for `alias`: `alias type key [comment]`, no markers, patterns or hashed names.
fn parse_host_key_line(alias: &str, line: &str) -> Result<(String, String), SshError> {
    let mut fields = line.split_whitespace();
    let (host, kind, key) = (fields.next(), fields.next(), fields.next());
    match (host, kind, key) {
        (Some(host), Some(kind), Some(key)) if host == alias && valid_key_type(kind) && valid_key(key) => {
            Ok((kind.to_string(), key.to_string()))
        }
        _ => Err(SshError::Other(format!("Not a host key line for {alias}: {}", serde_json::json!(line)))),
    }
}

/// Known-hosts files being changed, so changes to one file happen one at a time.
static KNOWN_HOSTS_CHANGES: LazyLock<Mutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>> =
    LazyLock::new(Mutex::default);

/// Rewrite the known-hosts file one change at a time, atomically.
async fn change_known_hosts(
    file: &str,
    change: impl FnOnce(Vec<String>) -> Result<Vec<String>, SshError>,
) -> Result<(), SshError> {
    let path = PathBuf::from(file);
    let gate = lock(&KNOWN_HOSTS_CHANGES).entry(path.clone()).or_default().clone();
    let _turn = gate.lock().await;
    if let Some(parent) = path.parent().filter(|parent| !parent.as_os_str().is_empty()) {
        tokio::fs::create_dir_all(parent).await.map_err(io_error)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = tokio::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700)).await;
        }
    }
    let existing = tokio::fs::read_to_string(&path).await.unwrap_or_default();
    let lines = change(existing.lines().filter(|line| !line.is_empty()).map(str::to_string).collect())?;
    let text = if lines.is_empty() { String::new() } else { format!("{}\n", lines.join("\n")) };
    let temporary = PathBuf::from(format!("{file}.{}.tmp", random_hex(6)));
    let mut options = tokio::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let written = async {
        use tokio::io::AsyncWriteExt;
        let mut handle = options.open(&temporary).await?;
        handle.write_all(text.as_bytes()).await?;
        handle.flush().await?;
        drop(handle);
        tokio::fs::rename(&temporary, &path).await
    };
    if let Err(error) = written.await {
        let _ = tokio::fs::remove_file(&temporary).await;
        return Err(io_error(error));
    }
    Ok(())
}

/// Trust host-key lines from `scan_host_key` by adding them to the target's known-hosts file. Only plain lines for
/// the target's alias are accepted. A key that differs from a trusted key of the same type is refused with
/// `HostKeyChanged`; `forget_host_key` must remove the old keys first.
pub async fn accept_host_key(target: &SshTarget, lines: &[String]) -> Result<(), SshError> {
    let alias = &target.host_key_alias;
    let accepted = lines
        .iter()
        .map(|line| parse_host_key_line(alias, line).map(|(kind, key)| (line.trim().to_string(), kind, key)))
        .collect::<Result<Vec<_>, _>>()?;
    change_known_hosts(&target.known_hosts_file, |mut existing| {
        let mut trusted: HashMap<String, String> = HashMap::new();
        for line in &existing {
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.len() >= 3 && fields[0] == alias {
                trusted.insert(fields[1].to_string(), fields[2].to_string());
            }
        }
        for (line, kind, key) in accepted {
            match trusted.get(&kind) {
                Some(known) if *known == key => continue,
                Some(_) => {
                    let message = format!("The {kind} host key of {} changed; forget the old key first", target.host);
                    return Err(SshError::HostKeyChanged(message));
                }
                None => {}
            }
            trusted.insert(kind, key);
            existing.push(line);
        }
        Ok(existing)
    })
    .await
}

/// Stop trusting every key stored for the target's alias, e.g. after the owner confirmed a changed host key.
pub async fn forget_host_key(target: &SshTarget) -> Result<(), SshError> {
    let alias = target.host_key_alias.clone();
    change_known_hosts(&target.known_hosts_file, move |existing| {
        Ok(existing.into_iter().filter(|line| line.split_whitespace().next() != Some(alias.as_str())).collect())
    })
    .await
}
