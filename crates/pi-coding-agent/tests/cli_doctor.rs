//! `pi doctor` through the binary, with a temporary agent directory and a `PATH` the test builds:
//! a healthy setup as JSON, and on Unix a fake `pi` earlier on `PATH` with broken files.

mod cli_support;

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use cli_support::{isolate, setup};
use serde_json::{Value, json};

fn bin_dir() -> PathBuf {
    Path::new(env!("CARGO_BIN_EXE_pi")).parent().unwrap().to_path_buf()
}

/// `pi doctor` with `path` as `PATH` and a terminal that supports Kitty images.
fn doctor(root: &Path, path: OsString, args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_pi"));
    isolate(&mut command, root)
        .arg("doctor")
        .args(args)
        .current_dir(root.join("work"))
        .env("PATH", path)
        .env("TERM", "xterm-256color")
        .env("TERM_PROGRAM", "WezTerm")
        .stdin(Stdio::null());
    for name in ["TMUX", "PI_IMAGE_PROTOCOL", "PI_TRUE_COLOR", "PI_HYPERLINKS"] {
        command.env_remove(name);
    }
    command.output().unwrap()
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[test]
fn doctor_json_reports_a_healthy_setup() {
    let root = tempfile::tempdir().unwrap();
    setup(root.path(), "http://127.0.0.1:9/v1");
    std::fs::write(root.path().join("agent/settings.json"), json!({"theme": "dark"}).to_string()).unwrap();

    let output = doctor(root.path(), bin_dir().into_os_string(), &["--json"]);
    assert_eq!(output.status.code(), Some(0), "stdout: {}\nstderr: {}", text(&output.stdout), text(&output.stderr));
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["ok"], json!(true), "{report:#}");
    assert_eq!(report["problems"], json!([]));
    assert_eq!(report["version"], json!(env!("CARGO_PKG_VERSION")));
    assert!(report["target"].as_str().is_some_and(|target| !target.is_empty()));
    assert!(report["install"]["method"].is_string() && report["install"]["executable"].is_string());
    assert_eq!((report["path"]["onPath"].clone(), report["path"]["shadowed"].clone()), (json!(true), json!(false)));
    assert_eq!(report["path"]["currentIndex"], json!(0));
    let names: Vec<&str> =
        report["config"]["files"].as_array().unwrap().iter().map(|file| file["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["settings", "project settings", "models", "auth"]);
    assert_eq!(report["config"]["files"][0]["exists"], json!(true));
    let mock = json!({"provider": "mock", "authType": "api_key", "source": "configured API key"});
    assert!(report["auth"]["providers"].as_array().unwrap().contains(&mock), "{report:#}");
    assert_eq!(report["mcp"], json!({"projectTrusted": false, "servers": [], "errors": []}));
    let terminal = &report["terminal"];
    assert_eq!(terminal["term"], json!("xterm-256color"));
    assert_eq!(
        (terminal["colorDepth"].clone(), terminal["imageProtocol"].clone()),
        (json!("truecolor"), json!("kitty"))
    );
    assert_eq!((terminal["interactive"].clone(), terminal["keyboardProtocol"].clone()), (json!(false), Value::Null));
    assert!(!text(&output.stdout).contains("secret-key"), "the models.json key leaked");

    let usage = doctor(root.path(), bin_dir().into_os_string(), &["--bogus"]);
    assert_eq!(usage.status.code(), Some(1));
    assert!(text(&usage.stderr).contains("Unknown argument --bogus"));
    let help = doctor(root.path(), bin_dir().into_os_string(), &["--help"]);
    assert!(help.status.success() && text(&help.stdout).contains("doctor [--json]"));
}

#[test]
fn doctor_warns_when_a_moved_feature_extension_is_missing() {
    let root = tempfile::tempdir().unwrap();
    setup(root.path(), "http://127.0.0.1:9/v1");
    let settings = json!({"statusLine": {"preset": "pills"}, "terminal": {"iterm2": false}});
    std::fs::write(root.path().join("agent/settings.json"), settings.to_string()).unwrap();

    let output = doctor(root.path(), bin_dir().into_os_string(), &["--json"]);
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    let warnings: Vec<&str> = report["warnings"].as_array().unwrap().iter().filter_map(Value::as_str).collect();
    let moved: Vec<&&str> = warnings.iter().filter(|warning| warning.contains("marketplace:")).collect();
    assert_eq!(moved.len(), 1, "{warnings:?}");
    assert!(moved[0].contains("`statusLine` is set") && moved[0].contains("`pi install marketplace:statusline`"));
}

#[cfg(unix)]
#[test]
fn doctor_reports_a_pi_earlier_on_path_and_broken_files_without_secrets() {
    use std::os::unix::fs::PermissionsExt;

    let root = tempfile::tempdir().unwrap();
    setup(root.path(), "http://127.0.0.1:9/v1");
    let agent = root.path().join("agent");
    std::fs::write(agent.join("settings.json"), "{\"theme\": ").unwrap();
    let auth = json!({"anthropic": {"type": "api_key", "key": "sk-doctor-secret"}});
    std::fs::write(agent.join("auth.json"), auth.to_string()).unwrap();
    let executable = |path: &Path| {
        std::fs::write(path, "#!/bin/sh\necho fake\n").unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    };
    let fake = root.path().join("fake-bin");
    std::fs::create_dir_all(&fake).unwrap();
    executable(&fake.join("pi"));
    let server = root.path().join("server.sh");
    executable(&server);
    let mcp = json!({"mcpServers": {
        "gone": {"command": "no-such-mcp-server"},
        "local": {"command": server.display().to_string(), "args": ["--token", "arg-secret"]},
        "off": {"command": "also-missing", "enabled": false},
        "web": {"url": "https://mcp.example.com/mcp?token=url-secret"},
    }});
    std::fs::write(agent.join("mcp.json"), mcp.to_string()).unwrap();

    let path = std::env::join_paths([fake.clone(), bin_dir()]).unwrap();
    let output = doctor(root.path(), path.clone(), &[]);
    let stdout = text(&output.stdout);
    assert_eq!(output.status.code(), Some(1), "stdout: {stdout}\nstderr: {}", text(&output.stderr));
    assert!(stdout.contains(&fake.join("pi").display().to_string()), "{stdout}");
    assert!(stdout.contains(&bin_dir().join("pi").display().to_string()), "{stdout}");
    assert!(stdout.contains("Another pi comes first on PATH"), "{stdout}");
    assert!(stdout.contains("Invalid settings file"), "{stdout}");
    assert!(stdout.contains("MCP server \"gone\": command \"no-such-mcp-server\" not found"), "{stdout}");
    assert!(!stdout.contains("also-missing\" not found"), "a disabled server is not a problem: {stdout}");
    assert!(stdout.contains("anthropic") && stdout.contains("API key from stored credential"), "{stdout}");
    assert!(stdout.contains("https://mcp.example.com"), "{stdout}");
    for secret in ["sk-doctor-secret", "secret-key", "url-secret", "arg-secret"] {
        assert!(!stdout.contains(secret), "{secret} leaked: {stdout}");
    }

    let json_output = doctor(root.path(), path, &["--json"]);
    assert_eq!(json_output.status.code(), Some(1));
    let report: Value = serde_json::from_slice(&json_output.stdout).unwrap();
    assert_eq!((report["ok"].clone(), report["path"]["shadowed"].clone()), (json!(false), json!(true)));
    assert_eq!(report["path"]["found"][0], json!(fake.join("pi").display().to_string()));
    assert_eq!(report["problems"].as_array().map(Vec::len), Some(3), "{report:#}");
    let servers = report["mcp"]["servers"].as_array().unwrap();
    let local = servers.iter().find(|server| server["name"] == "local").unwrap();
    assert_eq!(local["resolved"], json!(server.display().to_string()));
    assert!(!json_output.stdout.windows(10).any(|window| window == b"sk-doctor-"));
}
