//! Checksum, extraction, and replacing the running executable. On Windows the running file is
//! renamed into a quarantine directory first and removed on the next start (upstream
//! `utils/windows-self-update.ts`).

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use super::release::{ArchiveKind, UpdateError};
use crate::diagnostics::zip::read_zip_archive;

pub const QUARANTINE_DIR_NAME: &str = ".pi-update-quarantine";

/// `name -> lowercase hex digest` from `sha256sum` output (`<hash>  <name>` or `<hash> *<name>`).
pub fn parse_sha256sums(text: &str) -> BTreeMap<String, String> {
    text.lines()
        .filter_map(|line| {
            let (hash, name) = line.trim().split_once(char::is_whitespace)?;
            let name = name.trim_start().trim_start_matches('*');
            let valid = hash.len() == 64 && hash.chars().all(|ch| ch.is_ascii_hexdigit());
            (valid && !name.is_empty()).then(|| (name.to_string(), hash.to_ascii_lowercase()))
        })
        .collect()
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, bytes);
    digest.as_ref().iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Make sure `bytes` match the `name` line of `sums`.
pub fn verify_checksum(sums: &str, name: &str, bytes: &[u8]) -> Result<(), UpdateError> {
    let expected = parse_sha256sums(sums);
    let Some(expected) = expected.get(name) else {
        return Err(UpdateError::Release(format!("SHA256SUMS has no entry for {name}")));
    };
    let actual = sha256_hex(bytes);
    if &actual != expected {
        return Err(UpdateError::Release(format!("Checksum mismatch for {name}: expected {expected}, got {actual}")));
    }
    Ok(())
}

fn file_name_of(path: &str) -> &str {
    path.trim_end_matches('/').rsplit(['/', '\\']).next().unwrap_or(path)
}

/// The `binary` file from a release archive (`pi-<tag>-<target>/<binary>`).
pub fn extract_binary(archive: &[u8], kind: ArchiveKind, binary: &str) -> Result<Vec<u8>, UpdateError> {
    let missing = || UpdateError::Release(format!("The archive does not contain {binary}"));
    match kind {
        ArchiveKind::Zip => {
            let entries = read_zip_archive(archive).map_err(UpdateError::Release)?;
            let entry = entries.into_iter().find(|entry| file_name_of(&entry.name) == binary).ok_or_else(missing)?;
            Ok(entry.data)
        }
        ArchiveKind::TarGz => {
            let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(archive));
            let invalid = |error: std::io::Error| UpdateError::Release(format!("Invalid archive: {error}"));
            for entry in tar.entries().map_err(invalid)? {
                let mut entry = entry.map_err(invalid)?;
                let path = entry.path().map_err(invalid)?.to_string_lossy().into_owned();
                if entry.header().entry_type().is_file() && file_name_of(&path) == binary {
                    let mut data = Vec::new();
                    entry.read_to_end(&mut data).map_err(invalid)?;
                    return Ok(data);
                }
            }
            Err(missing())
        }
    }
}

fn sibling(executable: &Path, suffix: &str) -> PathBuf {
    let name = executable.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_else(|| "pi".into());
    executable.with_file_name(format!(".{name}.{suffix}-{}", std::process::id()))
}

fn write_executable(path: &Path, data: &[u8], like: &Path) -> std::io::Result<()> {
    std::fs::write(path, data)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(like).map(|metadata| metadata.permissions().mode()).unwrap_or(0o755) | 0o111;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    }
    #[cfg(not(unix))]
    let _ = like;
    Ok(())
}

/// Write the new binary next to `executable` and rename it over the old one.
pub fn replace_by_rename(executable: &Path, data: &[u8]) -> Result<(), UpdateError> {
    let staged = sibling(executable, "update");
    write_executable(&staged, data, executable)?;
    std::fs::rename(&staged, executable).map_err(|error| {
        let _ = std::fs::remove_file(&staged);
        UpdateError::Io(format!("Could not replace {}: {error}", executable.display()))
    })
}

/// Windows cannot overwrite a running executable but can rename it: move it into the
/// quarantine directory, then write the new binary in its place.
pub fn replace_by_quarantine(executable: &Path, data: &[u8]) -> Result<PathBuf, UpdateError> {
    let directory = executable.parent().unwrap_or(Path::new(".")).join(QUARANTINE_DIR_NAME);
    std::fs::create_dir_all(&directory)?;
    let name = executable.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
    let quarantined = directory.join(format!("{}-{}-{name}", pi_ai::now_ms(), std::process::id()));
    std::fs::rename(executable, &quarantined)
        .map_err(|error| UpdateError::Io(format!("Could not move {} aside: {error}", executable.display())))?;
    if let Err(error) = write_executable(executable, data, &quarantined) {
        let _ = std::fs::remove_file(executable);
        let _ = std::fs::rename(&quarantined, executable);
        return Err(UpdateError::Io(format!("Could not write {}: {error}", executable.display())));
    }
    Ok(quarantined)
}

/// Replace `executable` with `data` the way this platform allows.
pub fn replace_executable(executable: &Path, data: &[u8]) -> Result<(), UpdateError> {
    if cfg!(windows) { replace_by_quarantine(executable, data).map(drop) } else { replace_by_rename(executable, data) }
}

/// Remove executables an earlier update moved aside. A file still in use stays for next time.
pub fn cleanup_update_quarantine(executable: &Path) {
    let directory = executable.parent().unwrap_or(Path::new(".")).join(QUARANTINE_DIR_NAME);
    let _ = std::fs::remove_dir_all(directory);
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InstallMethod {
    /// A release archive; `pi update` replaces the binary.
    Release,
    /// `cargo install`; updated by running cargo again.
    Cargo,
}

/// `cargo install` puts binaries in `$CARGO_HOME/bin`, `~/.cargo/bin` by default.
pub fn detect_install_method(executable: &Path, cargo_home: Option<&Path>, home: &Path) -> InstallMethod {
    let cargo_bin = cargo_home.map(Path::to_path_buf).unwrap_or_else(|| home.join(".cargo")).join("bin");
    let same = |a: &Path, b: &Path| match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    };
    match executable.parent() {
        Some(directory) if same(directory, &cargo_bin) => InstallMethod::Cargo,
        _ => InstallMethod::Release,
    }
}

pub const CARGO_UPDATE_COMMAND: &str =
    "cargo install --locked --git https://github.com/yannelli/pi-rust pi-coding-agent";
