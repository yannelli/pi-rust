//! Requests from the agent to pi: `session/request_permission`, `fs/*`, and `terminal/*`. Paths
//! must be inside the session directory.

use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::checkpoints::Checkpoints;
use super::connection::{Connection, Incoming};
use super::host::{PermissionRequest, host};
use super::terminal::Terminals;

/// What a request handler needs from the session.
#[derive(Clone)]
pub(crate) struct ClientContext {
    pub provider: String,
    pub agent_name: String,
    pub pi_session: String,
    pub cwd: PathBuf,
    pub terminals: Terminals,
    /// A one-shot session, such as a compaction summary: no writes, terminals, or permissions.
    pub read_only: bool,
    pub checkpoints: Arc<Mutex<Checkpoints>>,
}

const READ_ONLY: &str = "pi runs this request read-only";

fn normalize(path: &Path) -> PathBuf {
    let mut normal = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                normal.pop();
            }
            Component::CurDir => {}
            other => normal.push(other),
        }
    }
    normal
}

/// The longest existing ancestor of `path`, resolved, with the rest appended.
fn resolve(path: &Path) -> PathBuf {
    let mut existing = path.to_path_buf();
    let mut rest = Vec::new();
    while !existing.exists() {
        match (existing.file_name().map(ToOwned::to_owned), existing.parent()) {
            (Some(name), Some(parent)) => {
                rest.push(name);
                existing = parent.to_path_buf();
            }
            _ => return path.to_path_buf(),
        }
    }
    let mut resolved = existing.canonicalize().unwrap_or(existing);
    resolved.extend(rest.iter().rev());
    resolved
}

/// `path` as an agent gets it: without the `\\?\` prefix that `canonicalize` adds on Windows,
/// under which Windows reads `/` as part of a name.
pub(crate) fn agent_path(path: &Path) -> String {
    let text = path.to_string_lossy();
    if let Some(share) = text.strip_prefix(r"\\?\UNC\") {
        return format!(r"\\{share}");
    }
    match text.strip_prefix(r"\\?\") {
        Some(rest) if rest.as_bytes().get(1) == Some(&b':') => rest.to_string(),
        _ => text.into_owned(),
    }
}

/// `path` when it is absolute and inside `cwd`. On Windows, `/` separates names too.
pub(crate) fn inside(cwd: &Path, path: &str) -> Result<PathBuf, String> {
    let path = if cfg!(windows) { path.replace('/', "\\") } else { path.to_string() };
    let candidate = Path::new(&path);
    if !candidate.is_absolute() {
        return Err(format!("{path} is not an absolute path"));
    }
    let root = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    let resolved = resolve(&normalize(candidate));
    if resolved.starts_with(&root) { Ok(resolved) } else { Err(format!("{path} is outside {}", root.display())) }
}

fn read_text_file(context: &ClientContext, params: &Value) -> Result<Value, String> {
    let path = inside(&context.cwd, params["path"].as_str().unwrap_or_default())?;
    let text = std::fs::read_to_string(&path).map_err(|error| format!("Cannot read {}: {error}", path.display()))?;
    let start = params["line"].as_u64().map_or(0, |line| line.saturating_sub(1) as usize);
    let content = match (start, params["limit"].as_u64()) {
        (0, None) => text,
        (start, limit) => {
            let lines = text.split_inclusive('\n').skip(start);
            lines.take(limit.map_or(usize::MAX, |limit| limit as usize)).collect()
        }
    };
    Ok(json!({"content": content}))
}

fn write_text_file(context: &ClientContext, params: &Value) -> Result<Value, String> {
    let path = inside(&context.cwd, params["path"].as_str().unwrap_or_default())?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| format!("Cannot create {}: {error}", parent.display()))?;
    }
    let content = params["content"].as_str().unwrap_or_default();
    let mut checkpoints = context.checkpoints.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    checkpoints.before_write(&path);
    std::fs::write(&path, content).map_err(|error| format!("Cannot write {}: {error}", path.display()))?;
    checkpoints.after_write(&context.pi_session, &path);
    Ok(Value::Null)
}

