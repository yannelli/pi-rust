//! Git branch, extension statuses, and provider facts for the footer (upstream
//! `core/footer-data-provider.ts`). The branch is read from `HEAD`, which is polled for changes.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime};

use pi_tui::ui_loop::{self, TimerId};

use crate::extensions::ComponentFactory;

/// Where a repository keeps its metadata (worktrees point `.git` at another directory).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitPaths {
    pub repo_dir: PathBuf,
    pub common_git_dir: PathBuf,
    pub head_path: PathBuf,
}

/// Walk up from `cwd` to the repository that contains it.
pub fn find_git_paths(cwd: &Path) -> Option<GitPaths> {
    let mut dir = cwd.to_path_buf();
    loop {
        let git = dir.join(".git");
        if let Ok(metadata) = std::fs::metadata(&git) {
            if metadata.is_file() {
                let content = std::fs::read_to_string(&git).ok()?;
                let git_dir = dir.join(content.trim().strip_prefix("gitdir: ")?.trim());
                let head_path = git_dir.join("HEAD");
                if !head_path.exists() {
                    return None;
                }
                let common = match std::fs::read_to_string(git_dir.join("commondir")) {
                    Ok(common) => git_dir.join(common.trim()),
                    Err(_) => git_dir.clone(),
                };
                return Some(GitPaths { repo_dir: dir, common_git_dir: common, head_path });
            }
            if metadata.is_dir() {
                let head_path = git.join("HEAD");
                return head_path.exists().then_some(GitPaths { repo_dir: dir, common_git_dir: git, head_path });
            }
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// `git symbolic-ref --short HEAD`, for reftable repositories whose `HEAD` is a placeholder.
fn branch_from_git(repo_dir: &Path) -> Option<String> {
    let output = Command::new("git")
        .args(["--no-optional-locks", "symbolic-ref", "--quiet", "--short", "HEAD"])
        .current_dir(repo_dir)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let branch = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (output.status.success() && !branch.is_empty()).then_some(branch)
}

/// The branch name, `detached`, or `None` outside a repository.
pub fn resolve_git_branch(paths: Option<&GitPaths>) -> Option<String> {
    let paths = paths?;
    let content = std::fs::read_to_string(&paths.head_path).ok()?;
    match content.trim().strip_prefix("ref: refs/heads/") {
        Some(".invalid") => Some(branch_from_git(&paths.repo_dir).unwrap_or_else(|| "detached".to_string())),
        Some(branch) => Some(branch.to_string()),
        None => Some("detached".to_string()),
    }
}

fn head_stamp(paths: Option<&GitPaths>) -> Option<(SystemTime, u64)> {
    let paths = paths?;
    let mut watched = vec![paths.head_path.clone()];
    let tables = paths.common_git_dir.join("reftable").join("tables.list");
    if tables.exists() {
        watched.push(tables);
    }
    watched
        .iter()
        .filter_map(|path| std::fs::metadata(path).ok())
        .map(|metadata| (metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH), metadata.len()))
        .max()
}

/// Footer facts that the session does not hold.
pub struct FooterData {
    cwd: PathBuf,
    git_paths: Option<GitPaths>,
    cached_branch: Option<Option<String>>,
    extension_statuses: BTreeMap<String, String>,
    available_provider_count: usize,
    subscriptions: HashMap<String, bool>,
    stamp: Option<(SystemTime, u64)>,
    watcher: Option<TimerId>,
    branch_listeners: Vec<(u64, ComponentFactory)>,
}

impl FooterData {
    pub fn new(cwd: &Path) -> Self {
        let git_paths = find_git_paths(cwd);
        let stamp = head_stamp(git_paths.as_ref());
        Self {
            cwd: cwd.to_path_buf(),
            git_paths,
            cached_branch: None,
            extension_statuses: BTreeMap::new(),
            available_provider_count: 0,
            subscriptions: HashMap::new(),
            stamp,
            watcher: None,
            branch_listeners: Vec::new(),
        }
    }

    /// Current branch, `detached`, or `None` outside a repository.
    pub fn git_branch(&mut self) -> Option<String> {
        if self.cached_branch.is_none() {
            self.cached_branch = Some(resolve_git_branch(self.git_paths.as_ref()));
        }
        self.cached_branch.clone().flatten()
    }

    /// The working tree of the repository that contains the cwd.
    pub fn repo_dir(&self) -> Option<&Path> {
        self.git_paths.as_ref().map(|paths| paths.repo_dir.as_path())
    }

    pub fn extension_statuses(&self) -> &BTreeMap<String, String> {
        &self.extension_statuses
    }

    pub fn set_extension_status(&mut self, key: &str, text: Option<&str>) {
        match text {
            Some(text) => self.extension_statuses.insert(key.to_string(), text.to_string()),
            None => self.extension_statuses.remove(key),
        };
    }

    pub fn clear_extension_statuses(&mut self) {
        self.extension_statuses.clear();
    }

    pub fn available_provider_count(&self) -> usize {
        self.available_provider_count
    }

    pub fn set_available_provider_count(&mut self, count: usize) {
        self.available_provider_count = count;
    }

    /// Whether `provider` authenticates with a subscription (OAuth), as last checked.
    pub fn is_using_subscription(&self, provider: &str) -> bool {
        self.subscriptions.get(provider).copied().unwrap_or(false)
    }

    pub fn set_subscription(&mut self, provider: &str, subscription: bool) {
        self.subscriptions.insert(provider.to_string(), subscription);
    }

    /// Keep the extension `onBranchChange` callback `key` of `factory`'s runner.
    pub fn add_branch_listener(&mut self, factory: ComponentFactory, key: u64) {
        self.branch_listeners.push((key, factory));
    }

    pub fn remove_branch_listener(&mut self, target: u64, key: u64) {
        self.branch_listeners.retain(|(existing, factory)| *existing != key || factory.target() != target);
    }

    pub fn clear_branch_listeners(&mut self) {
        self.branch_listeners.clear();
    }

    pub fn branch_listeners(&self) -> Vec<(u64, ComponentFactory)> {
        self.branch_listeners.clone()
    }

    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// Follow another directory, for example after switching to a session elsewhere.
    pub fn set_cwd(&mut self, cwd: &Path) -> bool {
        if self.cwd == cwd {
            return false;
        }
        self.cwd = cwd.to_path_buf();
        self.git_paths = find_git_paths(cwd);
        self.stamp = head_stamp(self.git_paths.as_ref());
        self.cached_branch = None;
        true
    }

    /// Re-read the branch when `HEAD` changed since the last check. Returns true on a change.
    pub fn poll_branch(&mut self) -> bool {
        let stamp = head_stamp(self.git_paths.as_ref());
        if stamp == self.stamp {
            return false;
        }
        self.stamp = stamp;
        let branch = resolve_git_branch(self.git_paths.as_ref());
        let changed = self.cached_branch.as_ref().is_some_and(|cached| *cached != branch);
        self.cached_branch = Some(branch);
        changed
    }

    /// Poll `HEAD` every second on the UI loop and call `on_change` when the branch changes.
    pub fn watch(data: &std::rc::Rc<std::cell::RefCell<FooterData>>, mut on_change: impl FnMut() + 'static) {
        let weak = std::rc::Rc::downgrade(data);
        let id = ui_loop::set_interval(Duration::from_secs(1), move || {
            let changed =
                weak.upgrade().is_some_and(|data| data.try_borrow_mut().is_ok_and(|mut data| data.poll_branch()));
            if changed {
                on_change();
            }
        });
        if let Some(old) = data.borrow_mut().watcher.replace(id) {
            ui_loop::clear_timer(old);
        }
    }

    pub fn dispose(&mut self) {
        if let Some(id) = self.watcher.take() {
            ui_loop::clear_timer(id);
        }
    }
}

impl Drop for FooterData {
    fn drop(&mut self) {
        self.dispose();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_branches_from_head_and_worktree_links() {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::write(repo.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::create_dir_all(repo.join("src/deep")).unwrap();
        let mut data = FooterData::new(&repo.join("src/deep"));
        assert_eq!(data.git_branch().as_deref(), Some("main"));

        std::fs::write(repo.join(".git/HEAD"), "0123456789abcdef\n").unwrap();
        assert_eq!(resolve_git_branch(data.git_paths.as_ref()).as_deref(), Some("detached"));

        let worktree_git = repo.join(".git/worktrees/wt");
        std::fs::create_dir_all(&worktree_git).unwrap();
        std::fs::write(worktree_git.join("HEAD"), "ref: refs/heads/feature\n").unwrap();
        std::fs::write(worktree_git.join("commondir"), "../..\n").unwrap();
        let worktree = root.path().join("wt");
        std::fs::create_dir_all(&worktree).unwrap();
        std::fs::write(worktree.join(".git"), format!("gitdir: {}\n", worktree_git.display())).unwrap();
        let paths = find_git_paths(&worktree).unwrap();
        assert_eq!(resolve_git_branch(Some(&paths)).as_deref(), Some("feature"));
        assert!(paths.common_git_dir.ends_with("worktrees/wt/../.."));

        let outside = tempfile::tempdir().unwrap();
        assert_eq!(FooterData::new(outside.path()).git_branch(), None);
    }

    #[test]
    fn notices_branch_switches() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join(".git")).unwrap();
        std::fs::write(root.path().join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        let mut data = FooterData::new(root.path());
        assert_eq!(data.git_branch().as_deref(), Some("main"));
        std::fs::write(root.path().join(".git/HEAD"), "ref: refs/heads/a-much-longer-branch\n").unwrap();
        assert!(data.poll_branch());
        assert_eq!(data.git_branch().as_deref(), Some("a-much-longer-branch"));
    }
}
