//! Marketplace installs against a local signed release server.

use serde_json::{Value, json};

use super::marketplace::{MARKETPLACE_PUBLIC_KEY, Marketplace};
use super::marketplace_index::{MARKETPLACE_TARGETS, host_target, verify_signature};
use super::marketplace_test_support::MockMarketplace;
use crate::platform::test_http::client;

fn read_json(path: &std::path::Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

async fn expect_install_error(mock: &MockMarketplace, index: Value, needle: &str) {
    let dir = tempfile::tempdir().unwrap();
    mock.publish(&index, true);
    let error = mock.marketplace(dir.path(), false).install("todo", None).await.unwrap_err();
    assert!(format!("{error:#}").contains(needle), "{error:#} does not contain {needle}");
    assert!(!dir.path().join("marketplace/packages/todo").exists());
    assert!(!dir.path().join("marketplace/bin").read_dir().is_ok_and(|mut entries| entries.next().is_some()));
}

#[test]
fn embedded_key_and_wrapped_signature_decode() {
    use base64::Engine as _;
    let key = base64::engine::general_purpose::STANDARD.decode(MARKETPLACE_PUBLIC_KEY).unwrap();
    assert_eq!(key.len(), 32);
    let error = verify_signature(&key, b"{}", &[b'A'; 88]).unwrap_err();
    assert!(error.to_string().contains("signature is not valid"));
}

#[tokio::test]
async fn install_writes_native_manifest_and_shares_the_binary() {
    let mock = MockMarketplace::start().await;
    mock.publish(&mock.release("v1", &["todo", "notes"], "binary-v1"), true);
    let dir = tempfile::tempdir().unwrap();
    let marketplace = mock.marketplace(dir.path(), false);

    let path = marketplace.install("todo", None).await.unwrap();
    assert_eq!(path, marketplace.package_path("todo").unwrap());
    let manifest = read_json(&path.join("package.json"));
    assert_eq!(manifest["name"], "@pi-rust-extensions/todo");
    assert_eq!(manifest["pi"]["native"][0]["args"], json!(["todo"]));
    assert_eq!(manifest["piMarketplace"]["pinned"], false);
    let command = std::path::PathBuf::from(manifest["pi"]["native"][0]["command"].as_str().unwrap());
    assert!(command.is_absolute());
    assert_eq!(std::fs::read_to_string(&command).unwrap(), "binary-v1");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&command).unwrap().permissions().mode() & 0o777, 0o755);
    }

    marketplace.install("notes", None).await.unwrap();
    assert_eq!(mock.server.count(&MockMarketplace::archive_path("v1")), 1);
    let installed = marketplace.installed().unwrap();
    assert_eq!(installed.iter().map(|package| package.name.as_str()).collect::<Vec<_>>(), ["notes", "todo"]);
    assert!(installed.iter().all(|package| package.binary_path == command && package.tag == "v1"));
    assert_eq!(marketplace.installed_tag("todo").unwrap().as_deref(), Some("v1"));

    let catalog = marketplace.browse(None).await.unwrap();
    assert_eq!((catalog.index.tag.as_str(), catalog.installed.len()), ("v1", 2));
    assert_eq!(catalog.installed("todo").map(|package| package.version.as_str()), Some("1"));
}

#[tokio::test]
async fn install_rejects_a_bad_signature() {
    let mock = MockMarketplace::start().await;
    mock.publish(&mock.release("v1", &["todo"], "x"), true);
    mock.put("/index.json.sig", mock.signature(b"other bytes"));
    let dir = tempfile::tempdir().unwrap();
    let error = mock.marketplace(dir.path(), false).install("todo", None).await.unwrap_err();
    assert!(error.to_string().contains("signature is not valid"), "{error}");
    let unsigned = Marketplace::new(dir.path().into(), client(), false).with_base_url(&mock.server.url);
    mock.publish(&mock.release("v1", &["todo"], "x"), true);
    assert!(unsigned.fetch_index(None).await.unwrap_err().to_string().contains("signature"));
    assert!(!dir.path().join("marketplace").exists());
}

#[tokio::test]
async fn install_rejects_hash_size_and_target_mismatches() {
    let mock = MockMarketplace::start().await;
    let target = host_target().unwrap();
    let mut index = mock.release("v1", &["todo"], "binary");
    index["binaries"]["pi-rust-ext"][target]["sha256"] = json!("0".repeat(64));
    expect_install_error(&mock, index, "SHA-256").await;

    let mut index = mock.release("v1", &["todo"], "binary");
    let size = index["binaries"]["pi-rust-ext"][target]["size"].as_u64().unwrap();
    index["binaries"]["pi-rust-ext"][target]["size"] = json!(size + 1);
    expect_install_error(&mock, index, "bytes").await;

    let mut index = mock.release("v1", &["todo"], "binary");
    index["binaries"]["pi-rust-ext"][target]["size"] = json!(size - 1);
    expect_install_error(&mock, index, "larger than").await;

    let other = MARKETPLACE_TARGETS.iter().find(|candidate| **candidate != target).unwrap();
    let mut index = mock.release("v1", &["todo"], "binary");
    index["extensions"][0]["platforms"] = json!([other]);
    expect_install_error(&mock, index, "does not support").await;
}

