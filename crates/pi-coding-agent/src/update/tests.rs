//! Self-update against a local GitHub release mock.

use std::io::Write;
use std::path::Path;

use flate2::Compression;
use flate2::write::GzEncoder;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use super::install::{replace_by_quarantine, replace_by_rename};
use super::*;
use crate::diagnostics::zip::{ZipEntry, create_zip_archive};
use crate::platform::test_http::{TestReply, TestServer, client};

const TARGET: &str = "x86_64-unknown-linux-musl";

fn tar_gz(files: &[(&str, &[u8])]) -> Vec<u8> {
    let mut builder = tar::Builder::new(GzEncoder::new(Vec::new(), Compression::default()));
    for (path, data) in files {
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        builder.append_data(&mut header, path, *data).unwrap();
    }
    builder.into_inner().unwrap().finish().unwrap()
}

/// A release server for `tag` whose archive holds `binary`; `corrupt` breaks the checksum.
async fn release_server(tag: &'static str, binary: &'static [u8], corrupt: bool) -> TestServer {
    let (name, _) = archive_name(tag, TARGET);
    let archive =
        tar_gz(&[(&format!("pi-{tag}-{TARGET}/README.md"), b"readme"), (&format!("pi-{tag}-{TARGET}/pi"), binary)]);
    let digest = if corrupt { "0".repeat(64) } else { sha256_hex(&archive) };
    let sums = format!("{}  other.zip\n{digest}  {name}\n", "1".repeat(64));
    let url = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let base = url.clone();
    let server = TestServer::start(move |request| {
        let base = base.lock().unwrap().clone();
        match request.path() {
            "/repos/yannelli/pi-rust/releases/latest" => {
                assert!(request.header("user-agent").unwrap().starts_with("pi/"));
                assert_eq!(request.header("accept"), Some("application/vnd.github+json"));
                TestReply::json(&json!({"tag_name": tag, "prerelease": false, "body": "## Notes", "assets": [
                    {"name": name, "browser_download_url": format!("{base}/download/{name}"), "size": archive.len()},
                    {"name": "SHA256SUMS", "browser_download_url": format!("{base}/download/SHA256SUMS"), "size": 10},
                ]}))
            }
            path if path.ends_with("/SHA256SUMS") => TestReply::status(200, &sums),
            path if path.starts_with("/download/") => TestReply::bytes(200, archive.clone()),
            _ => TestReply::status(404, "{}"),
        }
    })
    .await;
    *url.lock().unwrap() = server.url.clone();
    server
}

fn options(server: &TestServer, executable: &Path, current: &str) -> SelfUpdateOptions {
    SelfUpdateOptions {
        source: ReleaseSource { api_url: server.url.clone(), repo: GITHUB_REPO.into(), client: client(), token: None },
        current_version: current.into(),
        force: false,
        executable: executable.to_path_buf(),
        target: Some(TARGET.into()),
        cargo_home: Some(executable.parent().unwrap().join("cargo")),
        home: executable.parent().unwrap().to_path_buf(),
        cancel: CancellationToken::new(),
    }
}

fn installed(dir: &Path) -> std::path::PathBuf {
    let executable = dir.join("pi");
    std::fs::write(&executable, b"old binary").unwrap();
    executable
}

#[tokio::test]
async fn updates_a_release_install_after_checking_the_checksum() {
    let server = release_server("v0.3.0", b"new binary", false).await;
    let dir = tempfile::tempdir().unwrap();
    let executable = installed(dir.path());
    let outcome = run_self_update(&options(&server, &executable, "0.2.0")).await.unwrap();
    assert_eq!(outcome.message(), "Updated pi from 0.2.0 to 0.3.0");
    let SelfUpdateOutcome::Updated { notes, .. } = &outcome else { panic!("{outcome:?}") };
    assert_eq!(notes.as_deref(), Some("## Notes"));
    assert_eq!(std::fs::read(&executable).unwrap(), b"new binary");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_ne!(std::fs::metadata(&executable).unwrap().permissions().mode() & 0o111, 0);
    }
    let leftovers: Vec<_> = std::fs::read_dir(dir.path()).unwrap().map(|entry| entry.unwrap().file_name()).collect();
    assert_eq!(leftovers.len(), 1, "{leftovers:?}");
}

#[tokio::test]
async fn up_to_date_installs_are_left_alone_unless_forced() {
    let server = release_server("v0.3.0", b"new binary", false).await;
    let dir = tempfile::tempdir().unwrap();
    let executable = installed(dir.path());
    let outcome = run_self_update(&options(&server, &executable, "0.3.0")).await.unwrap();
    assert_eq!(outcome.message(), "pi is already up to date (v0.3.0)");
    assert_eq!(std::fs::read(&executable).unwrap(), b"old binary");
    let forced = SelfUpdateOptions { force: true, ..options(&server, &executable, "0.3.0") };
    run_self_update(&forced).await.unwrap();
    assert_eq!(std::fs::read(&executable).unwrap(), b"new binary");
}

