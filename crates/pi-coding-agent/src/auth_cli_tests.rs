use std::path::Path;

use serde_json::json;

use super::*;

fn args(line: &str) -> Vec<String> {
    line.split_whitespace().map(String::from).collect()
}

fn agent_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let model = |id: &str| json!({"id": id, "contextWindow": 1000, "maxTokens": 100});
    let models = json!({"providers": {
        "alpha": {"baseUrl": "http://127.0.0.1:9", "api": "openai-completions", "apiKey": "alpha-key",
            "models": [model("shared-model")]},
        "beta": {"baseUrl": "http://127.0.0.1:9", "api": "openai-completions", "authHeader": true,
            "models": [model("shared-model"), model("beta-only")]},
        "gamma": {"baseUrl": "http://127.0.0.1:9", "api": "openai-completions", "models": [model("gamma-only")]},
    }});
    std::fs::write(dir.path().join("models.json"), models.to_string()).unwrap();
    let auth = json!({"beta": {"type": "api_key", "key": "beta-key"}});
    std::fs::write(dir.path().join("auth.json"), auth.to_string()).unwrap();
    dir
}

async fn run(dir: &Path, line: &str) -> AuthOutput {
    run_auth_command(&args(line), dir, Arc::new(|_: &str| None)).await.expect("auth command")
}

#[test]
fn parses_commands_and_flags_like_upstream() {
    assert_eq!(parse_auth_command(&args("--model x")), Ok(None));
    let command =
        parse_auth_command(&args("auth check --provider p --json --credentials --no-refresh")).unwrap().unwrap();
    assert_eq!(command.kind, AuthCommandKind::Check);
    assert!(command.json && command.credentials && command.no_refresh);
    assert_eq!(command.args, ["--provider", "p"]);
    let bearer = parse_auth_command(&args("auth print-bearer-token --min-expiry 2h")).unwrap().unwrap();
    assert_eq!(bearer.min_expiry_ms, Some(7_200_000));
    assert_eq!(
        parse_auth_command(&args("auth print-api-key --json")),
        Err("--json is only supported by auth check".into())
    );
    assert_eq!(
        parse_auth_command(&args("auth check --min-expiry 1h")),
        Err("--min-expiry is only supported by print-bearer-token".into())
    );
    assert!(parse_auth_command(&args("auth print-bearer-token --min-expiry 1d")).unwrap_err().contains("30m or 1h"));
    assert!(parse_auth_command(&args("auth login")).unwrap_err().starts_with("Unknown auth command \"login\""));
    assert_eq!(
        [parse_min_expiry("500ms"), parse_min_expiry("45S"), parse_min_expiry("m")],
        [Some(500), Some(45_000), None]
    );
    assert!(is_auth_command_help(&args("auth")) && is_auth_command_help(&args("auth check -h")));
}

#[test]
fn extracts_api_keys_and_bearer_tokens() {
    let mut resolved = ResolvedAuth::default();
    assert_eq!(auth_credential(Some(&resolved)), None);
    resolved.auth.headers.insert("authorization".into(), "Bearer tok-1".into());
    assert_eq!(auth_credential(Some(&resolved)).as_deref(), Some("tok-1"));
    resolved.auth.api_key = Some("key-1".into());
    assert_eq!(auth_credential(Some(&resolved)).as_deref(), Some("key-1"));
}

#[tokio::test]
async fn checks_provider_readiness_with_exit_codes() {
    let dir = agent_dir();
    let ready = run(dir.path(), "auth check --provider beta --json").await;
    assert_eq!(ready.code, 0, "{ready:?}");
    assert_eq!(
        serde_json::from_str::<Value>(&ready.stdout).unwrap(),
        json!({"status": "ready", "provider": "beta", "authType": "api_key"})
    );
    let with_key = run(dir.path(), "auth check --model beta-only --credentials").await;
    assert_eq!((with_key.code, with_key.stdout.as_str()), (0, "beta-key\n"));
    let missing = run(dir.path(), "auth check --provider gamma --json --no-refresh").await;
    assert_eq!(missing.code, 1);
    assert_eq!(serde_json::from_str::<Value>(&missing.stdout).unwrap()["reason"], "credentials_not_configured");
    let unknown = run(dir.path(), "auth check --provider nowhere").await;
    assert_eq!((unknown.code, unknown.stdout.as_str()), (1, "not_ready\n"));
    let invalid = run(dir.path(), "auth check --model no-such-model").await;
    assert_eq!((invalid.code, invalid.stdout.as_str()), (2, "invalid\n"));
    let usage = run(dir.path(), "auth check").await;
    assert_eq!(usage.code, 2);
    assert!(usage.stderr.contains("Auth checks require --provider"));
    let extra = run(dir.path(), "auth check --provider beta hello").await;
    assert!(extra.stderr.contains("only accept --provider and --model"));
    let flag = run(dir.path(), "auth check --provider beta --bogus").await;
    assert_eq!(flag.code, 1);
    assert!(flag.stderr.starts_with("Unknown option --bogus for \"auth check\"."));
}

#[tokio::test]
async fn prints_credentials_for_one_matching_provider() {
    let dir = agent_dir();
    let alpha = run(dir.path(), "auth print-api-key --provider alpha").await;
    assert_eq!((alpha.code, alpha.stdout.as_str()), (0, "alpha-key\n"), "{alpha:?}");
    let by_model = run(dir.path(), "auth print-api-key --model shared-model").await;
    assert_eq!(by_model.stdout, "beta-key\n", "only providers with stored credentials take part: {by_model:?}");
    let missing = run(dir.path(), "auth print-api-key --model gamma-only").await;
    assert_eq!(missing.code, 1);
    assert!(missing.stderr.contains("Model \"gamma-only\" not found"));
    let unknown = run(dir.path(), "auth print-api-key --provider nowhere").await;
    assert!(unknown.stderr.contains("Unknown provider \"nowhere\""));
    let bearer = run(dir.path(), "auth print-bearer-token --provider beta").await;
    assert_eq!(bearer.code, 1);
    assert!(bearer.stderr.contains("Provider \"beta\" is not configured with an OAuth bearer token"));
}
