//! Writing the embedded bootstrap scripts and starting the `node` child.

use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;

use serde_json::json;

use super::bridge::{Bridge, forward_stderr};
use super::client::NodeClient;
use crate::error::JsError;
use crate::runtime::state::Shared;
use crate::runtime::{JsHandle, JsRuntimeOptions, OutputStream};

const FILES: &[(&str, &str)] = &[
    ("main.cjs", include_str!("js/main.cjs")),
    ("worker.cjs", include_str!("js/worker.cjs")),
    ("wire.cjs", include_str!("js/wire.cjs")),
    ("worker_globals.cjs", include_str!("js/worker_globals.cjs")),
    ("worker_modules.cjs", include_str!("js/worker_modules.cjs")),
    ("hooks.mjs", include_str!("js/hooks.mjs")),
];

const START_TIMEOUT: Duration = Duration::from_secs(60);

fn io_error(error: std::io::Error, what: &str) -> JsError {
    JsError::new(format!("cannot {what} for the Node backend: {error}")).with_code("ERR_PI_NODE_START")
}

/// Write the scripts under a directory named by their hash; each file is written to a temporary
/// name and renamed, so processes starting at once do not see partial files.
fn write_bootstrap(cache_dir: Option<&Path>) -> Result<PathBuf, JsError> {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    env!("CARGO_PKG_VERSION").hash(&mut hasher);
    FILES.hash(&mut hasher);
    let base = cache_dir.map(Path::to_path_buf).unwrap_or_else(|| std::env::temp_dir().join("pi-js"));
    let dir = base.join(format!("node-{:016x}", hasher.finish()));
    std::fs::create_dir_all(&dir).map_err(|error| io_error(error, "create the bootstrap directory"))?;
    for (name, source) in FILES {
        let path = dir.join(name);
        if std::fs::read_to_string(&path).is_ok_and(|current| current == *source) {
            continue;
        }
        let temporary = dir.join(format!("{name}.{}.{:?}.tmp", std::process::id(), std::thread::current().id()));
        std::fs::write(&temporary, source).map_err(|error| io_error(error, "write the bootstrap"))?;
        std::fs::rename(&temporary, &path).map_err(|error| io_error(error, "write the bootstrap"))?;
    }
    Ok(dir)
}

/// Start `node`, wait for the worker to report ready, and return the handle and bridge thread.
pub(crate) fn start(
    options: &JsRuntimeOptions,
    executable: &Path,
    shared: Arc<Shared>,
) -> Result<(JsHandle, JoinHandle<()>), JsError> {
    super::probe(executable)?;
    let dir = write_bootstrap(options.cache_dir.as_deref())?;
    let mut command = Command::new(executable);
    command
        .arg("--disable-warning=ExperimentalWarning")
        .arg(dir.join("main.cjs"))
        .current_dir(&shared.config.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(env) = &options.env {
        command.env_clear().envs(env);
    }
    let mut child = command.spawn().map_err(|error| io_error(error, &format!("run {}", executable.display())))?;
    let (stdin, stdout, stderr) = match (child.stdin.take(), child.stdout.take(), child.stderr.take()) {
        (Some(stdin), Some(stdout), Some(stderr)) => (stdin, stdout, stderr),
        _ => return Err(JsError::new("node started without piped stdio").with_code("ERR_PI_NODE_START")),
    };
    let client = Arc::new(NodeClient::new(child, stdin, dir.join("virtual")));
    let handle = JsHandle::node(client.clone(), shared.clone());
    let stderr_shared = shared.clone();
    std::thread::Builder::new()
        .name(format!("{}-stderr", options.name))
        .spawn(move || forward_stderr(stderr_shared, stderr))
        .map_err(|error| io_error(error, "start a thread"))?;
    let (ready, started) = mpsc::channel();
    let bridge = Bridge { handle: handle.clone(), client: client.clone(), shared: shared.clone(), ready: Some(ready) };
    let thread = std::thread::Builder::new()
        .name(options.name.clone())
        .spawn(move || bridge.run(stdout))
        .map_err(|error| io_error(error, "start a thread"))?;
    let tty = |stream| shared.output.is_tty(stream);
    let size = shared.output.terminal_size();
    let config = json!({
        "cwd": shared.config.cwd,
        "argv": shared.config.argv,
        "env": options.env,
        "memoryMb": options.memory_limit.map(|bytes| (bytes / (1024 * 1024)).max(16)),
        "stackMb": (options.max_stack_size / (1024 * 1024)).max(1),
        "tty": {
            "stdout": tty(OutputStream::Stdout),
            "stderr": tty(OutputStream::Stderr),
            "columns": size.map(|(columns, _)| columns),
            "rows": size.map(|(_, rows)| rows),
        },
    });
    client.write(&json!({ "t": "init", "config": config }))?;
    let outcome = started
        .recv_timeout(START_TIMEOUT)
        .unwrap_or_else(|_| Err(JsError::new("the Node backend did not start in time").with_code("ERR_PI_NODE_START")));
    if let Err(error) = outcome {
        client.shutdown();
        return Err(error);
    }
    Ok((handle, thread))
}
