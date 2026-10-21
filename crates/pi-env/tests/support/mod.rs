//! The daemon the tests run (upstream `test/daemon.ts`): `PI_ENV_DAEMON`, else `pi-env` built in this target dir.
#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use pi_chord::{BACKGROUND_CONTEXT, Context};
use pi_durable::env::{FileSystem, SharedEnv};
use pi_env::{Connection, ConnectionOptions, RemoteExecutionEnv, RemoteExecutionEnvOptions};

pub fn ctx() -> Context {
    BACKGROUND_CONTEXT.clone()
}

/// The daemon binary; built once per test binary with `cargo build -p pi-env-daemon` when not given.
pub fn daemon() -> String {
    static DAEMON: OnceLock<String> = OnceLock::new();
    DAEMON
        .get_or_init(|| {
            if let Ok(path) = std::env::var("PI_ENV_DAEMON") {
                return path;
            }
            // Test binaries live in `<target>/<profile>/deps`; the daemon in `<target>/<profile>`.
            let exe = std::env::current_exe().expect("test executable");
            let profile = exe.parent().and_then(|deps| deps.parent()).expect("profile directory").to_path_buf();
            let target = profile.parent().expect("target directory").to_path_buf();
            let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
            let status = std::process::Command::new(cargo)
                .args(["build", "-q", "-p", "pi-env-daemon", "--bin", "pi-env"])
                .env("CARGO_TARGET_DIR", &target)
                .current_dir(env!("CARGO_MANIFEST_DIR"))
                .status()
                .expect("run cargo build for the daemon");
            assert!(status.success(), "Build the daemon first: cargo build -p pi-env-daemon");
            let path: PathBuf = profile.join(format!("pi-env{}", std::env::consts::EXE_SUFFIX));
            assert!(path.exists(), "missing daemon {}", path.display());
            path.to_string_lossy().into_owned()
        })
        .clone()
}

pub fn connection() -> Connection {
    Connection::new(ConnectionOptions::new(vec![daemon()]))
}

/// A remote env in a fresh directory, with the directory and the connection kept by the guard.
pub struct Guard {
    pub dir: tempfile::TempDir,
    pub connection: Connection,
}

impl Drop for Guard {
    fn drop(&mut self) {
        self.connection.close();
    }
}

pub fn environment() -> (Arc<RemoteExecutionEnv>, Guard) {
    let dir = tempfile::Builder::new().prefix("pi-env-remote-").tempdir().expect("temp dir");
    let connection = connection();
    let cwd = dir.path().to_string_lossy().into_owned();
    let env = Arc::new(RemoteExecutionEnv::new(RemoteExecutionEnvOptions::new(connection.clone(), "pi-env:test", cwd)));
    (env, Guard { dir, connection })
}

pub fn shared(env: Arc<RemoteExecutionEnv>) -> SharedEnv {
    env
}

pub async fn write(env: &dyn FileSystem, path: &str, content: &[u8]) {
    env.write_file(path, content, &ctx()).await.expect("write");
}
