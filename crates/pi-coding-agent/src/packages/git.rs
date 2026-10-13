//! Git package checkouts: clone, reconcile to the configured ref or the remote branch,
//! clean, reinstall dependencies, and detect available updates.

use std::path::Path;

use anyhow::anyhow;

use super::git_url::GitSource;
use super::manifest::has_missing_dependencies;
use super::paths::{ensure_git_ignore, prune_empty_parents, sibling_marker};
use super::runner::{CommandSpec, git_dependency_install_args};
use super::{NETWORK_TIMEOUT, PackageManager, ProgressAction};
use crate::resources::SourceScope;

/// What an unpinned checkout updates to: the ref to reset to and how to fetch it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct GitUpdateTarget {
    pub git_ref: String,
    pub head: String,
    pub fetch_args: Vec<String>,
}

fn update_marker(target: &Path) -> std::path::PathBuf {
    sibling_marker(target, "pi-update-incomplete")
}

fn branch_fetch_args(branch: &str) -> Vec<String> {
    let refspec = format!("+refs/heads/{branch}:refs/remotes/origin/{branch}");
    ["fetch", "--prune", "--no-tags", "origin", &refspec].iter().map(|arg| arg.to_string()).collect()
}

fn first_hash(text: &str, suffix: Option<&str>) -> Option<String> {
    text.lines().find_map(|line| {
        let (hash, rest) = line.split_once(char::is_whitespace)?;
        let valid = hash.len() == 40 && hash.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
        (valid && suffix.is_none_or(|suffix| rest.trim() == suffix)).then(|| hash.to_string())
    })
}

impl PackageManager {
    pub(crate) async fn git(&self, args: &[&str], cwd: &Path) -> anyhow::Result<()> {
        self.runner.run(&CommandSpec::new("git", args.iter().copied()).in_dir(cwd)).await
    }

    pub(crate) async fn git_capture(&self, args: &[&str], cwd: &Path) -> anyhow::Result<String> {
        let spec = CommandSpec::new("git", args.iter().copied()).in_dir(cwd).with_timeout(NETWORK_TIMEOUT);
        self.runner.capture(&spec).await
    }

    async fn git_remote_capture(&self, args: &[&str], cwd: &Path) -> anyhow::Result<String> {
        let spec = CommandSpec::new("git", args.iter().copied()).in_dir(cwd).with_timeout(NETWORK_TIMEOUT);
        self.runner.capture(&spec.with_env("GIT_TERMINAL_PROMPT", "0")).await
    }

    async fn install_git_dependencies(&self, dir: &Path) -> anyhow::Result<()> {
        let manager = self.package_manager_name()?;
        self.run_npm(git_dependency_install_args(&manager), Some(dir)).await
    }

