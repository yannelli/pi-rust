//! Writing the embedded bootstrap scripts and starting the `node` child.

use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde_json::json;

use super::bridge::Bridge;
use super::client::NodeClient;
use super::transport::{self, Listener, PORT_ENV, TOKEN_ENV};
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
    ("require_rewrite.mjs", include_str!("js/require_rewrite.mjs")),
];

const START_TIMEOUT: Duration = Duration::from_secs(60);

fn io_error(error: std::io::Error, what: &str) -> JsError {
    JsError::new(format!("cannot {what} for the Node backend: {error}")).with_code("ERR_PI_NODE_START")
}

/// The scripts' directory: `node-<hash>` under `cache_dir`, or a new temporary directory.
fn bootstrap_dir(cache_dir: Option<&Path>) -> Result<(PathBuf, Option<tempfile::TempDir>), JsError> {
    let Some(base) = cache_dir else {
        let temp = tempfile::Builder::new()
            .prefix("pi-js-node-")
            .tempdir()
            .map_err(|error| io_error(error, "create the bootstrap directory"))?;
        return Ok((temp.path().to_path_buf(), Some(temp)));
    };
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    env!("CARGO_PKG_VERSION").hash(&mut hasher);
    FILES.hash(&mut hasher);
    let dir = base.join(format!("node-{:016x}", hasher.finish()));
    private_dir(base)?;
    private_dir(&dir)?;
    Ok((dir, None))
}

/// Create `dir` if needed and check that only this user can use it.
fn private_dir(dir: &Path) -> Result<(), JsError> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(dir).map_err(|error| io_error(error, "create the bootstrap directory"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let refuse = |why: String| {
            JsError::new(format!("refusing the Node bootstrap directory {}: {why}", dir.display()))
                .with_code("ERR_PI_NODE_START")
        };
        let metadata =
            std::fs::symlink_metadata(dir).map_err(|error| io_error(error, "check the bootstrap directory"))?;
        if !metadata.is_dir() {
            return Err(refuse("it is not a directory".into()));
        }
        if metadata.mode() & 0o077 != 0 {
            return Err(refuse(format!("its permissions are {:o}; they must be 700", metadata.mode() & 0o777)));
        }
        let probe = tempfile::tempfile_in(dir).map_err(|error| io_error(error, "check the bootstrap directory"))?;
        let user = probe.metadata().map_err(|error| io_error(error, "check the bootstrap directory"))?.uid();
        if metadata.uid() != user {
            return Err(refuse(format!("it belongs to user {}", metadata.uid())));
        }
    }
    Ok(())
}

/// Write the scripts to `dir`; each file is written to a temporary name and renamed, so
/// processes starting at once do not see partial files.
fn write_bootstrap(dir: &Path) -> Result<(), JsError> {
    for (name, source) in FILES {
        let path = dir.join(name);
        if std::fs::read_to_string(&path).is_ok_and(|current| current == *source) {
            continue;
        }
        let temporary = dir.join(format!("{name}.{}.{:?}.tmp", std::process::id(), std::thread::current().id()));
        std::fs::write(&temporary, source).map_err(|error| io_error(error, "write the bootstrap"))?;
        std::fs::rename(&temporary, &path).map_err(|error| io_error(error, "write the bootstrap"))?;
    }
    Ok(())
}

/// Start `node`, wait for the worker to report ready, and return the handle and bridge thread.
pub(crate) fn start(
    options: &JsRuntimeOptions,
    executable: &Path,
    shared: Arc<Shared>,
) -> Result<(JsHandle, JoinHandle<()>), JsError> {
    super::probe(executable)?;
    let (dir, temp) = bootstrap_dir(options.cache_dir.as_deref())?;
    write_bootstrap(&dir)?;
    let listener = Listener::bind()?;
    let mut command = Command::new(executable);
    command
        .arg("--disable-warning=ExperimentalWarning")
        .arg(dir.join("main.cjs"))
        .current_dir(&shared.config.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(env) = &options.env {
        command.env_clear().envs(env);
    }
    command.env(PORT_ENV, listener.port()?.to_string()).env(TOKEN_ENV, listener.token());
    let mut child = command.spawn().map_err(|error| io_error(error, &format!("run {}", executable.display())))?;
    let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
        return Err(JsError::new("node started without piped stdio").with_code("ERR_PI_NODE_START"));
    };
    transport::forward(format!("{}-stdout", options.name), shared.output.clone(), OutputStream::Stdout, stdout)?;
    transport::forward(format!("{}-stderr", options.name), shared.output.clone(), OutputStream::Stderr, stderr)?;
    let deadline = Instant::now() + START_TIMEOUT;
    let socket = match listener.accept(deadline, || !matches!(child.try_wait(), Ok(None))) {
        Ok(socket) => socket,
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }
    };
    let reader = socket.try_clone().map_err(|error| io_error(error, "share the connection"))?;
    let client = Arc::new(NodeClient::new(child, socket, dir.join("virtual"), temp));
    let handle = JsHandle::node(client.clone(), shared.clone());
    let (ready, started) = mpsc::channel();
    let bridge = Bridge { handle: handle.clone(), client: client.clone(), shared: shared.clone(), ready: Some(ready) };
    let thread = std::thread::Builder::new()
        .name(options.name.clone())
        .spawn(move || bridge.run(reader))
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
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .unwrap_or_else(|_| Err(JsError::new("the Node backend did not start in time").with_code("ERR_PI_NODE_START")));
    if let Err(error) = outcome {
        client.shutdown();
        return Err(error);
    }
    Ok((handle, thread))
}
