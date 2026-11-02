//! `pi mcp` subcommands. Ported from coding-agent's mcp-command.test.ts and the `pi mcp login`
//! cases of agent-session-mcp-oauth.test.ts.

mod common;

use std::path::Path;
use std::sync::{Arc, Mutex};

use common::fake_mcp::{Fake, FakeOptions};
use common::oauth_server::{OAuthServerOptions, start_oauth_mcp_server};
use pi_mcp::runtime::{InMemoryAuthStorageBackend, McpOAuthCredentialStore};
use pi_mcp::{McpCommandOptions, run_mcp_command};
use serde_json::{Value, json};

struct Run {
    code: i32,
    output: String,
}

fn options(dir: &Path, output: &Arc<Mutex<Vec<String>>>) -> McpCommandOptions {
    let mut options = McpCommandOptions::new(dir, dir);
    let (log, error) = (output.clone(), output.clone());
    options.log = Arc::new(move |line| log.lock().unwrap().push(line.into()));
    options.error = Arc::new(move |line| error.lock().unwrap().push(line.into()));
    options
}

async fn run(args: &[&str], servers: Option<Value>, dir: &Path) -> Run {
    run_with(args, servers, dir, |_| {}).await
}

async fn run_with(
    args: &[&str],
    servers: Option<Value>,
    dir: &Path,
    configure: impl FnOnce(&mut McpCommandOptions),
) -> Run {
    if let Some(servers) = servers {
        std::fs::write(dir.join("mcp.json"), json!({ "mcpServers": servers }).to_string()).unwrap();
    }
    let output = Arc::new(Mutex::new(Vec::new()));
    let mut options = options(dir, &output);
    configure(&mut options);
    let args: Vec<String> = args.iter().map(|arg| arg.to_string()).collect();
    let code = run_mcp_command(&args, &options).await;
    let output = output.lock().unwrap().join("\n");
    Run { code, output }
}