#[tokio::test]
async fn install_rejects_schema_protocol_version_and_tag_mismatches() {
    let mock = MockMarketplace::start().await;
    let cases = [
        ("schemaVersion", json!(2), "schema 2"),
        ("protocolVersion", json!(2), "protocol 2"),
        ("minPiVersion", json!("999.0.0"), "needs pi 999.0.0"),
    ];
    for (field, value, needle) in cases {
        let mut index = mock.release("v1", &["todo"], "binary");
        index[field] = value;
        expect_install_error(&mock, index, needle).await;
    }
    let mut index = mock.release("v1", &["todo"], "binary");
    index["extensions"][0]["binary"] = json!("../evil");
    expect_install_error(&mock, index, "not valid").await;

    let dir = tempfile::tempdir().unwrap();
    let marketplace = mock.marketplace(dir.path(), false);
    let body = serde_json::to_vec(&mock.release("v2", &["todo"], "binary")).unwrap();
    mock.put("/v1/index.json.sig", mock.signature(&body));
    mock.put("/v1/index.json", body);
    let error = marketplace.install("todo", Some("v1")).await.unwrap_err();
    assert!(error.to_string().contains("tag v1 was requested"), "{error}");
    for (name, tag) in [("../todo", None), ("Todo", None), ("todo", Some("../v1")), ("todo", Some("v1/x"))] {
        assert!(marketplace.install(name, tag).await.unwrap_err().to_string().contains("is not valid"));
    }
    assert!(marketplace.package_path("a/b").is_err());
}

#[tokio::test]
async fn update_moves_unpinned_packages_and_prunes_unused_binaries() {
    let mock = MockMarketplace::start().await;
    mock.publish(&mock.release("v1", &["todo", "notes"], "binary-v1"), true);
    let dir = tempfile::tempdir().unwrap();
    let marketplace = mock.marketplace(dir.path(), false);
    marketplace.install("todo", None).await.unwrap();
    marketplace.install("notes", Some("v1")).await.unwrap();
    let v1 = marketplace.installed_package("notes").unwrap().unwrap();
    assert!(v1.pinned);
    assert!(!marketplace.has_update("todo").await.unwrap());

    mock.publish(&mock.release("v2", &["todo", "notes"], "binary-v2"), true);
    assert!(marketplace.has_update("todo").await.unwrap());
    assert!(!marketplace.has_update("notes").await.unwrap());
    marketplace.update("todo").await.unwrap();
    marketplace.update("notes").await.unwrap();
    let todo = marketplace.installed_package("todo").unwrap().unwrap();
    assert_eq!((todo.tag.as_str(), todo.pinned), ("v2", false));
    assert_eq!(std::fs::read_to_string(&todo.binary_path).unwrap(), "binary-v2");
    assert_eq!(marketplace.installed_tag("notes").unwrap().as_deref(), Some("v1"));
    assert!(v1.binary_path.is_file());

    marketplace.remove("notes").unwrap();
    assert!(!v1.binary_path.exists());
    assert!(!dir.path().join("marketplace/bin/v1").exists());
    assert!(todo.binary_path.is_file());
    marketplace.remove("todo").unwrap();
    assert_eq!(std::fs::read_dir(dir.path().join("marketplace/bin")).unwrap().count(), 0);
    assert!(marketplace.installed().unwrap().is_empty());
    marketplace.remove("todo").unwrap();
}

#[tokio::test]
async fn prune_keeps_binaries_when_a_package_state_cannot_be_read() {
    let mock = MockMarketplace::start().await;
    mock.publish(&mock.release("v1", &["todo", "notes"], "binary-v1"), true);
    let dir = tempfile::tempdir().unwrap();
    let marketplace = mock.marketplace(dir.path(), false);
    marketplace.install("todo", None).await.unwrap();
    let path = marketplace.install("notes", None).await.unwrap();
    std::fs::write(path.join("package.json"), "{ not json").unwrap();
    let binary = marketplace.installed_package("todo").unwrap().unwrap().binary_path;
    marketplace.remove("todo").unwrap();
    assert!(binary.is_file());
}

