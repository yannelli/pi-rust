//! Session, resource, and trust flags of the `pi` binary: `--session-id`, `--fork`, `--name`,
//! `--export` (HTML, or Markdown for `.md` outputs), `--skill`, `--prompt-template`,
//! `--no-context-files`, and `-a`/`-na`.

mod cli_support;

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use cli_support::{events, mock_server, pi, pi_async, setup, text_body, write_settings};

fn session_files(root: &Path) -> Vec<PathBuf> {
    let sessions = root.join("agent/sessions");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&sessions)
        .into_iter()
        .flatten()
        .flatten()
        .flat_map(|dir| std::fs::read_dir(dir.path()).into_iter().flatten().flatten().map(|file| file.path()))
        .collect();
    files.sort();
    files
}

fn lines(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path).unwrap().lines().map(|line| serde_json::from_str(line).unwrap()).collect()
}

fn stderr(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

fn system_text(request: &Value) -> String {
    let messages = request["messages"].as_array().unwrap();
    messages
        .iter()
        .filter(|message| message["role"] == "system")
        .map(|message| message["content"].to_string())
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn session_id_creates_then_reopens_and_names_sessions() {
    let root = tempfile::tempdir().unwrap();
    let (url, requests) = mock_server(vec![text_body("one"), text_body("two")]).await;
    setup(root.path(), &url);
    let args = ["--model", "mock-model", "--session-id", "release-1", "-n", " Release ", "-p", "first"];
    let output = pi_async(root.path(), &args).await;
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stderr(&output).contains("Warning: No project session found with id 'release-1'; creating a new session"));
    let files = session_files(root.path());
    assert_eq!(files.len(), 1);
    let entries = lines(&files[0]);
    assert_eq!(entries[0]["id"], "release-1");
    assert!(entries.iter().any(|entry| entry["type"] == "session_info" && entry["name"] == "Release"));

    let output = pi_async(root.path(), &["--model", "mock-model", "--session-id", "release-1", "-p", "second"]).await;
    assert!(output.status.success() && !stderr(&output).contains("Warning"), "{}", stderr(&output));
    assert_eq!(session_files(root.path()).len(), 1);
    let second = requests.lock().unwrap()[1]["messages"].to_string();
    assert!(second.contains("first") && second.contains("one"), "the reopened session has its history");

    let output = pi_async(root.path(), &["--name", "  ", "-p", "x"]).await;
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("Error: --name requires a non-empty value"));
    let output = pi_async(root.path(), &["--session-id", "bad id!", "-p", "x"]).await;
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("Session id must be non-empty"));
}

#[tokio::test(flavor = "multi_thread")]
async fn fork_copies_a_session_and_rejects_conflicts() {
    let root = tempfile::tempdir().unwrap();
    let (url, requests) = mock_server(vec![text_body("original"), text_body("forked")]).await;
    setup(root.path(), &url);
    let output = pi_async(root.path(), &["--model", "mock-model", "--session-id", "base-1", "-p", "hello"]).await;
    assert!(output.status.success(), "{}", stderr(&output));

    let output = pi_async(root.path(), &["--fork", "base-1", "--continue", "-p", "x"]).await;
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("Error: --fork cannot be combined with --continue"));
    let output = pi_async(root.path(), &["--fork", "base-1", "--session-id", "base-1", "-p", "x"]).await;
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("Session already exists with id 'base-1'"));
    let output = pi_async(root.path(), &["--fork", "missing-id", "-p", "x"]).await;
    assert!(stderr(&output).contains("No session found matching 'missing-id'"));

    let output = pi_async(root.path(), &["--model", "mock-model", "--fork", "base-1", "--mode", "json", "again"]).await;
    assert!(output.status.success(), "{}", stderr(&output));
    let header = &events(&output)[0];
    assert_eq!(header["type"], "session");
    assert!(header["parentSession"].as_str().unwrap().contains("base-1"));
    assert_eq!(session_files(root.path()).len(), 2);
    let forked = requests.lock().unwrap()[1]["messages"].to_string();
    assert!(forked.contains("hello") && forked.contains("original"));
}