#[tokio::test]
async fn checksum_mismatches_keep_the_old_binary() {
    let server = release_server("v0.3.0", b"new binary", true).await;
    let dir = tempfile::tempdir().unwrap();
    let executable = installed(dir.path());
    let error = run_self_update(&options(&server, &executable, "0.2.0")).await.unwrap_err();
    assert!(
        error.to_string().starts_with("Checksum mismatch for pi-v0.3.0-x86_64-unknown-linux-musl.tar.gz"),
        "{error}"
    );
    assert_eq!(std::fs::read(&executable).unwrap(), b"old binary");
}

#[tokio::test]
async fn cargo_installs_and_unknown_targets_are_refused() {
    let server = release_server("v0.3.0", b"new binary", false).await;
    let dir = tempfile::tempdir().unwrap();
    let cargo_bin = dir.path().join(".cargo").join("bin");
    std::fs::create_dir_all(&cargo_bin).unwrap();
    let executable = installed(&cargo_bin);
    let mut cargo = options(&server, &executable, "0.2.0");
    cargo.cargo_home = None;
    cargo.home = dir.path().to_path_buf();
    let error = run_self_update(&cargo).await.unwrap_err();
    assert_eq!(error.to_string(), format!("pi was installed with cargo. Update it with: {CARGO_UPDATE_COMMAND}"));
    let other = installed(dir.path());
    let unknown = SelfUpdateOptions { target: None, ..options(&server, &other, "0.2.0") };
    assert!(matches!(run_self_update(&unknown).await, Err(UpdateError::Unsupported(_))));
    assert!(server.requests().is_empty());
}

#[test]
fn archives_and_assets_follow_the_release_workflow_names() {
    assert_eq!(
        archive_name("v1.2.0", TARGET),
        ("pi-v1.2.0-x86_64-unknown-linux-musl.tar.gz".into(), ArchiveKind::TarGz)
    );
    assert_eq!(archive_name("v1.2.0", "aarch64-pc-windows-msvc").1, ArchiveKind::Zip);
    assert_eq!(binary_name("x86_64-pc-windows-msvc"), "pi.exe");
    let release = GitHubRelease::from_json(&json!({"tag_name": "v1.2.0", "assets": [
        {"name": "SHA256SUMS", "browser_download_url": "https://x/SHA256SUMS"}]}))
    .unwrap();
    assert_eq!(release.version(), "1.2.0");
    let error = select_assets(&release, TARGET).unwrap_err();
    assert_eq!(
        error.to_string(),
        "Release v1.2.0 has no archive for x86_64-unknown-linux-musl (pi-v1.2.0-x86_64-unknown-linux-musl.tar.gz)"
    );
    assert!(release_target().is_some());
}

#[test]
fn checksums_parse_both_sha256sum_formats() {
    let hash = sha256_hex(b"abc");
    assert_eq!(hash, "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    let sums = parse_sha256sums(&format!("{}  a.tar.gz\n{} *b.zip\nnot a line\n", hash.to_uppercase(), "f".repeat(64)));
    assert_eq!(sums["a.tar.gz"], hash);
    assert!(sums.contains_key("b.zip") && sums.len() == 2);
    verify_checksum(&format!("{hash}  a\n"), "a", b"abc").unwrap();
    assert_eq!(verify_checksum("", "a", b"abc").unwrap_err().to_string(), "SHA256SUMS has no entry for a");
}

#[test]
fn binaries_come_out_of_zip_and_tar_archives() {
    let zip =
        create_zip_archive(&[ZipEntry::new("pi-v1-x/README.md", "r"), ZipEntry::new("pi-v1-x/pi.exe", "exe")]).unwrap();
    assert_eq!(extract_binary(&zip, ArchiveKind::Zip, "pi.exe").unwrap(), b"exe");
    let tar = tar_gz(&[("pi-v1-x/pi", b"elf")]);
    assert_eq!(extract_binary(&tar, ArchiveKind::TarGz, "pi").unwrap(), b"elf");
    assert!(extract_binary(&tar, ArchiveKind::TarGz, "pi.exe").is_err());
    let mut gz = GzEncoder::new(Vec::new(), Compression::default());
    gz.write_all(b"not a tar").unwrap();
    assert!(extract_binary(&gz.finish().unwrap(), ArchiveKind::TarGz, "pi").is_err());
}

#[test]
fn windows_style_replacement_moves_the_running_binary_aside() {
    let dir = tempfile::tempdir().unwrap();
    let executable = dir.path().join("pi.exe");
    std::fs::write(&executable, b"running").unwrap();
    let quarantined = replace_by_quarantine(&executable, b"fresh").unwrap();
    assert_eq!(std::fs::read(&executable).unwrap(), b"fresh");
    assert_eq!(std::fs::read(&quarantined).unwrap(), b"running");
    assert!(quarantined.starts_with(dir.path().join(QUARANTINE_DIR_NAME)));
    cleanup_update_quarantine(&executable);
    assert!(!dir.path().join(QUARANTINE_DIR_NAME).exists());
    cleanup_update_quarantine(&executable);

    replace_by_rename(&executable, b"renamed").unwrap();
    assert_eq!(std::fs::read(&executable).unwrap(), b"renamed");
    assert_eq!(detect_install_method(&executable, None, dir.path()), InstallMethod::Release);
}
