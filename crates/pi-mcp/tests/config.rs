//! `mcp.json` loading, overrides, validation, and exposure, ported from `mcp-extension.test.ts`.

use std::path::{Path, PathBuf};

use pi_mcp::config::{McpServerConfigPatch, add_mcp_server_config, remove_mcp_server_config, update_mcp_server_config};
use pi_mcp::{McpConfigScope, McpExposure, McpServerConfig, get_mcp_tool_exposure, load_mcp_config};
use serde_json::{Value, json};

struct Paths {
    _root: tempfile::TempDir,
    agent_dir: PathBuf,
    cwd: PathBuf,
}

impl Paths {
    fn project_file(&self) -> PathBuf {
        self.cwd.join(".pi").join("mcp.json")
    }
}

fn setup(global: Value, project: Value) -> Paths {
    let root = tempfile::tempdir().unwrap();
    let agent_dir = root.path().join("agent");
    let cwd = root.path().join("project");
    std::fs::create_dir_all(&agent_dir).unwrap();
    std::fs::create_dir_all(cwd.join(".pi")).unwrap();
    std::fs::write(agent_dir.join("mcp.json"), global.to_string()).unwrap();
    std::fs::write(cwd.join(".pi").join("mcp.json"), project.to_string()).unwrap();
    Paths { _root: root, agent_dir, cwd }
}

fn config_json(config: &McpServerConfig) -> Value {
    serde_json::to_value(config).unwrap()
}