fn read_config(path: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn fixture() -> Value {
    json!({ "command": env!("CARGO_BIN_EXE_mcp_fake_server") })
}

fn servers() -> Value {
    json!({
        "fixture": fixture(),
        "broken": { "command": "pi-test-missing-mcp-server" },
        "parked": { "command": env!("CARGO_BIN_EXE_mcp_fake_server"), "enabled": false },
        "bad": { "args": ["no command"] }
    })
}

#[tokio::test]
async fn lists_servers_with_their_state_tools_and_errors_and_fails_while_anything_is_wrong() {
    let dir = tempfile::tempdir().unwrap();
    let Run { code, output } = run(&["list"], Some(servers()), dir.path()).await;
    assert_eq!(code, 1);
    assert!(output.contains("fixture: connected, 3 tools (codemode, global)\n"), "{output}");
    assert!(output.contains("  tools: echo, cwd, log"), "{output}");
    let broken =
        "broken: failed (codemode, global)\n  pi-test-missing-mcp-server\n  spawn pi-test-missing-mcp-server ENOENT";
    assert!(output.contains(broken), "{output}");
    assert!(output.contains("parked: disabled (codemode, global)"), "{output}");
    assert!(output.contains("config error: "), "{output}");
    assert!(output.contains("server \"bad\" needs either \"command\""), "{output}");

    let ok = tempfile::tempdir().unwrap();
    assert_eq!(run(&["list"], Some(json!({ "fixture": fixture() })), ok.path()).await.code, 0);
}

// #10526: listing every resource can take long (a large drive), so the list only notes that they exist.
#[tokio::test]
async fn notes_that_a_server_offers_resources_without_listing_them() {
    let dir = tempfile::tempdir().unwrap();
    let servers = json!({ "docs": { "url": "http://unused.invalid" } });
    let fake = Fake::new(FakeOptions { resources: true, ..FakeOptions::default() });
    let factory = fake.factory();
    let text = run_with(&["list"], Some(servers.clone()), dir.path(), |options| {
        options.create_transport = Some(factory.clone());
    })
    .await;
    assert!(text.output.contains("docs: connected, 3 tools (codemode, global)\n"), "{}", text.output);
    assert!(text.output.contains("  offers resources"), "{}", text.output);
    assert!(!text.output.contains("URI templates"), "{}", text.output);
    let json_run = run_with(&["list", "--json"], Some(servers), dir.path(), |options| {
        options.create_transport = Some(factory.clone());
    })
    .await;
    let parsed: Value = serde_json::from_str(&json_run.output).unwrap();
    assert_eq!(parsed["servers"][0]["resources"], json!(true));
    assert!(parsed["servers"][0].get("resourceTemplates").is_none());
}

#[tokio::test]
async fn prints_json_for_scripts() {
    let dir = tempfile::tempdir().unwrap();
    let servers = json!({ "fixture": fixture(), "parked": { "command": "x", "enabled": false } });
    let Run { code, output } = run(&["list", "--json"], Some(servers), dir.path()).await;
    assert_eq!(code, 0);
    let parsed: Value = serde_json::from_str(&output).unwrap();
    let summary: Vec<Value> = parsed["servers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|server| json!({ "name": server["name"], "state": server["state"], "tools": server["tools"] }))
        .collect();
    let expected = [
        json!({ "name": "fixture", "state": "connected", "tools": ["echo", "cwd", "log"] }),
        json!({ "name": "parked", "state": "disabled", "tools": [] }),
    ];
    assert_eq!(summary, expected);
    assert_eq!(parsed["errors"], json!([]));
}

#[tokio::test]
async fn rejects_unknown_servers_and_servers_without_oauth_for_login_and_logout() {
    let dir = tempfile::tempdir().unwrap();
    let unknown = run(&["login", "nope"], Some(servers()), dir.path()).await;
    assert_eq!(
        (unknown.code, unknown.output.as_str()),
        (1, "No MCP server named \"nope\". Configured: fixture, broken, parked.")
    );
    let logout = run(&["logout", "fixture"], None, dir.path()).await;
    let expected = "MCP server \"fixture\" does not use OAuth. Only HTTP servers without an Authorization header do.";
    assert_eq!((logout.code, logout.output.as_str()), (1, expected));
    assert_eq!(run(&["frobnicate"], None, dir.path()).await.code, 1);
    let help = run(&["--help"], None, dir.path()).await;
    assert_eq!(help.code, 0);
    assert!(help.output.starts_with("Usage:\n  pi mcp add <server> [options] -- <command> [args...]"));
    assert!(help.output.contains("  --timeout <seconds>     Time limit for the whole sign-in (default: 300)"));
}

#[tokio::test]
async fn suggests_a_configured_server_for_a_mistyped_name() {
    let dir = tempfile::tempdir().unwrap();
    for command in ["login", "logout"] {
        let result = run(&[command, "fixtures"], Some(servers()), dir.path()).await;
        assert_eq!(result.code, 1);
        assert_eq!(
            result.output,
            "No MCP server named \"fixtures\". Did you mean \"fixture\"? Configured: fixture, broken, parked."
        );
    }
}

#[tokio::test]
async fn reports_unknown_options_as_typed() {
    let dir = tempfile::tempdir().unwrap();
    for option in ["-l", "--local", "--unknown"] {
        let result = run(&["list", option], None, dir.path()).await;
        assert_eq!(result.code, 1);
        assert_eq!(result.output, format!("Unknown option {option}.\nUse \"pi mcp --help\" for usage."));
    }
}

#[tokio::test]
async fn restricts_login_and_logout_to_project_servers_with_local() {
    let dir = tempfile::tempdir().unwrap();
    let added = run(&["add", "-l", "local", "--", "node", "server.js"], Some(servers()), dir.path()).await;
    assert_eq!(added.code, 0, "{}", added.output);
    for command in ["login", "logout"] {
        for flag in ["-l", "--local"] {
            let local = run_with(&[command, flag, "local"], None, dir.path(), |options| {
                options.project_trusted = true;
            })
            .await;
            assert_eq!(local.code, 1);
            assert_eq!(
                local.output,
                "MCP server \"local\" does not use OAuth. Only HTTP servers without an Authorization header do."
            );
            let global = run_with(&[command, flag, "fixture"], None, dir.path(), |options| {
                options.project_trusted = true;
            })
            .await;
            assert_eq!(global.code, 1);
            assert_eq!(
                global.output,
                format!(
                    "MCP server \"fixture\" is defined in {}, not in the project; omit --local.",
                    dir.path().join("mcp.json").display()
                )
            );
        }
    }
    let untrusted = run(&["logout", "-l", "local"], None, dir.path()).await;
    assert_eq!(untrusted.code, 1);
    assert!(untrusted.output.contains("is ignored because the project is not trusted"));
    assert!(!untrusted.output.contains("does not use OAuth"));
}

#[tokio::test]
async fn adds_stdio_servers_and_passes_options_after_the_command_through() {
    let dir = tempfile::tempdir().unwrap();
    let args = ["add", "--env", "A=1", "--env", "B=x=y", "files", "--", "npx", "-y", "server", "--root", "."];
    let added = run(&args, None, dir.path()).await;
    assert_eq!(added.code, 0, "{}", added.output);
    assert!(added.output.contains("Added global MCP server \"files\""));
    let expected = json!({ "mcpServers": { "files": { "command": "npx", "args": ["-y", "server", "--root", "."], "env": { "A": "1", "B": "x=y" } } } });
    assert_eq!(read_config(&dir.path().join("mcp.json")), expected);

    // Without `--`, options after the command belong to the command too.
    let replaced = run(&["add", "files", "node", "server.js", "--port", "1"], None, dir.path()).await;
    assert!(replaced.output.contains("Replaced global MCP server \"files\""), "{}", replaced.output);
    let expected = json!({ "mcpServers": { "files": { "command": "node", "args": ["server.js", "--port", "1"] } } });
    assert_eq!(read_config(&dir.path().join("mcp.json")), expected);
}

#[tokio::test]
async fn adds_http_servers_and_keeps_other_content_of_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let args = [
        "add",
        "docs",
        "--url",
        "https://example.com/mcp",
        "--bearer-token-env-var",
        "DOCS_TOKEN",
        "--header",
        "X-Team=core",
        "--exposure",
        "direct",
        "--description",
        "Product docs",
    ];
    let Run { code, output } = run(&args, Some(json!({ "fixture": fixture() })), dir.path()).await;
    assert_eq!(code, 0, "{output}");
    assert!(!output.contains("mcp login"));
    let docs = json!({
        "url": "https://example.com/mcp",
        "headers": { "X-Team": "core", "Authorization": "Bearer ${DOCS_TOKEN}" },
        "exposure": "direct",
        "description": "Product docs"
    });
    assert_eq!(
        read_config(&dir.path().join("mcp.json")),
        json!({ "mcpServers": { "fixture": fixture(), "docs": docs } })
    );

    let args = [
        "add",
        "sentry",
        "--url",
        "https://mcp.sentry.dev/mcp",
        "--oauth-client-id",
        "pi",
        "--oauth-client-name",
        "Claude Code",
    ];
    let oauth = run(&args, None, dir.path()).await;
    assert!(oauth.output.contains("If it requires sign-in: pi mcp login sentry"), "{}", oauth.output);
    let sentry = &read_config(&dir.path().join("mcp.json"))["mcpServers"]["sentry"];
    assert_eq!(
        *sentry,
        json!({ "url": "https://mcp.sentry.dev/mcp", "oauth": { "clientId": "pi", "clientName": "Claude Code" } })
    );
}

#[tokio::test]
async fn rejects_invalid_add_invocations_without_writing() {
    let cases: [&[&str]; 8] = [
        &["add", "x"],
        &["add", "x", "--url", "https://example.com", "--", "cmd"],
        &["add", "bad name", "--", "cmd"],
        &["add", "x", "--url", "ftp://example.com"],
        &["add", "x", "--env", "A=1", "--url", "https://example.com"],
        &["add", "x", "--header", "A=1", "--", "cmd"],
        &["add", "x", "--env", "NOVALUE", "--", "cmd"],
        &["add", "x", "--exposure", "loud", "--", "cmd"],
    ];
    for args in cases {
        let dir = tempfile::tempdir().unwrap();
        let result = run(args, None, dir.path()).await;
        assert_eq!(result.code, 1, "{}", args.join(" "));
        assert!(!dir.path().join("mcp.json").exists(), "{}", args.join(" "));
    }
}

#[tokio::test]
async fn adds_and_removes_project_servers() {
    let dir = tempfile::tempdir().unwrap();
    let added = run(&["add", "-l", "local", "--", "node", "server.js"], None, dir.path()).await;
    assert!(added.output.contains("The project is not trusted"), "{}", added.output);
    let project_config = dir.path().join(".pi").join("mcp.json");
    assert_eq!(
        read_config(&project_config),
        json!({ "mcpServers": { "local": { "command": "node", "args": ["server.js"] } } })
    );

    let wrong_scope = run(&["remove", "local"], None, dir.path()).await;
    assert_eq!(wrong_scope.code, 1);
    let hint = format!("It is defined in {}; use --local.", project_config.display());
    assert!(wrong_scope.output.contains(&hint), "{}", wrong_scope.output);

    let removed = run(&["remove", "local", "--local"], None, dir.path()).await;
    assert_eq!(removed.code, 0);
    assert!(removed.output.contains("Removed project MCP server \"local\""));
    assert_eq!(read_config(&project_config), json!({ "mcpServers": {} }));
}

#[tokio::test]
async fn removes_global_servers() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(run(&["remove", "broken"], Some(servers()), dir.path()).await.code, 0);
    let config = read_config(&dir.path().join("mcp.json"));
    let names: Vec<&String> = config["mcpServers"].as_object().unwrap().keys().collect();
    assert_eq!(names, ["fixture", "parked", "bad"]);
    let missing = run(&["remove", "broken"], None, dir.path()).await;
    assert_eq!(missing.code, 1);
    assert!(missing.output.contains("No global MCP server named \"broken\""), "{}", missing.output);
}

