//! Builds the two variant images, discovers cases, and runs one isolated arm per container,
//! as upstream `docker.ts`.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::Serialize;
use serde_json::Value;

use crate::fsutil::{create_private_dir, sha256_hex};
use crate::harness::EnvMap;
use crate::plan::{DocumentationVariant, EvalTask};
use crate::report::task_identity_hash;

/// The crate directory that holds `docker/` and `.eval/`.
pub fn package_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct ImageRef {
    pub name: String,
    pub id: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct BuiltImages {
    pub without_docs: ImageRef,
    pub with_docs: ImageRef,
}

impl BuiltImages {
    pub fn get(&self, variant: DocumentationVariant) -> &ImageRef {
        match variant {
            DocumentationVariant::WithoutDocs => &self.without_docs,
            DocumentationVariant::WithDocs => &self.with_docs,
        }
    }
}

pub struct DockerContext {
    pub images: BuiltImages,
    pub artifact_directory: PathBuf,
    pub auth_path: PathBuf,
    pub provider: String,
    pub model: String,
    pub runs_per_variant: u32,
}

fn execute(command: &str, args: &[String], capture: bool) -> anyhow::Result<(i32, String)> {
    let mut process = Command::new(command);
    process.args(args).current_dir(package_root()).stdin(Stdio::null());
    if capture {
        let output = process.stderr(Stdio::inherit()).output()?;
        return Ok((output.status.code().unwrap_or(1), String::from_utf8_lossy(&output.stdout).into_owned()));
    }
    Ok((process.status()?.code().unwrap_or(1), String::new()))
}

fn strings(args: &[&str]) -> Vec<String> {
    args.iter().map(|arg| arg.to_string()).collect()
}

/// Build both targets of `docker/Dockerfile` from the repository root and record image IDs.
pub fn build_images(repository_root: &Path) -> anyhow::Result<BuiltImages> {
    let hash = sha256_hex(repository_root.display().to_string().as_bytes());
    let prefix = format!("pi-evals-{}", &hash[..12]);
    let mut images = BuiltImages {
        without_docs: ImageRef { name: format!("{prefix}-without-docs:local"), id: String::new() },
        with_docs: ImageRef { name: format!("{prefix}-with-docs:local"), id: String::new() },
    };
    let dockerfile = package_root().join("docker").join("Dockerfile");
    for (variant, image) in [("without_docs", &mut images.without_docs), ("with_docs", &mut images.with_docs)] {
        let mut args = strings(&["build", "--target", variant, "--tag", &image.name, "--file"]);
        args.extend([dockerfile.display().to_string(), repository_root.display().to_string()]);
        let (status, _) = execute("docker", &args, false)?;
        if status != 0 {
            anyhow::bail!("docker exited with status {status}.");
        }
        let inspect = strings(&["image", "inspect", "--format", "{{.Id}}", &image.name]);
        let (status, stdout) = execute("docker", &inspect, true)?;
        if status != 0 || stdout.trim().is_empty() {
            anyhow::bail!("Cannot inspect {}.", image.name);
        }
        image.id = stdout.trim().to_string();
    }
    Ok(images)
}

/// `<agent dir>/auth.json`, which must hold a credential for `provider`.
pub fn require_eval_auth_file(provider: &str, env: &EnvMap) -> anyhow::Result<PathBuf> {
    let agent_dir = match env.get("PI_CODING_AGENT_DIR") {
        Some(dir) => std::path::absolute(dir)?,
        None => {
            let home = env.get("HOME").or_else(|| env.get("USERPROFILE")).cloned().unwrap_or_default();
            Path::new(&home).join(".pi").join("agent")
        }
    };
    let path = agent_dir.join("auth.json");
    if !path.is_file() {
        anyhow::bail!("Eval authentication file does not exist: {}", path.display());
    }
    let credentials: Value = std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .ok_or_else(|| anyhow::anyhow!("Eval authentication file is invalid: {}", path.display()))?;
    if !credentials.as_object().is_some_and(|object| object.contains_key(provider)) {
        anyhow::bail!("Eval authentication file has no credential for provider {provider}.");
    }
    Ok(path)
}

fn environment(args: &mut Vec<String>, name: &str, value: &str) {
    args.extend(["--env".to_string(), format!("{name}={value}")]);
}

/// `docker run` arguments for one container of `variant` writing to `output_directory`.
pub fn docker_args(
    context: &DockerContext,
    variant: DocumentationVariant,
    output_directory: &Path,
    entrypoint_args: &[String],
) -> anyhow::Result<Vec<String>> {
    create_private_dir(output_directory)?;
    let mut args = strings(&["run", "--rm", "--read-only", "--tmpfs", "/tmp:rw,exec,mode=1777", "--mount"]);
    args.push(format!("type=bind,source={},target=/artifacts", output_directory.display()));
    environment(&mut args, "PI_EVAL_ARTIFACT_DIR", "/artifacts");
    environment(&mut args, "PI_EVAL_RUNS_PER_VARIANT", &context.runs_per_variant.to_string());
    environment(&mut args, "PI_EVAL_SANDBOX_UID", "65532");
    environment(&mut args, "PI_EVAL_SANDBOX_GID", "65532");
    environment(&mut args, "PI_PROVIDER", &context.provider);
    environment(&mut args, "PI_MODEL", &context.model);
    // The new directory is owned by this process's user, which gives its IDs without libc.
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = std::fs::metadata(output_directory)?;
        environment(&mut args, "PI_EVAL_ARTIFACT_UID", &metadata.uid().to_string());
        environment(&mut args, "PI_EVAL_ARTIFACT_GID", &metadata.gid().to_string());
    }
    args.push("--mount".into());
    args.push(format!(
        "type=bind,source={},target=/run/pi-eval-secrets/auth.json,readonly",
        context.auth_path.display()
    ));
    args.push(context.images.get(variant).name.clone());
    args.extend(entrypoint_args.iter().cloned());
    Ok(args)
}

/// Discover the selected cases inside `variant`'s image; returns the discovery file.
pub fn discover_cases(
    context: &DockerContext,
    variant: DocumentationVariant,
    files: &[String],
    filter_args: &[String],
) -> anyhow::Result<PathBuf> {
    let output_directory = context.artifact_directory.join("discovery").join(variant.as_str());
    let mut entrypoint = strings(&["--discover", "--project", "docs"]);
    entrypoint.extend(files.iter().cloned());
    entrypoint.extend(filter_args.iter().cloned());
    let args = docker_args(context, variant, &output_directory, &entrypoint)?;
    if execute("docker", &args, false)?.0 != 0 {
        anyhow::bail!("{variant} eval discovery failed.");
    }
    Ok(output_directory.join("discovered-tests.json"))
}

/// `^<name>$` with regex metacharacters escaped.
pub fn exact_name_pattern(name: &str) -> String {
    format!("^{}$", regex::escape(name))
}

/// Run one arm in a fresh container; returns its report when the container wrote one.
pub fn run_task(context: &DockerContext, task: &EvalTask) -> anyhow::Result<Option<PathBuf>> {
    let output_directory = context.artifact_directory.join("tasks").join(task_identity_hash(task));
    let pattern = exact_name_pattern(&format!("{} {}", task.eval_set, task.case_id));
    let entrypoint = strings(&["--project", "docs", &task.file, "--testNamePattern", &pattern]);
    let args = docker_args(context, task.variant, &output_directory, &entrypoint)?;
    execute("docker", &args, false)?;
    let report = output_directory.join("vitest.json");
    Ok(report.is_file().then_some(report))
}
