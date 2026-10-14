//! The image entrypoint, as upstream `docker/entrypoint.ts`: validates the image, hands the
//! agent directory and artifacts to the sandbox user, then runs the selected cases.

use std::path::Path;

use crate::harness::EnvMap;

pub const EVALUATOR_DIR: &str = "/opt/pi-evals";
pub const EVALUATOR_BIN: &str = "/opt/pi-evals/pi-evals";
pub const PI_BIN: &str = "/usr/local/bin/pi";
pub const DOCS_DIR: &str = "/usr/local/share/pi/docs";
const AGENT_DIR: &str = "/tmp/pi-eval-host-agent";
const AUTH_SOURCE: &str = "/run/pi-eval-secrets/auth.json";

fn parse_id(env: &EnvMap, name: &str) -> anyhow::Result<u32> {
    match env.get(name).and_then(|value| value.parse::<u32>().ok()) {
        Some(id) if id >= 1 => Ok(id),
        _ => anyhow::bail!("{name} must be a positive integer."),
    }
}

fn assert_directory_entries(path: &str, expected: &[&str]) -> anyhow::Result<()> {
    let mut actual: Vec<String> =
        std::fs::read_dir(path)?.flatten().map(|entry| entry.file_name().to_string_lossy().into_owned()).collect();
    actual.sort();
    let mut expected: Vec<String> = expected.iter().map(|name| name.to_string()).collect();
    expected.sort();
    if actual != expected {
        anyhow::bail!("Unexpected eval image contents in {path}: {}", actual.join(", "));
    }
    Ok(())
}

/// The documentation the `with_docs` variant ships, and that `without_docs` must omit.
pub fn assert_image(variant: Option<&str>) -> anyhow::Result<()> {
    assert_directory_entries(EVALUATOR_DIR, &["pi-evals"])?;
    if !Path::new(PI_BIN).is_file() {
        anyhow::bail!("The eval image is missing {PI_BIN}.");
    }
    match variant {
        Some("without_docs") if Path::new(DOCS_DIR).exists() => {
            anyhow::bail!("without_docs contains pi documentation.")
        }
        Some("without_docs") => Ok(()),
        Some("with_docs") => {
            for name in ["README.md", "CHANGELOG.md"] {
                let path = Path::new(DOCS_DIR).join(name);
                if std::fs::metadata(&path).map_or(0, |metadata| metadata.len()) == 0 {
                    anyhow::bail!("Missing documentation: {}", path.display());
                }
            }
            Ok(())
        }
        _ => anyhow::bail!("Eval image has no valid PI_EVAL_VARIANT."),
    }
}

#[cfg(unix)]
mod unix {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use std::os::unix::process::CommandExt;
    use std::path::Path;
    use std::process::{Command, Stdio};

    pub fn chown_tree(path: &Path, uid: u32, gid: u32) -> std::io::Result<()> {
        if std::fs::symlink_metadata(path)?.is_dir() {
            for entry in std::fs::read_dir(path)? {
                chown_tree(&entry?.path(), uid, gid)?;
            }
        }
        std::os::unix::fs::lchown(path, Some(uid), Some(gid))
    }

    pub fn assert_root_only(path: &str) -> anyhow::Result<()> {
        let metadata = std::fs::metadata(path)?;
        if metadata.uid() != 0 || metadata.permissions().mode() & 0o077 != 0 {
            anyhow::bail!("Evaluator files must be readable only by root: {path}");
        }
        Ok(())
    }

    pub fn assert_sandbox_cannot_read(path: &str, uid: u32, gid: u32) -> anyhow::Result<()> {
        let tool = if Path::new(path).is_dir() { "ls" } else { "cat" };
        let mut probe = Command::new(tool);
        probe.arg(path).uid(uid).gid(gid).stdout(Stdio::null()).stderr(Stdio::null());
        if probe.status().is_ok_and(|status| status.success()) {
            anyhow::bail!("The model-facing user can read evaluator files: {path}");
        }
        Ok(())
    }
}

/// Run the entrypoint with the container arguments; returns the runner's exit code.
#[cfg(unix)]
pub fn run_container(args: &[String], env: &EnvMap) -> anyhow::Result<i32> {
    let (uid, gid) = (parse_id(env, "PI_EVAL_SANDBOX_UID")?, parse_id(env, "PI_EVAL_SANDBOX_GID")?);
    assert_image(env.get("PI_EVAL_VARIANT").map(String::as_str))?;
    for path in [EVALUATOR_DIR, EVALUATOR_BIN] {
        unix::assert_root_only(path)?;
        unix::assert_sandbox_cannot_read(path, uid, gid)?;
    }
    std::fs::create_dir_all(AGENT_DIR)?;
    if Path::new(AUTH_SOURCE).exists() {
        std::fs::copy(AUTH_SOURCE, Path::new(AGENT_DIR).join("auth.json"))?;
    }
    unix::chown_tree(Path::new(AGENT_DIR), uid, gid)?;
    unix::chown_tree(Path::new("/artifacts"), uid, gid)?;
    let discover = args.iter().any(|arg| arg == "--discover");
    let runner_args: Vec<&String> = args.iter().filter(|arg| *arg != "--discover").collect();
    if !runner_args.iter().any(|arg| arg.ends_with(".eval.rs")) {
        anyhow::bail!("The container runner requires an explicit eval file.");
    }
    let mut command = std::process::Command::new(EVALUATOR_BIN);
    command.arg("run").args(runner_args);
    if discover {
        command.args(["--list", "/artifacts/discovered-tests.json"]);
    } else {
        command.args(["--json", "/artifacts/vitest.json"]);
    }
    command
        .env("HOME", "/tmp/pi-eval-bootstrap")
        .env("USERPROFILE", "/tmp/pi-eval-bootstrap")
        .env("PI_CODING_AGENT_DIR", AGENT_DIR)
        .env("PI_EVAL_CONTAINER", "1")
        .env("PI_EVAL_PI_BIN", PI_BIN);
    let status = command.status()?;
    if env.contains_key("PI_EVAL_ARTIFACT_UID") && env.contains_key("PI_EVAL_ARTIFACT_GID") {
        let (artifact_uid, artifact_gid) =
            (parse_id(env, "PI_EVAL_ARTIFACT_UID")?, parse_id(env, "PI_EVAL_ARTIFACT_GID")?);
        unix::chown_tree(Path::new("/artifacts"), artifact_uid, artifact_gid)?;
    }
    Ok(status.code().unwrap_or(1))
}

#[cfg(not(unix))]
pub fn run_container(_args: &[String], env: &EnvMap) -> anyhow::Result<i32> {
    parse_id(env, "PI_EVAL_SANDBOX_UID")?;
    assert_image(env.get("PI_EVAL_VARIANT").map(String::as_str))?;
    anyhow::bail!("The container runner requires a Unix image.")
}
