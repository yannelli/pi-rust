use std::sync::Arc;
use std::time::Duration;

use futures::FutureExt;
use serde_json::json;

use super::*;
use crate::auth::OAuthCredential;
use crate::auth::store_lock::lock_dir;

fn set(credential: Credential) -> UpdateFn {
    Box::new(move |_| async move { Ok(Some(credential)) }.boxed())
}

fn api_key(key: &str) -> Credential {
    Credential::ApiKey(ApiKeyCredential { key: Some(key.into()), env: None })
}

fn key_of(credential: Option<Credential>) -> Option<String> {
    match credential {
        Some(Credential::ApiKey(credential)) => credential.key,
        _ => None,
    }
}

fn temp_auth(name: &str) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(name).join("auth.json");
    (dir, path)
}

#[tokio::test]
async fn reads_and_resolves_stored_credentials() {
    let (_dir, path) = temp_auth("agent");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let text = json!({
        "openai": {"type": "api_key", "key": "sk-literal"},
        "scoped": {"type": "api_key", "key": "$PI_STORE_TEST_KEY", "env": {"PI_STORE_TEST_KEY": "from-env"}},
        "cmd": {"type": "api_key", "key": "!printf from-command"},
        "anthropic": {"type": "oauth", "refresh": "r", "access": "a", "expires": 5, "scopes": ["x"]}
    });
    std::fs::write(&path, text.to_string()).unwrap();
    let store = FileCredentialStore::new(&path);
    assert_eq!(key_of(store.read("openai").await.unwrap()).as_deref(), Some("sk-literal"));
    assert_eq!(key_of(store.read("scoped").await.unwrap()).as_deref(), Some("from-env"));
    #[cfg(unix)]
    assert_eq!(key_of(store.read("cmd").await.unwrap()).as_deref(), Some("from-command"));
    let Some(Credential::OAuth(oauth)) = store.read("anthropic").await.unwrap() else { panic!("expected oauth") };
    assert_eq!((oauth.access.as_str(), oauth.extra["scopes"][0].as_str()), ("a", Some("x")));
    assert!(store.read("missing").await.unwrap().is_none());
    let mut listed: Vec<(String, AuthType)> =
        store.list().await.unwrap().into_iter().map(|info| (info.provider_id, info.auth_type)).collect();
    listed.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(listed[0], ("anthropic".to_string(), AuthType::OAuth));
    let read_only = FileCredentialStore::read_only(&path);
    assert_eq!(key_of(read_only.read("cmd").await.unwrap()).as_deref(), Some("!printf from-command"));
}

#[tokio::test]
async fn update_round_trips_and_preserves_external_edits() {
    let (_dir, path) = temp_auth("agent");
    let store = FileCredentialStore::new(&path);
    let stored = store.update("openai", set(api_key("first"))).await.unwrap();
    assert_eq!(key_of(stored).as_deref(), Some("first"));
    assert!(path.parent().unwrap().exists() && !lock_dir(&path).exists());

    let mut external: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    external["other"] = json!({"type": "api_key", "key": "external", "note": "kept"});
    external["future"] = json!({"type": "something-new", "x": 1});
    std::fs::write(&path, serde_json::to_string_pretty(&external).unwrap()).unwrap();

    let oauth = Credential::OAuth(OAuthCredential {
        refresh: "r".into(),
        access: "a".into(),
        expires: 9,
        ..Default::default()
    });
    store.update("anthropic", set(oauth)).await.unwrap();
    let unchanged = store.update("openai", Box::new(|_| async { Ok(None) }.boxed())).await.unwrap();
    assert_eq!(key_of(unchanged).as_deref(), Some("first"));
    store.delete("openai").await.unwrap();

    let saved: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert!(saved.get("openai").is_none());
    assert_eq!(saved["other"]["note"], "kept");
    assert_eq!(saved["future"]["type"], "something-new");
    assert_eq!(saved["anthropic"]["type"], "oauth");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_updates_are_serialized_across_store_instances() {
    let (_dir, path) = temp_auth("agent");
    let first = Arc::new(FileCredentialStore::new(&path));
    let second = Arc::new(FileCredentialStore::new(&path));
    let mut tasks = Vec::new();
    for index in 0..8 {
        let store = if index % 2 == 0 { first.clone() } else { second.clone() };
        tasks.push(tokio::spawn(async move {
            store
                .update(
                    "counter",
                    Box::new(|current| {
                        async move {
                            tokio::time::sleep(Duration::from_millis(5)).await;
                            let count: u32 = key_of(current).and_then(|key| key.parse().ok()).unwrap_or(0);
                            Ok(Some(api_key(&(count + 1).to_string())))
                        }
                        .boxed()
                    }),
                )
                .await
                .unwrap();
        }));
    }
    for task in tasks {
        task.await.unwrap();
    }
    assert_eq!(key_of(first.read("counter").await.unwrap()).as_deref(), Some("8"));
}

#[tokio::test]
async fn waits_for_a_briefly_held_lock_and_never_overwrites_malformed_files() {
    let (_dir, path) = temp_auth("agent");
    std::fs::create_dir_all(lock_dir(&path)).unwrap();
    let held = lock_dir(&path);
    let release = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        std::fs::remove_dir(held).unwrap();
    });
    let store = FileCredentialStore::new(&path);
    store.update("openai", set(api_key("after-lock"))).await.unwrap();
    release.await.unwrap();
    assert_eq!(key_of(store.read("openai").await.unwrap()).as_deref(), Some("after-lock"));

    std::fs::write(&path, "{not json").unwrap();
    assert!(store.update("openai", set(api_key("x"))).await.is_err());
    assert!(store.read("openai").await.is_err());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "{not json");
}

#[tokio::test]
async fn read_only_store_does_not_create_files_and_rejects_writes() {
    let (dir, path) = temp_auth("missing-agent");
    let store = FileCredentialStore::read_only(&path);
    assert!(store.read("openai").await.unwrap().is_none());
    assert!(store.update("openai", set(api_key("x"))).await.is_err());
    assert!(!dir.path().join("missing-agent").exists());
}

#[tokio::test]
async fn in_memory_and_runtime_overlay_follow_the_same_contract() {
    let memory: Arc<dyn CredentialStore> = Arc::new(InMemoryCredentialStore::default());
    memory.update("openai", set(api_key("stored"))).await.unwrap();
    let overlay = RuntimeCredentialStore::new(memory.clone());
    overlay.set_runtime_api_key("openai", "runtime");
    assert_eq!(key_of(overlay.read("openai").await.unwrap()).as_deref(), Some("runtime"));
    assert_eq!(key_of(memory.read("openai").await.unwrap()).as_deref(), Some("stored"));
    overlay.set_runtime_api_key("groq", "runtime-groq");
    let listed = overlay.list().await.unwrap();
    assert_eq!(listed.len(), 2);
    overlay.delete("openai").await.unwrap();
    assert!(overlay.read("openai").await.unwrap().is_none());
    assert!(overlay.has_runtime_api_key("groq"));
}
