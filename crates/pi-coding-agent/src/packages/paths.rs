//! Where packages are installed: `<agent dir>/{npm,git,url}` for user packages,
//! `<cwd>/.pi/{npm,git,url}` for project packages, and hashed directories under
//! `<agent dir>/tmp/extensions` for `-e` sources.

use std::path::{Path, PathBuf};

use anyhow::bail;

use super::PackageManager;
use super::git_url::GitSource;
use super::source::ArchiveSource;
use crate::resources::SourceScope;
use crate::resources::brand::CONFIG_DIR_NAME;
use crate::resources::paths::{normalize, resolve_path};

/// `<agent dir>/tmp/extensions`, created private to the user.
pub fn extension_temp_folder(agent_dir: &Path) -> std::io::Result<PathBuf> {
    let folder = agent_dir.join("tmp").join("extensions");
    std::fs::create_dir_all(&folder)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(folder)
}

/// `root` joined with `parts`, refusing results outside `root`.
pub(crate) fn resolve_managed_path(root: &Path, parts: &[&str]) -> anyhow::Result<PathBuf> {
    let root = normalize(root);
    let mut path = root.clone();
    for part in parts.iter().filter(|part| !part.is_empty()) {
        path.push(part);
    }
    let path = normalize(&path);
    if path != root && !path.starts_with(&root) {
        bail!("Refusing to use path outside package install root: {}", path.display());
    }
    Ok(path)
}

/// A `.gitignore` that keeps installed packages out of the surrounding repository.
pub(crate) fn ensure_git_ignore(dir: &Path) {
    let _ = std::fs::create_dir_all(dir);
    let path = dir.join(".gitignore");
    if !path.exists() {
        let _ = std::fs::write(path, "*\n!.gitignore\n");
    }
}

/// Mark `path` so Dropbox and the macOS file provider skip it, ignoring failures.
pub(crate) fn mark_path_ignored_by_cloud_sync(path: &Path) {
    let attributes: &[&str] = if cfg!(target_os = "macos") {
        &["com.dropbox.ignored", "com.apple.fileprovider.ignore#P"]
    } else if cfg!(target_os = "linux") {
        &["user.com.dropbox.ignored"]
    } else {
        &[]
    };
    for attribute in attributes {
        let mut command = if cfg!(target_os = "macos") {
            let mut command = std::process::Command::new("xattr");
            command.args(["-w", attribute, "1"]).arg(path);
            command
        } else {
            let mut command = std::process::Command::new("setfattr");
            command.args(["-n", attribute, "-v", "1"]).arg(path);
            command
        };
        let quiet = command.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null());
        let _ = quiet.stderr(std::process::Stdio::null()).status();
    }
}

/// The npm project holding managed installs: `package.json`, `.gitignore`, cloud sync marks.
pub(crate) fn ensure_npm_project(root: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(root)?;
    mark_path_ignored_by_cloud_sync(root);
    ensure_git_ignore(root);
    let package_json = root.join("package.json");
    if !package_json.exists() {
        std::fs::write(package_json, "{\n  \"name\": \"pi-extensions\",\n  \"private\": true\n}")?;
    }
    Ok(())
}

/// Remove empty directories from `target`'s parent up to (not including) `root`.
pub(crate) fn prune_empty_parents(target: &Path, root: Option<&Path>) {
    let Some(root) = root.map(normalize) else { return };
    let mut current = target.parent().map(normalize);
    while let Some(dir) = current.filter(|dir| dir.starts_with(&root) && *dir != root) {
        if dir.exists() {
            let empty = std::fs::read_dir(&dir).map(|mut entries| entries.next().is_none()).unwrap_or(false);
            if !empty || std::fs::remove_dir(&dir).is_err() {
                break;
            }
        }
        current = dir.parent().map(Path::to_path_buf);
    }
}

/// `.<name>.<suffix>` next to `target`, for update markers and archive metadata.
pub(crate) fn sibling_marker(target: &Path, suffix: &str) -> PathBuf {
    let name = target.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
    target.with_file_name(format!(".{name}.{suffix}"))
}

fn short_hash(text: &str) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, text.as_bytes());
    digest.as_ref().iter().take(4).map(|byte| format!("{byte:02x}")).collect()
}

impl PackageManager {
    pub(crate) fn assert_project_trusted(&self, scope: SourceScope) -> anyhow::Result<()> {
        if scope == SourceScope::Project && !self.settings.is_project_trusted() {
            bail!("Project is not trusted; refusing to access project package storage");
        }
        Ok(())
    }

    /// Where relative local sources of a scope resolve from.
    pub(crate) fn base_dir_for_scope(&self, scope: SourceScope) -> anyhow::Result<PathBuf> {
        self.assert_project_trusted(scope)?;
        Ok(match scope {
            SourceScope::Project => self.cwd.join(CONFIG_DIR_NAME),
            SourceScope::User => self.agent_dir.clone(),
            SourceScope::Temporary => self.cwd.clone(),
        })
    }

    pub(crate) fn resolve_input_path(&self, input: &str) -> PathBuf {
        resolve_path(input, &self.cwd)
    }

    /// The managed root of an install kind (`npm`, `git`, `url`); `None` for temporary sources.
    pub(crate) fn install_root(&self, scope: SourceScope, kind: &str) -> anyhow::Result<Option<PathBuf>> {
        self.assert_project_trusted(scope)?;
        Ok(match scope {
            SourceScope::Project => Some(self.cwd.join(CONFIG_DIR_NAME).join(kind)),
            SourceScope::User => Some(self.agent_dir.join(kind)),
            SourceScope::Temporary => None,
        })
    }

    /// A hashed directory under the extension temp folder, one per prefix, suffix, and ref.
    pub(crate) fn temporary_dir(&self, prefix: &str, suffix: &str, git_ref: Option<&str>) -> anyhow::Result<PathBuf> {
        let root = resolve_managed_path(&extension_temp_folder(&self.agent_dir)?, &[prefix])?;
        let hash = short_hash(&format!("{prefix}-{suffix}{}", git_ref.map(|r| format!("@{r}")).unwrap_or_default()));
        resolve_managed_path(&root, &[&hash, suffix])
    }

    pub(crate) fn npm_install_root(&self, scope: SourceScope, temporary: bool) -> anyhow::Result<PathBuf> {
        if temporary || scope == SourceScope::Temporary {
            return self.temporary_dir("npm", "", None);
        }
        Ok(self.install_root(scope, "npm")?.unwrap_or_default())
    }

    pub(crate) fn git_install_path(&self, source: &GitSource, scope: SourceScope) -> anyhow::Result<PathBuf> {
        match self.install_root(scope, "git")? {
            Some(root) => resolve_managed_path(&root, &[&source.host, &source.path]),
            None => self.temporary_dir(&format!("git-{}", source.host), &source.path, source.git_ref.as_deref()),
        }
    }

    pub(crate) fn archive_install_path(&self, source: &ArchiveSource, scope: SourceScope) -> anyhow::Result<PathBuf> {
        match self.install_root(scope, "url")? {
            Some(root) => resolve_managed_path(&root, &[&source.host, &source.path]),
            None => self.temporary_dir(&format!("url-{}", source.host), &source.path, None),
        }
    }
}
