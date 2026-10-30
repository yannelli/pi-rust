//! Installs Google's Antigravity ACP server: the pinned release archive from `dl.google.com`,
//! checked by size and SHA-256, unpacked into `~/.local/opt/agy-acp`, where [`super::process::locate`] looks.

use std::path::{Path, PathBuf};

use futures::StreamExt;
use ring::digest::{Context, SHA256};
use tokio::io::AsyncWriteExt;
use tokio_util::sync::CancellationToken;

pub(crate) struct Release {
    pub version: &'static str,
    pub url: &'static str,
    pub(crate) sha256: &'static str,
    pub bytes: u64,
    pub(crate) binary: (&'static str, u64),
    pub(crate) harness: (&'static str, u64),
}

const VERSION: &str = "1.4.3";

macro_rules! release {
    ($path:literal, $sha:literal, $bytes:literal, $binary:literal, $binary_bytes:literal, $harness:literal, $harness_bytes:literal) => {
        Release {
            version: VERSION,
            url: concat!("https://dl.google.com/agy-extensions/releases/", $path),
            sha256: $sha,
            bytes: $bytes,
            binary: ($binary, $binary_bytes),
            harness: ($harness, $harness_bytes),
        }
    };
}

/// Google's release 1.4.3 for this platform, from the signed runtime manifest of the reference provider.
pub(crate) fn release() -> Option<Release> {
    Some(match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => release!(
            "macos/agy-acp-server-1.4.3-darwin-arm64.zip",
            "c57db0a9b8b712daa9c3c17cdb990a61842ea569f279e8bd79d7aa85a303b20b",
            114147623,
            "agy_acp_server.par",
            282446160,
            "localharness_external",
            122957936
        ),
        ("macos", "x86_64") => release!(
            "macos/agy-acp-server-1.4.3-darwin-x86_64.zip",
            "9392dbbdad7f0663dbfb5a0bf641297b0cf80d2ee9cd2d49a616bbbc7378731d",
            119801100,
            "agy_acp_server.par",
            286653712,
            "localharness_external",
            127612208
        ),
        ("linux", "x86_64") => release!(
            "linux/agy-acp-server-1.4.3-linux-x86_64.zip",
            "0634ce5306ba92c932945455e7a83fda78e2d99c33fa8aa6ee0566206a382714",
            336846435,
            "agy_acp_server.par",
            927707135,
            "localharness_external",
            133926136
        ),
        ("linux", "aarch64") => release!(
            "linux/agy-acp-server-1.4.3-linux-arm64.zip",
            "05c153443b0b1175151c760eeddea538950bc9843e317d54786a1f03670182bc",
            322887047,
            "agy_acp_server.par",
            932662106,
            "localharness_external",
            127512832
        ),
        ("windows", "x86_64") => release!(
            "windows/agy-acp-server-1.4.3-windows-x86_64.zip",
            "54c2e16378d9a4952e1da70e355f3c33c4f284308d6de48ab3e6e34a127bd325",
            131267135,
            "agy_acp_server.exe",
            87022872,
            "localharness_external.exe",
            147843736
        ),
        ("windows", "aarch64") => release!(
            "windows/agy-acp-server-1.4.3-windows-arm64.zip",
            "fcd46431ad33d3b510ce0b98fe7b43d095bd8b3f1c363ee2d57faa7ee981c7bb",
            134301870,
            "agy_acp_server.exe",
            94238816,
            "localharness_external.exe",
            139322008
        ),
        _ => return None,
    })
}

pub(crate) fn root() -> Result<PathBuf, String> {
    let home =
        std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).ok_or("Cannot find the home directory")?;
    Ok(PathBuf::from(home).join(".local/opt/agy-acp"))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

async fn download(
    release: &Release,
    to: &Path,
    progress: &(dyn Fn(String) + Send + Sync),
    cancel: &CancellationToken,
) -> Result<(), String> {
    let response = crate::registry::http_client()
        .get(release.url)
        .send()
        .await
        .map_err(|error| format!("Download failed: {error}"))?;
    if !response.status().is_success() {
        return Err(format!("Download failed: HTTP {} from {}", response.status().as_u16(), release.url));
    }
    let mut file =
        tokio::fs::File::create(to).await.map_err(|error| format!("Cannot write {}: {error}", to.display()))?;
    let (mut hash, mut written, mut reported) = (Context::new(&SHA256), 0u64, 0u64);
    let mut body = response.bytes_stream();
    loop {
        let chunk = tokio::select! {
            () = cancel.cancelled() => return Err("Install cancelled".into()),
            chunk = body.next() => chunk,
        };
        let Some(chunk) = chunk else { break };
        let chunk = chunk.map_err(|error| format!("Download failed: {error}"))?;
        hash.update(&chunk);
        file.write_all(&chunk).await.map_err(|error| format!("Cannot write {}: {error}", to.display()))?;
        written += chunk.len() as u64;
        let percent = written * 100 / release.bytes.max(1);
        if percent >= reported + 10 {
            reported = percent - percent % 10;
            progress(format!("Downloading the Antigravity ACP server: {reported}%"));
        }
    }
    file.flush().await.map_err(|error| error.to_string())?;
    if written != release.bytes || hex(hash.finish().as_ref()) != release.sha256 {
        return Err(
            "The downloaded Antigravity ACP server does not match the pinned SHA-256; pi did not install it".into()
        );
    }
    Ok(())
}

/// Unpacks with the system `unzip`, else `tar` (bsdtar reads zip on macOS and Windows).
async fn unpack(archive: &Path, into: &Path) -> Result<(), String> {
    let unzip = tokio::process::Command::new("unzip").arg("-qo").arg(archive).arg("-d").arg(into).status().await;
    if unzip.is_ok_and(|status| status.success()) {
        return Ok(());
    }
    let tar = tokio::process::Command::new("tar").arg("-xf").arg(archive).arg("-C").arg(into).status().await;
    match tar {
        Ok(status) if status.success() => Ok(()),
        _ => Err("Cannot unpack the Antigravity ACP server: install unzip and try again".into()),
    }
}

fn check(directory: &Path, release: &Release) -> Result<(), String> {
    let mut names: Vec<String> = std::fs::read_dir(directory)
        .map_err(|error| error.to_string())?
        .filter_map(|entry| entry.ok().map(|entry| entry.file_name().to_string_lossy().into_owned()))
        .collect();
    names.sort();
    let mut expected = vec![release.binary.0.to_string(), release.harness.0.to_string()];
    expected.sort();
    let sizes_match = [release.binary, release.harness]
        .iter()
        .all(|(name, bytes)| std::fs::metadata(directory.join(name)).is_ok_and(|metadata| metadata.len() == *bytes));
    if names != expected || !sizes_match {
        return Err("The Antigravity ACP archive holds unexpected files; pi did not install it".into());
    }
    #[cfg(unix)]
    for (name, _) in [release.binary, release.harness] {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(directory.join(name), std::fs::Permissions::from_mode(0o755))
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

/// Downloads, checks, and unpacks `release` under `root`, then points `current` at it. Returns the
/// server path.
pub(crate) async fn install(
    root: &Path,
    release: &Release,
    progress: &(dyn Fn(String) + Send + Sync),
    cancel: &CancellationToken,
) -> Result<PathBuf, String> {
    let versions = root.join("versions");
    std::fs::create_dir_all(&versions).map_err(|error| format!("Cannot create {}: {error}", versions.display()))?;
    let target = if cfg!(windows) { root.join("current") } else { versions.join(release.sha256) };
    if check(&target, release).is_err() {
        let archive = versions.join(format!("{}.zip.part", release.sha256));
        let staging = versions.join(format!("{}.staging", release.sha256));
        let _ = std::fs::remove_dir_all(&staging);
        std::fs::create_dir_all(&staging).map_err(|error| error.to_string())?;
        let result = async {
            download(release, &archive, progress, cancel).await?;
            progress("Unpacking the Antigravity ACP server".into());
            unpack(&archive, &staging).await?;
            check(&staging, release)?;
            let _ = std::fs::remove_dir_all(&target);
            std::fs::rename(&staging, &target).map_err(|error| error.to_string())
        }
        .await;
        let _ = std::fs::remove_file(&archive);
        let _ = std::fs::remove_dir_all(&staging);
        result?;
    }
    #[cfg(unix)]
    {
        let current = root.join("current");
        let _ = std::fs::remove_file(&current);
        std::os::unix::fs::symlink(&target, &current)
            .map_err(|error| format!("Cannot link {}: {error}", current.display()))?;
    }
    Ok(root.join("current").join(release.binary.0))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::providers::http_mock::{Reply, RouteServer};

    fn leak(text: String) -> &'static str {
        Box::leak(text.into_boxed_str())
    }

    /// A zip of the two runtime files made with the system `zip`, or `None` without it.
    fn archive(dir: &Path) -> Option<Vec<u8>> {
        std::fs::write(dir.join("agy_acp_server.par"), b"#!/bin/sh\n").unwrap();
        std::fs::write(dir.join("localharness_external"), b"harness").unwrap();
        let status = std::process::Command::new("zip")
            .current_dir(dir)
            .args(["-q", "runtime.zip", "agy_acp_server.par", "localharness_external"])
            .status()
            .ok()?;
        status.success().then(|| std::fs::read(dir.join("runtime.zip")).unwrap())
    }

    fn release_for(url: &str, bytes: &[u8], sha256: String) -> Release {
        Release {
            version: "test",
            url: leak(url.to_string()),
            sha256: leak(sha256),
            bytes: bytes.len() as u64,
            binary: ("agy_acp_server.par", 10),
            harness: ("localharness_external", 7),
        }
    }

    #[tokio::test]
    async fn installs_a_checked_archive_and_rejects_a_changed_one() {
        let source = tempfile::tempdir().unwrap();
        let Some(zip) = archive(source.path()) else { return };
        let body = zip.clone();
        let server = RouteServer::start(move |_| Reply::new(200, "application/zip", body.clone())).await;
        let sha = hex(ring::digest::digest(&SHA256, &zip).as_ref());
        let root = tempfile::tempdir().unwrap();
        let cancel = CancellationToken::new();
        let release = release_for(&format!("{}/runtime.zip", server.base_url), &zip, sha.clone());
        let path = install(root.path(), &release, &|_| {}, &cancel).await.unwrap();
        assert_eq!(path, root.path().join("current/agy_acp_server.par"));
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o755);

        let other = tempfile::tempdir().unwrap();
        let wrong = release_for(&format!("{}/runtime.zip", server.base_url), &zip, "00".repeat(32));
        let error = install(other.path(), &wrong, &|_| {}, &cancel).await.unwrap_err();
        assert!(error.contains("SHA-256"), "{error}");
        assert!(!other.path().join("current").exists());
    }
}
