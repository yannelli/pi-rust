//! `pi update --models` against a local catalog server, `pi update --self` against a local
//! GitHub release server, and `PI_TIMING`, all through the binary.

mod cli_support;
#[path = "agent_session/http_mock.rs"]
mod http_mock;

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use cli_support::{isolate, mock_server, setup, text_body};
use http_mock::{Reply, Server};
use pi_coding_agent::update::{ArchiveKind, archive_name, binary_name, release_target, sha256_hex};
use serde_json::{Value, json};

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Run `executable` in `root/work` with the agent directory `root/agent` and `envs`.
async fn run(executable: PathBuf, root: &Path, args: &[&str], envs: &[(&str, String)]) -> Output {
    let mut command = Command::new(executable);
    // Online, for the local servers, but without inherited credentials for real providers.
    isolate(&mut command)
        .env_remove("PI_OFFLINE")
        .args(args)
        .current_dir(root.join("work"))
        .env("PI_CODING_AGENT_DIR", root.join("agent"))
        .env("CARGO_HOME", root.join("cargo-home"))
        .env_remove("PI_TIMING")
        .stdin(Stdio::null());
    for (name, value) in envs {
        command.env(name, value);
    }
    // A fresh copy can be briefly busy on Linux while a sibling test's fork still holds its
    // write descriptor (ETXTBSY); that clears as soon as the fork execs, so retry.
    tokio::task::spawn_blocking(move || {
        for _ in 0..100 {
            match command.output() {
                Err(error) if error.kind() == std::io::ErrorKind::ExecutableFileBusy => {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                result => return result.unwrap(),
            }
        }
        command.output().unwrap()
    })
    .await
    .unwrap()
}

fn pi_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_pi"))
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[tokio::test(flavor = "multi_thread")]
async fn update_models_refreshes_the_catalog_cache_used_by_offline_starts() {
    let root = tempfile::tempdir().unwrap();
    setup(root.path(), "http://127.0.0.1:9/v1");
    let auth = json!({"anthropic": {"type": "api_key", "key": "sk-test"}});
    std::fs::write(root.path().join("agent/auth.json"), auth.to_string()).unwrap();
    let model = json!({"id": "claude-catalog-test", "name": "Catalog Test", "api": "anthropic-messages",
        "provider": "anthropic", "baseUrl": "https://api.anthropic.com", "contextWindow": 200000, "maxTokens": 8192});
    let server = Server::start(move |request| match request.path() {
        "/api/models/providers/anthropic" => Reply::json(&json!([model])),
        "/v1/models" => Reply::json(&json!({"data": [{"id": "claude-opus-4-8"}, {"id": "claude-catalog-test"}]})),
        _ => Reply::json(&json!([])),
    })
    .await;
    // Anthropic's model list goes to the local server, not the real API.
    let models_path = root.path().join("agent/models.json");
    let mut models: Value = serde_json::from_str(&std::fs::read_to_string(&models_path).unwrap()).unwrap();
    models["providers"]["anthropic"] = json!({"baseUrl": server.url});
    std::fs::write(&models_path, models.to_string()).unwrap();
    let catalog = [("PI_CATALOG_URL", server.url.clone())];

    let output = run(pi_binary(), root.path(), &["update", "--models"], &catalog).await;
    assert!(output.status.success(), "stderr: {}", text(&output.stderr));
    assert!(text(&output.stdout).contains("Model catalogs refreshed"));
    assert_eq!(server.count("/api/models/providers/anthropic"), 1);
    let store: Value =
        serde_json::from_str(&std::fs::read_to_string(root.path().join("agent/models-store.json")).unwrap()).unwrap();
    let ids: Vec<&str> =
        store["anthropic"]["models"].as_array().unwrap().iter().filter_map(|m| m["id"].as_str()).collect();
    assert_eq!(ids, ["claude-opus-4-8", "claude-catalog-test"]);
    assert_eq!(server.count("/v1/models"), 1, "the key's model list applies");
    assert!(!store.to_string().contains("sk-test"), "the key is never saved");

    let output = run(pi_binary(), root.path(), &["--offline", "--list-models", "catalog-test"], &catalog).await;
    let stdout = text(&output.stdout);
    let row = stdout.lines().nth(1).unwrap_or_default();
    assert!(row.starts_with("anthropic") && row.contains("claude-catalog-test  200K"), "{stdout}");
    assert_eq!(server.count("/api/models/providers/anthropic"), 1, "offline starts read the cache");
}

fn release_archive(tag: &str, target: &str, binary: &[u8]) -> (String, Vec<u8>) {
    let (name, kind) = archive_name(tag, target);
    let inner = format!("pi-{tag}-{target}/{}", binary_name(target));
    let bytes = match kind {
        ArchiveKind::Zip => {
            let entry = pi_coding_agent::diagnostics::zip::ZipEntry { name: inner, data: binary.to_vec() };
            pi_coding_agent::diagnostics::zip::create_zip_archive(&[entry]).unwrap()
        }
        ArchiveKind::TarGz => {
            let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            let mut builder = tar::Builder::new(encoder);
            let mut header = tar::Header::new_gnu();
            header.set_size(binary.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            builder.append_data(&mut header, inner, binary).unwrap();
            builder.into_inner().unwrap().finish().unwrap()
        }
    };
    (name, bytes)
}

/// A GitHub API server whose latest release `tag` holds an archive of `binary` for `target`.
async fn release_server(tag: &'static str, target: &str, binary: &[u8]) -> Server {
    let (name, archive) = release_archive(tag, target, binary);
    let sums = format!("{}  {name}\n", sha256_hex(&archive));
    Server::start(move |request| {
        let base = format!("http://{}", request.header("host").unwrap_or_default());
        match request.path() {
            "/repos/yannelli/pi-rust/releases/latest" => Reply::json(&json!({
            "tag_name": tag, "prerelease": false, "body": "## Changes", "assets": [
                {"name": name, "browser_download_url": format!("{base}/download/{name}"), "size": archive.len()},
                {"name": "SHA256SUMS", "browser_download_url": format!("{base}/download/SHA256SUMS"), "size": 1},
            ]})),
            "/download/SHA256SUMS" => Reply::bytes(sums.clone().into_bytes()),
            path if path == format!("/download/{name}") => Reply::bytes(archive.clone()),
            _ => Reply::status(404),
        }
    })
    .await
}

/// A copy of the `pi` binary at `dir/<binary name>`.
fn install_copy(dir: &Path, target: &str) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let executable = dir.join(binary_name(target));
    std::fs::copy(pi_binary(), &executable).unwrap();
    executable
}

#[tokio::test(flavor = "multi_thread")]
async fn update_self_replaces_a_release_install_with_the_latest_github_release() {
    let Some(target) = release_target() else { return };
    let root = tempfile::tempdir().unwrap();
    setup(root.path(), "http://127.0.0.1:9/v1");
    let server = release_server("v99.0.0", target, b"new pi binary").await;
    let executable = install_copy(&root.path().join("install"), target);
    let api = [("PI_GITHUB_API_URL", server.url.clone())];

    let output = run(executable.clone(), root.path(), &["update", "--self"], &api).await;
    assert!(output.status.success(), "stderr: {}", text(&output.stderr));
    assert!(text(&output.stdout).contains(&format!("Updated pi from {VERSION} to 99.0.0")), "{}", text(&output.stdout));
    assert_eq!(std::fs::read(&executable).unwrap(), b"new pi binary");
    assert_eq!(server.count("/download/SHA256SUMS"), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn update_self_leaves_current_installs_alone_and_points_cargo_installs_at_cargo() {
    let Some(target) = release_target() else { return };
    let root = tempfile::tempdir().unwrap();
    setup(root.path(), "http://127.0.0.1:9/v1");
    let tag: &'static str = Box::leak(format!("v{VERSION}").into_boxed_str());
    let server = release_server(tag, target, b"same version").await;
    let api = [("PI_GITHUB_API_URL", server.url.clone())];

    let executable = install_copy(&root.path().join("install"), target);
    let output = run(executable.clone(), root.path(), &["update", "--self"], &api).await;
    assert!(output.status.success(), "stderr: {}", text(&output.stderr));
    assert!(text(&output.stdout).contains(&format!("pi is already up to date (v{VERSION})")));
    assert_eq!(server.count("/download/SHA256SUMS"), 0);

    let cargo = install_copy(&root.path().join("cargo-home/bin"), target);
    let output = run(cargo.clone(), root.path(), &["update", "--self"], &api).await;
    assert_eq!(output.status.code(), Some(1));
    let stderr = text(&output.stderr);
    assert!(stderr.contains("error: pi cannot self-update this installation."), "{stderr}");
    assert!(stderr.contains(&format!("Update it with: {}", pi_coding_agent::update::CARGO_UPDATE_COMMAND)));
    assert_eq!(std::fs::read(&cargo).unwrap(), std::fs::read(pi_binary()).unwrap());
}

#[tokio::test(flavor = "multi_thread")]
async fn timings_go_to_stderr_and_json_mode_keeps_stdout_for_events() {
    let root = tempfile::tempdir().unwrap();
    let (url, _) = mock_server(vec![text_body("Hello"), text_body("Again")]).await;
    setup(root.path(), &url);
    let args = ["--model", "mock-model", "--mode", "json", "--no-session", "-p", "Hi"];
    let output = run(pi_binary(), root.path(), &args, &[("PI_TIMING", "1".into())]).await;
    assert!(output.status.success(), "stderr: {}", text(&output.stderr));
    let stderr = text(&output.stderr);
    assert!(stderr.contains("Startup Timings: main") && stderr.contains("createRuntime"), "{stderr}");
    assert_eq!(stderr.matches("Startup Timings: main").count(), 1, "{stderr}");
    let labels: Vec<&str> = stderr
        .split("Startup Timings: main ---\n")
        .nth(1)
        .unwrap()
        .lines()
        .take_while(|line| !line.contains("TOTAL"))
        .filter_map(|line| line.trim().split(':').next())
        .collect();
    let expected = [
        "parseArgs",
        "runMigrations",
        "createSessionManager",
        "createRuntime",
        "createAgentSessionRuntime",
        "readPipedStdin",
        "prepareInitialMessage",
        "initTheme",
        "resolveModelScope",
        "createAgentSession",
    ];
    assert_eq!(labels, expected, "{stderr}");
    let stdout = text(&output.stdout);
    assert!(stdout.lines().all(|line| serde_json::from_str::<Value>(line).is_ok()), "{stdout}");

    let output = run(pi_binary(), root.path(), &args, &[]).await;
    assert!(output.status.success(), "stderr: {}", text(&output.stderr));
    assert!(!text(&output.stderr).contains("Startup Timings"), "timings are off by default");

    std::fs::write(root.path().join("work/timed.ts"), "export default function () {}\n").unwrap();
    let with_extension = ["-e", "./timed.ts", "--model", "mock-model", "--mode", "json", "--no-session", "-p", "Hi"];
    let output = run(pi_binary(), root.path(), &with_extension, &[("PI_TIMING", "1".into())]).await;
    assert!(output.status.success(), "stderr: {}", text(&output.stderr));
    let stderr = text(&output.stderr);
    let extensions = stderr.split("--- Startup Timings: extensions ---\n").nth(1).expect(&stderr);
    let import = extensions.find("timed.ts module import: ").expect(extensions);
    let factory = extensions.find("timed.ts factory: ").expect(extensions);
    assert!(import < factory, "{extensions}");
}