fn read(path: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

#[test]
fn merges_global_and_trusted_project_servers_and_validates_entries() {
    let paths = setup(
        json!({ "mcpServers": {
            "shared": { "command": "global-cmd" },
            "remote": { "url": "https://example.com/mcp", "headers": { "Authorization": "Bearer ${TOKEN}" } },
            "off": { "command": "x", "enabled": false },
            "bad": { "args": ["no command"] },
            "legacy": { "type": "sse", "url": "https://example.com/sse" },
            "badUrl": { "url": "example.com/mcp" },
            "bad name": { "command": "x" }
        } }),
        json!({ "mcpServers": { "shared": { "command": "project-cmd", "exposure": "direct" } } }),
    );
    let trusted = load_mcp_config(&paths.agent_dir, &paths.cwd, true);
    let servers: Vec<(String, Option<McpConfigScope>, Value)> =
        trusted.servers.iter().map(|entry| (entry.name.clone(), entry.scope, config_json(&entry.config))).collect();
    assert_eq!(
        servers,
        vec![
            ("shared".into(), Some(McpConfigScope::Project), json!({ "command": "project-cmd", "exposure": "direct" })),
            (
                "remote".into(),
                Some(McpConfigScope::Global),
                json!({ "url": "https://example.com/mcp", "headers": { "Authorization": "Bearer ${TOKEN}" } })
            ),
            ("off".into(), Some(McpConfigScope::Global), json!({ "command": "x", "enabled": false })),
        ]
    );
    assert_eq!(trusted.errors.len(), 4);
    assert!(trusted.errors[0].contains("server \"bad\" needs either \"command\""));
    assert!(trusted.errors[1].contains("legacy SSE transport is not supported"));
    assert!(trusted.errors[2].contains("server \"badUrl\": url must be an http or https URL"));
    assert!(trusted.errors[3].contains("invalid server name \"bad name\""));
    assert_eq!(trusted.project_config, Some(paths.project_file()));

    // Untrusted projects cannot add or override servers, since stdio servers run commands.
    let untrusted = load_mcp_config(&paths.agent_dir, &paths.cwd, false);
    let shared = untrusted.servers.iter().find(|entry| entry.name == "shared").unwrap();
    assert_eq!(config_json(&shared.config), json!({ "command": "global-cmd" }));
    assert_eq!(untrusted.project_config, None);
}

#[test]
fn lets_project_entries_override_enabled_and_exposure_of_global_servers() {
    let paths = setup(
        json!({ "mcpServers": { "tools": { "command": "x", "env": { "TOKEN": "secret" } } } }),
        // An override cannot change the command, which would run with the global env.
        json!({ "mcpServers": { "tools": { "enabled": false, "args": ["y"] }, "missing": { "enabled": false } } }),
    );
    let loaded = load_mcp_config(&paths.agent_dir, &paths.cwd, true);
    assert_eq!(loaded.servers.len(), 1);
    assert_eq!(loaded.servers[0].override_path, None);
    assert_eq!(config_json(&loaded.servers[0].config), json!({ "command": "x", "env": { "TOKEN": "secret" } }));
    assert_eq!(loaded.errors.len(), 2);
    assert!(loaded.errors[0].contains("server \"tools\": an override can only set enabled, exposure, toolExposure"));
    assert!(
        loaded.errors[1].contains("server \"missing\" needs \"command\" or \"url\", or a global server to override")
    );

    let project = paths.project_file();
    std::fs::write(&project, json!({ "mcpServers": { "tools": { "enabled": false } } }).to_string()).unwrap();
    let tools = load_mcp_config(&paths.agent_dir, &paths.cwd, true).servers.remove(0);
    assert_eq!(tools.override_path.as_deref(), Some(project.display().to_string().as_str()));
    assert_eq!(config_json(&tools.config), json!({ "command": "x", "env": { "TOKEN": "secret" }, "enabled": false }));

    // Overrides keep `enabled: true`, since it replaces the global value.
    update_mcp_server_config(&project, "tools", McpServerConfigPatch { enabled: Some(true), exposure: None }, false)
        .unwrap();
    assert_eq!(read(&project)["mcpServers"]["tools"], json!({ "enabled": true }));
    // Definitions drop default values instead.
    let global = paths.agent_dir.join("mcp.json");
    let patch = McpServerConfigPatch { enabled: Some(false), exposure: Some(McpExposure::Direct) };
    update_mcp_server_config(&global, "tools", patch, false).unwrap();
    let patch = McpServerConfigPatch { enabled: Some(true), exposure: Some(McpExposure::Codemode) };
    update_mcp_server_config(&global, "tools", patch, false).unwrap();
    assert_eq!(read(&global)["mcpServers"]["tools"], json!({ "command": "x", "env": { "TOKEN": "secret" } }));
    // A missing entry is added as an override when asked to.
    update_mcp_server_config(&project, "other", McpServerConfigPatch { enabled: Some(false), exposure: None }, true)
        .unwrap();
    assert_eq!(read(&project)["mcpServers"]["other"], json!({ "enabled": false }));
    let missing = update_mcp_server_config(&global, "nope", McpServerConfigPatch::default(), false).unwrap_err();
    assert!(missing.ends_with("does not define MCP server \"nope\""));
}

#[test]
fn rejects_server_names_that_differ_only_in_dash_and_underscore() {
    let paths = setup(
        json!({ "mcpServers": { "work-files": { "command": "a" }, "work_files": { "command": "b" } } }),
        json!({}),
    );
    let loaded = load_mcp_config(&paths.agent_dir, &paths.cwd, false);
    assert_eq!(loaded.servers.iter().map(|entry| entry.name.as_str()).collect::<Vec<_>>(), vec!["work-files"]);
    assert_eq!(loaded.errors.len(), 1);
    assert!(loaded.errors[0].contains("server \"work_files\" conflicts with \"work-files\""));
}

#[test]
fn validates_exposure_and_reads_auto_enable_codemode_with_project_precedence() {
    let paths = setup(
        json!({
            "autoEnableCodemode": false,
            "mcpServers": {
                "later": { "command": "x", "exposure": "deferred" },
                "scripts": { "command": "x", "exposure": "codemode-deferred", "toolExposure": { "a": "codemode-deferred" } },
                "off": { "command": "x", "exposure": "hidden" },
                "wrong": { "command": "x", "exposure": "model-only" },
                "described": { "command": "x", "description": "Docs search" },
                "badDescription": { "command": "x", "description": 1 }
            }
        }),
        json!({ "autoEnableCodemode": "yes", "mcpServers": {} }),
    );
    let untrusted = load_mcp_config(&paths.agent_dir, &paths.cwd, false);
    assert_eq!(untrusted.auto_enable_codemode, Some(false));
    let exposures: Vec<(&str, Option<McpExposure>)> =
        untrusted.servers.iter().map(|entry| (entry.name.as_str(), entry.config.exposure)).collect();
    assert_eq!(
        exposures,
        vec![
            ("later", Some(McpExposure::Deferred)),
            ("scripts", Some(McpExposure::Codemode)),
            ("off", Some(McpExposure::Hidden)),
            ("described", None),
        ]
    );
    assert_eq!(untrusted.servers[1].config.tool_exposures(), vec![("a", McpExposure::Codemode)]);
    assert_eq!(untrusted.servers[3].config.description.as_deref(), Some("Docs search"));
    assert_eq!(untrusted.errors.len(), 2);
    assert!(untrusted.errors[0].contains("server \"wrong\": exposure must be one of"));
    assert!(untrusted.errors[1].contains("server \"badDescription\": description must be a string"));

    let trusted = load_mcp_config(&paths.agent_dir, &paths.cwd, true);
    assert_eq!(trusted.auto_enable_codemode, Some(false));
    assert!(trusted.errors.iter().any(|error| error.contains("autoEnableCodemode must be a boolean")));
}

#[test]
fn validates_the_oauth_callback_url_scope_and_client_name() {
    let paths = setup(
        json!({ "mcpServers": {
            "ok": { "url": "https://a.example/mcp", "oauth": { "callbackUrl": "http://localhost:8080/callback", "scope": "a b" } },
            "ipv6": { "url": "https://a.example/mcp", "oauth": { "callbackUrl": "http://[::1]/cb", "callbackPort": 9000 } },
            "same": { "url": "https://a.example/mcp", "oauth": { "callbackUrl": "http://127.0.0.1:2/cb", "callbackPort": 2 } },
            "remote": { "url": "https://a.example/mcp", "oauth": { "callbackUrl": "https://example.com/callback" } },
            "both": { "url": "https://a.example/mcp", "oauth": { "callbackUrl": "http://127.0.0.1:1/cb", "callbackPort": 2 } },
            "scope": { "url": "https://a.example/mcp", "oauth": { "scope": ["a"] } },
            "named": { "url": "https://a.example/mcp", "oauth": { "clientName": "Claude Code" } },
            "unnamed": { "url": "https://a.example/mcp", "oauth": { "clientName": " " } },
            "metadata": { "url": "https://a.example/mcp", "oauth": { "authServerMetadataUrl": "https://idp.example/m" } },
            "plainMetadata": { "url": "https://a.example/mcp", "oauth": { "authServerMetadataUrl": "http://idp.example/m" } },
            "cimd": { "url": "https://a.example/mcp", "oauth": { "clientRegistration": "cimd", "callbackUrl": "http://localhost/callback" } },
            "badRegistration": { "url": "https://a.example/mcp", "oauth": { "clientRegistration": "auto" } },
            "cimdClient": { "url": "https://a.example/mcp", "oauth": { "clientRegistration": "cimd", "clientId": "x" } },
            "cimdPath": { "url": "https://a.example/mcp", "oauth": { "clientRegistration": "cimd", "callbackUrl": "http://127.0.0.1/cb" } }
        } }),
        json!({}),
    );
    let loaded = load_mcp_config(&paths.agent_dir, &paths.cwd, false);
    let names: Vec<&str> = loaded.servers.iter().map(|entry| entry.name.as_str()).collect();
    assert_eq!(names, vec!["ok", "ipv6", "same", "named", "metadata", "cimd"]);
    let expected = [
        "server \"remote\": oauth.callbackUrl must be an http URI on localhost",
        "server \"both\": oauth.callbackUrl and oauth.callbackPort name different ports",
        "server \"scope\": oauth.scope must be a string",
        "server \"unnamed\": oauth.clientName must be a non-empty string",
        "server \"plainMetadata\": oauth.authServerMetadataUrl must be an https URL",
        "server \"badRegistration\": oauth.clientRegistration must be \"dcr\" or \"cimd\"",
        "server \"cimdClient\": oauth.clientRegistration \"cimd\" cannot be combined",
        "server \"cimdPath\": oauth.clientRegistration \"cimd\" requires oauth.callbackUrl",
    ];
    assert_eq!(loaded.errors.len(), expected.len(), "{:?}", loaded.errors);
    for (error, expected) in loaded.errors.iter().zip(expected) {
        assert!(error.contains(expected), "{error} lacks {expected}");
    }
}

#[test]
fn resolves_per_tool_exposure_from_exact_names_then_patterns_in_order() {
    let paths = setup(
        json!({ "mcpServers": {
            "gh": {
                "command": "x",
                "exposure": "deferred",
                "toolExposure": { "get_*": "codemode", "get_me": "direct", "*delete*": "hidden", "get_file.*": "direct" }
            },
            "bad": { "command": "x", "toolExposure": { "a": "visible" } }
        } }),
        json!({}),
    );
    let loaded = load_mcp_config(&paths.agent_dir, &paths.cwd, false);
    assert_eq!(loaded.errors.len(), 1);
    assert!(loaded.errors[0].contains("server \"bad\": toolExposure \"a\" must be one of"));
    let config = &loaded.servers[0].config;
    assert_eq!(get_mcp_tool_exposure(config, "get_me"), McpExposure::Direct);
    assert_eq!(get_mcp_tool_exposure(config, "get_issue"), McpExposure::Codemode);
    assert_eq!(get_mcp_tool_exposure(config, "get_delete_hint"), McpExposure::Codemode);
    assert_eq!(get_mcp_tool_exposure(config, "delete_repo"), McpExposure::Hidden);
    assert_eq!(get_mcp_tool_exposure(config, "list_issues"), McpExposure::Deferred);
    // Only `*` is special.
    let only_star: McpServerConfig =
        serde_json::from_value(json!({ "command": "x", "toolExposure": { "get_file.*": "direct" } })).unwrap();
    assert_eq!(get_mcp_tool_exposure(&only_star, "get_file_x"), McpExposure::Codemode);
}

#[test]
fn validates_provider_auth_and_accepts_it_only_in_the_global_mcp_json() {
    let paths = setup(
        json!({ "mcpServers": {
            "radius": { "url": "https://radius.example/mcp", "auth": { "provider": "radius" } },
            "local": { "url": "http://localhost:8788/mcp", "auth": { "provider": "radius-dev" } },
            "plain": { "url": "http://radius.example/mcp", "auth": { "provider": "radius" } },
            "empty": { "url": "https://radius.example/mcp", "auth": { "provider": "" } }
        } }),
        json!({ "mcpServers": { "radius": { "url": "https://evil.example/mcp", "auth": { "provider": "radius" } } } }),
    );
    let loaded = load_mcp_config(&paths.agent_dir, &paths.cwd, true);
    let servers: Vec<(&str, Option<McpConfigScope>, Option<&str>)> =
        loaded.servers.iter().map(|entry| (entry.name.as_str(), entry.scope, entry.config.url.as_deref())).collect();
    assert_eq!(
        servers,
        vec![
            ("radius", Some(McpConfigScope::Global), Some("https://radius.example/mcp")),
            ("local", Some(McpConfigScope::Global), Some("http://localhost:8788/mcp")),
        ]
    );
    assert_eq!(loaded.errors.len(), 3);
    assert!(loaded.errors[0].contains("server \"plain\": auth requires an https URL"));
    assert!(loaded.errors[1].contains("server \"empty\": auth.provider must be a provider name"));
    assert!(loaded.errors[2].contains("server \"radius\": auth is only allowed in the global mcp.json"));
    assert!(!loaded.servers[0].config.uses_oauth());
}

#[test]
fn adds_and_removes_entries_keeping_other_content_and_indentation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mcp.json");
    std::fs::write(&path, "{\n\t\"other\": 1,\n\t\"mcpServers\": {\n\t\t\"a\": { \"command\": \"x\" },\n\t\t\"b\": { \"command\": \"y\" }\n\t}\n}\n").unwrap();
    assert!(!add_mcp_server_config(&path, "c", &json!({ "url": "https://c.example/mcp" })).unwrap());
    assert!(add_mcp_server_config(&path, "a", &json!({ "command": "z" })).unwrap());
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.starts_with("{\n\t\"other\": 1,\n\t\"mcpServers\": {\n\t\t\"a\": {"), "{text}");
    assert!(remove_mcp_server_config(&path, "b").unwrap());
    assert!(!remove_mcp_server_config(&path, "b").unwrap());
    let servers: Vec<String> = read(&path)["mcpServers"].as_object().unwrap().keys().cloned().collect();
    assert_eq!(servers, vec!["a", "c"]);
    assert!(!remove_mcp_server_config(&dir.path().join("missing.json"), "a").unwrap());
    std::fs::write(&path, "[]").unwrap();
    let error = add_mcp_server_config(&path, "a", &json!({ "command": "x" })).unwrap_err();
    assert!(error.ends_with("expected an object with an \"mcpServers\" object"));
}