fn in_memory(options: &mut McpCommandOptions, backend: &Arc<InMemoryAuthStorageBackend>) {
    options.credentials = Some(McpOAuthCredentialStore::with_backend(backend.clone(), None));
}

#[tokio::test]
async fn gives_up_on_login_after_the_timeout_also_while_the_authorization_server_hangs() {
    let server = start_oauth_mcp_server(OAuthServerOptions::default()).await;
    let dir = tempfile::tempdir().unwrap();
    let backend = Arc::new(InMemoryAuthStorageBackend::default());
    let servers = json!({ "issues": { "url": server.url } });
    // The user never approves in the browser.
    let unapproved = run_with(&["login", "issues", "--timeout", "0.5"], Some(servers), dir.path(), |options| {
        in_memory(options, &backend)
    })
    .await;
    assert_eq!(unapproved.code, 1);
    assert!(unapproved.output.ends_with("was cancelled or not completed within 1 seconds."), "{}", unapproved.output);

    server.state().stall.insert("/.well-known/oauth-authorization-server".into());
    let stalled =
        run_with(&["login", "issues", "--timeout", "0.5"], None, dir.path(), |options| in_memory(options, &backend))
            .await;
    assert_eq!(stalled.code, 1);
    assert!(stalled.output.contains("was cancelled or not completed within"), "{}", stalled.output);
    let invalid =
        run_with(&["login", "issues", "--timeout", "soon"], None, dir.path(), |options| in_memory(options, &backend))
            .await;
    assert_eq!((invalid.code, invalid.output.as_str()), (1, "--timeout must be a positive number of seconds."));
}