#[tokio::test]
async fn a_missing_binary_needs_a_reinstall() {
    let mock = MockMarketplace::start().await;
    mock.publish(&mock.release("v1", &["todo"], "binary-v1"), true);
    let dir = tempfile::tempdir().unwrap();
    let marketplace = mock.marketplace(dir.path(), false);
    marketplace.install("todo", None).await.unwrap();
    let binary = marketplace.installed_package("todo").unwrap().unwrap().binary_path;
    std::fs::remove_file(&binary).unwrap();
    assert!(marketplace.installed_package("todo").unwrap().is_none());
    assert!(marketplace.installed_tag("todo").unwrap().is_none());

    marketplace.install("todo", None).await.unwrap();
    assert_eq!(mock.server.count(&MockMarketplace::archive_path("v1")), 2);
    assert_eq!(std::fs::read_to_string(&binary).unwrap(), "binary-v1");
}

#[tokio::test]
async fn offline_mode_makes_no_requests() {
    let mock = MockMarketplace::start().await;
    mock.publish(&mock.release("v1", &["todo"], "binary-v1"), true);
    let dir = tempfile::tempdir().unwrap();
    let path = mock.marketplace(dir.path(), false).install("todo", None).await.unwrap();
    mock.publish(&mock.release("v2", &["todo"], "binary-v2"), true);
    let requests = mock.server.requests().len();

    let offline = mock.marketplace(dir.path(), true);
    assert!(offline.fetch_index(None).await.unwrap_err().to_string().contains("offline"));
    assert!(offline.install("notes", None).await.is_err());
    assert!(!offline.has_update("todo").await.unwrap());
    assert_eq!(offline.update("todo").await.unwrap(), path);
    assert_eq!(offline.installed_tag("todo").unwrap().as_deref(), Some("v1"));
    assert_eq!(mock.server.requests().len(), requests);
}

#[test]
fn index_urls_follow_release_and_mirror_layouts() {
    let dir = std::path::Path::new("agent");
    let default = Marketplace::new(dir.into(), client(), false);
    if std::env::var_os("PI_MARKETPLACE_URL").is_none() {
        let base = "https://github.com/yannelli/pi-rust-extensions/releases";
        assert_eq!(default.index_url(None), format!("{base}/latest/download/index.json"));
        assert_eq!(default.index_url(Some("v1")), format!("{base}/download/v1/index.json"));
    }
    assert!(default.package_path("todo").unwrap().is_absolute());
    let mirror = default.with_base_url("http://mirror/assets/");
    assert_eq!(mirror.index_url(None), "http://mirror/assets/index.json");
    assert_eq!(mirror.index_url(Some("v1")), "http://mirror/assets/v1/index.json");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_installs_share_one_binary() {
    let mock = MockMarketplace::start().await;
    mock.publish(&mock.release("v1", &["todo", "notes"], "binary-v1"), true);
    let dir = tempfile::tempdir().unwrap();
    let marketplace = std::sync::Arc::new(mock.marketplace(dir.path(), false));
    let tasks = ["todo", "notes"].map(|name| {
        let marketplace = marketplace.clone();
        tokio::spawn(async move { marketplace.install(name, None).await })
    });
    for task in tasks {
        task.await.unwrap().unwrap();
    }
    let installed = marketplace.installed().unwrap();
    assert_eq!(installed.len(), 2);
    let binary = &installed[0].binary_path;
    assert_eq!(binary, &installed[1].binary_path);
    assert_eq!(std::fs::read_to_string(binary).unwrap(), "binary-v1");
    let mut files: Vec<String> = std::fs::read_dir(binary.parent().unwrap())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    files.sort();
    let name = binary.file_name().unwrap().to_string_lossy().into_owned();
    assert_eq!(files, [name.clone(), format!("{name}.sha256")]);
}

#[tokio::test]
async fn a_failed_update_keeps_the_installed_release() {
    let mock = MockMarketplace::start().await;
    mock.publish(&mock.release("v1", &["todo"], "binary-v1"), true);
    let dir = tempfile::tempdir().unwrap();
    let marketplace = mock.marketplace(dir.path(), false);
    let path = marketplace.install("todo", None).await.unwrap();
    let manifest = std::fs::read_to_string(path.join("package.json")).unwrap();
    let binary = marketplace.installed_package("todo").unwrap().unwrap().binary_path;

    let mut index = mock.release("v2", &["todo"], "binary-v2");
    index["binaries"]["pi-rust-ext"][host_target().unwrap()]["sha256"] = json!("0".repeat(64));
    mock.publish(&index, true);
    let error = marketplace.update("todo").await.unwrap_err();
    assert!(error.to_string().contains("SHA-256"), "{error}");
    assert_eq!(std::fs::read_to_string(path.join("package.json")).unwrap(), manifest);
    assert_eq!(std::fs::read_to_string(&binary).unwrap(), "binary-v1");
    assert_eq!(marketplace.installed_tag("todo").unwrap().as_deref(), Some("v1"));
    assert!(!dir.path().join("marketplace/bin/v2").exists());
}