    pub(crate) async fn install_git(&self, source: &GitSource, scope: SourceScope) -> anyhow::Result<()> {
        let target = self.git_install_path(source, scope)?;
        if target.exists() {
            if let Some(git_ref) = &source.git_ref {
                return self
                    .ensure_git_ref(&target, &["fetch".into(), "origin".into(), git_ref.clone()], "FETCH_HEAD")
                    .await;
            }
            let update = self.local_git_update_target(&target).await?;
            return self.ensure_git_ref(&target, &update.fetch_args, &update.git_ref).await;
        }
        let root = self.install_root(scope, "git")?;
        if let Some(root) = &root {
            ensure_git_ignore(root);
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let _ = std::fs::remove_file(update_marker(&target));
        let result = async {
            let target_text = target.display().to_string();
            self.runner.run(&CommandSpec::new("git", ["clone", &source.repo, &target_text])).await?;
            if let Some(git_ref) = &source.git_ref {
                self.git(&["checkout", git_ref], &target).await?;
            }
            if target.join("package.json").exists() {
                self.install_git_dependencies(&target).await?;
            }
            anyhow::Ok(())
        }
        .await;
        if result.is_err() {
            let _ = std::fs::remove_dir_all(&target);
            prune_empty_parents(&target, root.as_deref());
        }
        result
    }

    pub(crate) async fn update_git(&self, source: &GitSource, scope: SourceScope) -> anyhow::Result<()> {
        let target = self.git_install_path(source, scope)?;
        if !target.exists() {
            return self.install_git(source, scope).await;
        }
        if let Some(git_ref) = &source.git_ref {
            return self
                .ensure_git_ref(&target, &["fetch".into(), "origin".into(), git_ref.clone()], "FETCH_HEAD")
                .await;
        }
        let update = self.local_git_update_target(&target).await?;
        self.ensure_git_ref(&target, &update.fetch_args, &update.git_ref).await
    }

    /// The tracked upstream branch, else the remote default branch (`origin/HEAD`).
    pub(crate) async fn local_git_update_target(&self, dir: &Path) -> anyhow::Result<GitUpdateTarget> {
        if let Ok(upstream) = self.git_capture(&["rev-parse", "--abbrev-ref", "@{upstream}"], dir).await
            && let Some(branch) = upstream.trim().strip_prefix("origin/").filter(|branch| !branch.is_empty())
            && let Ok(head) = self.git_capture(&["rev-parse", "@{upstream}"], dir).await
        {
            return Ok(GitUpdateTarget { git_ref: "@{upstream}".into(), head, fetch_args: branch_fetch_args(branch) });
        }
        let _ = self.git(&["remote", "set-head", "origin", "-a"], dir).await;
        let head = self.git_capture(&["rev-parse", "origin/HEAD"], dir).await?;
        let origin_head =
            self.git_capture(&["symbolic-ref", "refs/remotes/origin/HEAD"], dir).await.unwrap_or_default();
        let branch = origin_head.trim().trim_start_matches("refs/remotes/origin/");
        let fetch_args = if branch.is_empty() {
            ["fetch", "--prune", "--no-tags", "origin", "+HEAD:refs/remotes/origin/HEAD"].map(String::from).to_vec()
        } else {
            branch_fetch_args(branch)
        };
        Ok(GitUpdateTarget { git_ref: "origin/HEAD".into(), head, fetch_args })
    }

    /// Fetch only `git_ref`, then reset, clean, and reinstall when HEAD moves. An interrupted
    /// earlier update (its marker file remains) is finished even when HEAD already matches.
    async fn ensure_git_ref(&self, dir: &Path, fetch_args: &[String], git_ref: &str) -> anyhow::Result<()> {
        let fetch: Vec<&str> = fetch_args.iter().map(String::as_str).collect();
        self.git(&fetch, dir).await?;
        let local = self.git_capture(&["rev-parse", "HEAD"], dir).await?;
        let commit_ref = format!("{git_ref}^{{commit}}");
        let target = self.git_capture(&["rev-parse", &commit_ref], dir).await?;
        let marker = update_marker(dir);
        if local.trim() == target.trim() {
            if marker.exists() {
                return self.clean_and_install_git_dependencies(dir, &marker).await;
            }
            return self.repair_missing_git_dependencies(dir).await;
        }
        std::fs::write(&marker, "")?;
        self.git(&["reset", "--hard", &commit_ref], dir).await?;
        self.clean_and_install_git_dependencies(dir, &marker).await
    }

    async fn clean_and_install_git_dependencies(&self, dir: &Path, marker: &Path) -> anyhow::Result<()> {
        if let Err(error) = self.git(&["clean", "-fdx"], dir).await {
            let _ = self.repair_missing_git_dependencies(dir).await;
            return Err(error);
        }
        if dir.join("package.json").exists() {
            self.install_git_dependencies(dir).await?;
        }
        let _ = std::fs::remove_file(marker);
        Ok(())
    }

    async fn repair_missing_git_dependencies(&self, dir: &Path) -> anyhow::Result<()> {
        if !has_missing_dependencies(dir) {
            return Ok(());
        }
        self.install_git_dependencies(dir).await
    }

    /// Refresh an unpinned `-e` checkout; a failed refresh keeps the cached checkout.
    pub(crate) async fn refresh_temporary_git_source(&self, source: &GitSource, source_text: &str) {
        if self.is_offline() {
            return;
        }
        let message = format!("Refreshing {source_text}...");
        let refresh = self.update_git(source, SourceScope::Temporary);
        let _ = self.with_progress(ProgressAction::Pull, source_text, message, refresh).await;
    }

    pub(crate) fn remove_git(&self, source: &GitSource, scope: SourceScope) -> anyhow::Result<()> {
        let target = self.git_install_path(source, scope)?;
        if target.exists() {
            std::fs::remove_dir_all(&target)?;
        }
        let _ = std::fs::remove_file(update_marker(&target));
        prune_empty_parents(&target, self.install_root(scope, "git")?.as_deref());
        Ok(())
    }

    pub(crate) async fn git_has_available_update(&self, dir: &Path) -> bool {
        if self.is_offline() {
            return false;
        }
        let Ok(local) = self.git_capture(&["rev-parse", "HEAD"], dir).await else { return false };
        match self.remote_git_head(dir).await {
            Ok(remote) => local.trim() != remote.trim(),
            Err(_) => false,
        }
    }

    async fn remote_git_head(&self, dir: &Path) -> anyhow::Result<String> {
        if let Ok(upstream) = self.git_capture(&["rev-parse", "--abbrev-ref", "@{upstream}"], dir).await
            && let Some(branch) = upstream.trim().strip_prefix("origin/").filter(|branch| !branch.is_empty())
        {
            let remote_ref = format!("refs/heads/{branch}");
            let output = self.git_remote_capture(&["ls-remote", "origin", &remote_ref], dir).await?;
            if let Some(hash) = first_hash(&output, None) {
                return Ok(hash);
            }
        }
        let output = self.git_remote_capture(&["ls-remote", "origin", "HEAD"], dir).await?;
        first_hash(&output, Some("HEAD")).ok_or_else(|| anyhow!("Failed to determine remote HEAD"))
    }
}