#[tokio::test(flavor = "multi_thread")]
async fn exports_session_files_to_html() {
    let root = tempfile::tempdir().unwrap();
    let (url, _) = mock_server(vec![text_body("exported answer")]).await;
    setup(root.path(), &url);
    let output = pi_async(root.path(), &["--model", "mock-model", "-p", "export me"]).await;
    assert!(output.status.success(), "{}", stderr(&output));
    let file = session_files(root.path()).pop().unwrap();
    let target = root.path().join("work/out.html");
    let output = pi_async(root.path(), &["--export", file.to_str().unwrap(), target.to_str().unwrap()]).await;
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), format!("Exported to: {}", target.display()));
    assert!(std::fs::read_to_string(&target).unwrap().contains("<html"));
    let output = pi_async(root.path(), &["--export", "nope.jsonl"]).await;
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("Error: File not found:"));
}

#[test]
fn exports_session_files_to_markdown() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("agent")).unwrap();
    std::fs::create_dir_all(root.path().join("work")).unwrap();
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/export_markdown.jsonl");
    let output = pi(root.path(), &["--export", fixture.to_str().unwrap(), "out.md"], None);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "Exported to: out.md");
    let text = std::fs::read_to_string(root.path().join("work/out.md")).unwrap();
    assert!(text.starts_with("# Markdown fixture\n"), "{text}");
    assert!(text.contains("### Tool call: `bash`") && !text.contains("<html"), "{text}");
}

#[tokio::test(flavor = "multi_thread")]
async fn loads_cli_skills_templates_and_context_files() {
    let root = tempfile::tempdir().unwrap();
    let (url, requests) = mock_server(vec![text_body("a"), text_body("b"), text_body("c")]).await;
    setup(root.path(), &url);
    let extra = root.path().join("extra");
    std::fs::create_dir_all(extra.join("lint")).unwrap();
    std::fs::write(extra.join("lint/SKILL.md"), "---\nname: lint\ndescription: Lint the code\n---\nRun cargo clippy.")
        .unwrap();
    std::fs::write(extra.join("greet.md"), "Say hello to $1.").unwrap();
    std::fs::write(root.path().join("work/AGENTS.md"), "Context rule: use tabs.").unwrap();
    let skill = extra.join("lint");
    let template = extra.join("greet.md");
    let args = ["--model", "mock-model", "--no-session", "--skill", skill.to_str().unwrap(), "-nt"];
    let mut with_template = args.to_vec();
    with_template.extend(["--prompt-template", template.to_str().unwrap(), "-p", "/greet Ada"]);
    let output = pi_async(root.path(), &with_template).await;
    assert!(output.status.success(), "{}", stderr(&output));
    let output = pi_async(root.path(), &[&args[..], &["-p", "/skill:lint now"]].concat()).await;
    assert!(output.status.success(), "{}", stderr(&output));
    let output = pi_async(root.path(), &["--model", "mock-model", "--no-session", "-nc", "-ns", "-p", "plain"]).await;
    assert!(output.status.success(), "{}", stderr(&output));

    let requests = requests.lock().unwrap().clone();
    assert!(requests[0]["messages"].to_string().contains("Say hello to Ada."));
    assert!(system_text(&requests[0]).contains("Context rule: use tabs."));
    let second = requests[1]["messages"].to_string();
    assert!(second.contains("Run cargo clippy.") && second.contains("now"), "{second}");
    let third = system_text(&requests[2]);
    assert!(!third.contains("Context rule") && !third.contains("lint"), "{third}");
}

#[tokio::test(flavor = "multi_thread")]
async fn project_settings_need_trust() {
    let root = tempfile::tempdir().unwrap();
    let (url, requests) = mock_server(vec![text_body("a"), text_body("b")]).await;
    setup(root.path(), &url);
    write_settings(root.path(), json!({}));
    std::fs::create_dir_all(root.path().join("work/.pi")).unwrap();
    std::fs::write(root.path().join("work/.pi/settings.json"), json!({"defaultTools": ["read"]}).to_string()).unwrap();
    for flag in ["-na", "-a"] {
        let output = pi_async(root.path(), &["--model", "mock-model", "--no-session", flag, "-p", "hi"]).await;
        assert!(output.status.success(), "{}", stderr(&output));
    }
    let requests = requests.lock().unwrap().clone();
    let tools = |request: &Value| -> Vec<String> {
        let tools = request["tools"].as_array().unwrap();
        tools.iter().map(|tool| tool["function"]["name"].as_str().unwrap().to_string()).collect()
    };
    assert_eq!(tools(&requests[0]), ["read", "bash", "edit", "write"], "-na skips project settings");
    assert_eq!(tools(&requests[1]), ["read"], "-a trusts the project");
}