fn permission_request(context: &ClientContext, params: &Value) -> PermissionRequest {
    let call = &params["toolCall"];
    let kind = call["kind"].as_str().map(str::to_string);
    let title = call["title"].as_str().map(str::to_string).or_else(|| kind.clone()).unwrap_or_else(|| "tool".into());
    let options = params["options"].as_array().into_iter().flatten().filter_map(|option| {
        let text = |key: &str| option[key].as_str().map(str::to_string);
        Some((text("optionId")?, text("name").unwrap_or_default(), text("kind").unwrap_or_default()))
    });
    PermissionRequest {
        provider: context.provider.clone(),
        agent_name: context.agent_name.clone(),
        pi_session: context.pi_session.clone(),
        title,
        kind,
        options: options.collect(),
        tool_call: call.clone(),
    }
}

async fn permission(context: &ClientContext, params: &Value, cancel: &CancellationToken) -> Value {
    let request = permission_request(context, params);
    let known: Vec<String> = request.options.iter().map(|(id, _, _)| id.clone()).collect();
    let choice = match host() {
        Some(host) => tokio::select! {
            () = cancel.cancelled() => None,
            choice = host.request_permission(request) => choice,
        },
        None => None,
    };
    match choice.filter(|id| known.contains(id)) {
        Some(option) if !cancel.is_cancelled() => json!({"outcome": {"outcome": "selected", "optionId": option}}),
        _ => json!({"outcome": {"outcome": "cancelled"}}),
    }
}

/// Answers one agent request; `cancel` ends an open permission dialog with `cancelled`.
pub(crate) fn handle(
    connection: &Arc<Connection>,
    context: &ClientContext,
    event: Incoming,
    cancel: CancellationToken,
) {
    let Some(id) = event.id else { return };
    let (connection, context) = (connection.clone(), context.clone());
    tokio::spawn(async move {
        let params = &event.params;
        let method = event.method.as_str();
        if context.read_only && (method == "fs/write_text_file" || method.starts_with("terminal/")) {
            return connection.respond_error(id, -32603, READ_ONLY);
        }
        let result = match method {
            "session/request_permission" if context.read_only => Ok(json!({"outcome": {"outcome": "cancelled"}})),
            "session/request_permission" => Ok(permission(&context, params, &cancel).await),
            "fs/read_text_file" => read_text_file(&context, params),
            "fs/write_text_file" => write_text_file(&context, params),
            "terminal/create" => context.terminals.create(params, &context.cwd),
            "terminal/output" => context.terminals.output(params),
            "terminal/wait_for_exit" => context.terminals.wait_for_exit(params).await,
            "terminal/kill" => context.terminals.kill(params),
            "terminal/release" => context.terminals.release(params),
            _ => return connection.respond_error(id, -32601, "Method not supported by pi"),
        };
        match result {
            Ok(result) => connection.respond(id, result),
            Err(message) => connection.respond_error(id, -32603, &message),
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agents_get_paths_without_the_verbatim_prefix() {
        assert_eq!(agent_path(Path::new(r"\\?\C:\Users\me\work")), r"C:\Users\me\work");
        assert_eq!(agent_path(Path::new(r"\\?\UNC\server\share\work")), r"\\server\share\work");
        assert_eq!(agent_path(Path::new(r"\\?\Volume{1}\work")), r"\\?\Volume{1}\work");
        assert_eq!(agent_path(Path::new("/home/me/work")), "/home/me/work");
    }

    #[test]
    fn agent_paths_with_forward_slashes_stay_inside() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir(root.join("sub")).unwrap();
        let expected = root.join("sub").join("b.txt");
        for base in [agent_path(&root), root.display().to_string()] {
            assert_eq!(inside(&root, &format!("{base}/sub/b.txt")), Ok(expected.clone()), "{base}");
        }
        assert!(inside(&root, &format!("{}/../elsewhere.txt", agent_path(&root))).is_err());
    }
}
