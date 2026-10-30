//! Marketplace installs skip archive entries that are symlinks or escape the staging dir.

use std::path::Path;

use serde_json::{Value, json};

use super::marketplace_index::{host_target, sha256_hex};
use super::marketplace_test_support::{BINARY, MockMarketplace};

enum Entry<'a> {
    File(&'a str, &'a str),
    Symlink(&'a str, &'a str),
}

/// Raw header names so `../` and absolute paths reach the archive unchanged.
fn malicious_archive(entries: &[Entry]) -> Vec<u8> {
    let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    let mut builder = tar::Builder::new(encoder);
    for entry in entries {
        let mut header = tar::Header::new_gnu();
        header.set_mode(0o755);
        let (name, data) = match entry {
            Entry::File(name, content) => {
                header.set_entry_type(tar::EntryType::Regular);
                header.set_size(content.len() as u64);
                (name, content.as_bytes())
            }
            Entry::Symlink(name, target) => {
                header.set_entry_type(tar::EntryType::Symlink);
                header.set_size(0);
                header.as_gnu_mut().unwrap().linkname[..target.len()].copy_from_slice(target.as_bytes());
                (name, &[][..])
            }
        };
        header.as_gnu_mut().unwrap().name[..name.len()].copy_from_slice(name.as_bytes());
        header.set_cksum();
        builder.append(&header, data).unwrap();
    }
    builder.into_inner().unwrap().finish().unwrap()
}

fn publish_archive(mock: &MockMarketplace, tag: &str, archive: Vec<u8>) {
    let mut index: Value = mock.release(tag, &["todo"], "unused");
    let target = host_target().unwrap();
    let path = MockMarketplace::archive_path(tag);
    index["binaries"][BINARY][target] = json!({
        "url": format!("{}{path}", mock.server.url), "sha256": sha256_hex(&archive), "size": archive.len(),
    });
    mock.put(&path, archive);
    mock.publish(&index, true);
}

fn walk(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        out.push(path.clone());
        if std::fs::symlink_metadata(&path).unwrap().is_dir() {
            walk(&path, out);
        }
    }
}

#[tokio::test]
async fn symlinks_parent_and_absolute_entries_write_nothing_outside() {
    let root = tempfile::tempdir().unwrap();
    let (agent, outside) = (root.path().join("agent"), root.path().join("outside"));
    std::fs::create_dir_all(&outside).unwrap();
    let folder = format!("{BINARY}-{}", host_target().unwrap());
    let (link, through_link, binary) =
        (format!("{folder}/link"), format!("{folder}/link/evil"), format!("{folder}/{BINARY}"));
    let absolute = outside.join("absolute").display().to_string();
    let outside_text = outside.display().to_string();
    let archive = malicious_archive(&[
        Entry::Symlink(&link, &outside_text),
        Entry::File(&through_link, "evil"),
        Entry::File("../escape", "escape"),
        Entry::File(&format!("{folder}/../../../../../../escape"), "escape"),
        Entry::File(&absolute, "absolute"),
        Entry::File(&binary, "binary"),
    ]);
    let mock = MockMarketplace::start().await;
    publish_archive(&mock, "v1.0.0", archive);
    let marketplace = mock.marketplace(&agent, false);
    marketplace.install("todo", None).await.unwrap();
    let installed = marketplace.installed_package("todo").unwrap().unwrap();
    assert_eq!(std::fs::read_to_string(&installed.binary_path).unwrap(), "binary");

    assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0);
    let mut paths = Vec::new();
    walk(root.path(), &mut paths);
    for path in &paths {
        assert!(!std::fs::symlink_metadata(path).unwrap().is_symlink(), "{} is a symlink", path.display());
        let name = path.file_name().unwrap().to_string_lossy();
        assert!(!matches!(name.as_ref(), "escape" | "evil" | "absolute" | "link"), "{} was written", path.display());
    }
}

#[tokio::test]
async fn a_symlinked_binary_is_rejected() {
    let root = tempfile::tempdir().unwrap();
    let agent = root.path().join("agent");
    let secret = root.path().join("secret");
    std::fs::write(&secret, "secret").unwrap();
    let folder = format!("{BINARY}-{}", host_target().unwrap());
    let secret_text = secret.display().to_string();
    let archive = malicious_archive(&[Entry::Symlink(&format!("{folder}/{BINARY}"), &secret_text)]);
    let mock = MockMarketplace::start().await;
    publish_archive(&mock, "v1.0.0", archive);
    let marketplace = mock.marketplace(&agent, false);
    let error = marketplace.install("todo", None).await.unwrap_err();
    assert!(format!("{error:#}").contains("has no"), "{error:#}");
    assert!(marketplace.installed_package("todo").unwrap().is_none());
    assert_eq!(std::fs::read_to_string(&secret).unwrap(), "secret");
    let mut paths = Vec::new();
    walk(&agent, &mut paths);
    assert!(paths.iter().all(|path| !std::fs::symlink_metadata(path).unwrap().is_symlink()));
}