#[tokio::test]
async fn signs_in_and_out_through_the_browser() {
    let server = start_oauth_mcp_server(OAuthServerOptions::default()).await;
    let dir = tempfile::tempdir().unwrap();
    let backend = Arc::new(InMemoryAuthStorageBackend::default());
    let servers = json!({ "issues": { "url": server.url } });
    let follow = |options: &mut McpCommandOptions| {
        in_memory(options, &backend);
        options.open_url = Some(Arc::new(|url| {
            tokio::spawn(reqwest::get(url.to_string()));
        }));
    };
    let login = run_with(&["login", "issues"], Some(servers), dir.path(), follow).await;
    assert_eq!(login.code, 0, "{}", login.output);
    assert!(login.output.starts_with("Sign in to MCP server \"issues\" in your browser:\nhttp://127.0.0.1:"));
    assert!(login.output.ends_with("Signed in to MCP server \"issues\" (1 tools)."), "{}", login.output);
    assert_eq!(server.log(), ["401 none", "register", "token code"]);

    let again = run_with(&["login", "issues"], None, dir.path(), |options| in_memory(options, &backend)).await;
    assert_eq!((again.code, again.output.as_str()), (0, "Already signed in to MCP server \"issues\" (1 tools)."));
    let listed = run_with(&["list"], None, dir.path(), |options| in_memory(options, &backend)).await;
    assert_eq!(listed.code, 0, "{}", listed.output);
    assert!(listed.output.starts_with("issues: connected, 1 tool (codemode, global)"), "{}", listed.output);

    let logout = run_with(&["logout", "issues"], None, dir.path(), |options| in_memory(options, &backend)).await;
    assert_eq!((logout.code, logout.output.as_str()), (0, "Signed out of MCP server \"issues\"."));
    let listed = run_with(&["list"], None, dir.path(), |options| in_memory(options, &backend)).await;
    assert_eq!(listed.code, 1);
    assert!(listed.output.contains("issues: needs sign-in (codemode, global)\n"), "{}", listed.output);
    assert!(listed.output.contains("  sign in with: pi mcp login issues"), "{}", listed.output);
}
