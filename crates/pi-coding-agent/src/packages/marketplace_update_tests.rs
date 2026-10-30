//! Marketplace updates compare release tags as semver.

use std::cmp::Ordering;

use super::marketplace::{UpdateOutcome, compare_tags, is_newer_tag};
use super::marketplace_test_support::MockMarketplace;

#[test]
fn tags_compare_as_semver_without_the_leading_v() {
    assert_eq!(compare_tags("v0.10.0", "v0.9.0"), Some(Ordering::Greater));
    assert_eq!(compare_tags("0.1.0", "v0.1.0"), Some(Ordering::Equal));
    assert_eq!(compare_tags("v0.1.0", "v0.2.0"), Some(Ordering::Less));
    assert_eq!(compare_tags("latest", "v0.1.0"), None);
    assert!(!is_newer_tag("v2", "v1.0.0"));
    assert!(!is_newer_tag("v0.1.0", "v0.1.0"));
}

#[tokio::test]
async fn update_refuses_an_older_release_and_skips_an_equal_one() {
    let mock = MockMarketplace::start().await;
    mock.publish(&mock.release("v0.2.0", &["todo"], "binary-v2"), true);
    let dir = tempfile::tempdir().unwrap();
    let marketplace = mock.marketplace(dir.path(), false);
    let path = marketplace.install("todo", None).await.unwrap();

    assert!(!marketplace.has_update("todo").await.unwrap());
    assert_eq!(marketplace.update("todo").await.unwrap(), UpdateOutcome::UpToDate(path.clone()));

    mock.publish(&mock.release("v0.1.0", &["todo"], "binary-v1"), true);
    assert!(!marketplace.has_update("todo").await.unwrap());
    let error = marketplace.update("todo").await.unwrap_err();
    assert!(error.to_string().contains("older than the installed v0.2.0"), "{error}");
    let installed = marketplace.installed_package("todo").unwrap().unwrap();
    assert_eq!(std::fs::read_to_string(&installed.binary_path).unwrap(), "binary-v2");

    mock.publish(&mock.release("not-semver", &["todo"], "binary-x"), true);
    assert!(!marketplace.has_update("todo").await.unwrap());
    assert_eq!(marketplace.update("todo").await.unwrap(), UpdateOutcome::UpToDate(path.clone()));

    mock.publish(&mock.release("v0.10.0", &["todo"], "binary-v10"), true);
    assert!(marketplace.has_update("todo").await.unwrap());
    assert_eq!(marketplace.update("todo").await.unwrap(), UpdateOutcome::Updated(path));
    assert_eq!(marketplace.installed_tag("todo").unwrap().as_deref(), Some("v0.10.0"));
}

#[tokio::test]
async fn update_reports_pinned_and_offline_packages() {
    let mock = MockMarketplace::start().await;
    mock.publish(&mock.release("v0.1.0", &["todo"], "binary-v1"), true);
    let dir = tempfile::tempdir().unwrap();
    let path = mock.marketplace(dir.path(), false).install("todo", Some("v0.1.0")).await.unwrap();
    mock.publish(&mock.release("v0.2.0", &["todo"], "binary-v2"), true);
    let pinned = mock.marketplace(dir.path(), false).update("todo").await.unwrap();
    assert_eq!(pinned, UpdateOutcome::Pinned { tag: "v0.1.0".into(), path });
    let error = mock.marketplace(dir.path(), true).update("todo").await.unwrap_err();
    assert_eq!(error.to_string(), "Cannot update marketplace extension todo in offline mode");
}
