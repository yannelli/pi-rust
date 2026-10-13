//! Context files (`AGENTS.override.md`, `AGENTS.md`, `CLAUDE.md`, ...), ported from
//! `loadProjectContextFiles` in upstream `core/resource-loader.ts`.

use std::path::{Path, PathBuf};

use super::paths::{absolute, canonicalize};

/// Candidate names in each directory; the first existing file wins.
pub const CONTEXT_FILE_NAMES: [&str; 5] = ["AGENTS.override.md", "AGENTS.md", "AGENTS.MD", "CLAUDE.md", "CLAUDE.MD"];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContextFile {
    pub path: PathBuf,
    pub content: String,
}

/// The context file of one directory. Directories named like context files are skipped.
pub fn context_file_in(dir: &Path) -> Option<ContextFile> {
    CONTEXT_FILE_NAMES.iter().find_map(|name| {
        let path = dir.join(name);
        if !path.is_file() {
            return None;
        }
        match std::fs::read_to_string(&path) {
            Ok(content) => Some(ContextFile { path, content: content.trim_start_matches('\u{feff}').to_string() }),
            Err(error) => {
                eprintln!("Warning: Could not read {}: {error}", path.display());
                None
            }
        }
    })
}

struct GitPaths {
    repo_dir: PathBuf,
    common_git_dir: PathBuf,
}

/// The nearest `.git` directory or `gitdir:` file, with the shared git dir of a linked worktree.
fn find_git_paths(cwd: &Path) -> Option<GitPaths> {
    for dir in cwd.ancestors() {
        let git_path = dir.join(".git");
        let Ok(meta) = std::fs::metadata(&git_path) else { continue };
        if meta.is_file() {
            let content = std::fs::read_to_string(&git_path).ok()?;
            let Some(target) = content.trim().strip_prefix("gitdir: ") else { continue };
            let git_dir = super::paths::normalize(&dir.join(target.trim()));
            if !git_dir.join("HEAD").exists() {
                return None;
            }
            let common_git_dir = match std::fs::read_to_string(git_dir.join("commondir")) {
                Ok(common) => super::paths::normalize(&git_dir.join(common.trim())),
                Err(_) => git_dir,
            };
            return Some(GitPaths { repo_dir: dir.to_path_buf(), common_git_dir });
        }
        if meta.is_dir() {
            return git_path
                .join("HEAD")
                .exists()
                .then(|| GitPaths { repo_dir: dir.to_path_buf(), common_git_dir: git_path });
        }
    }
    None
}

/// In a linked worktree nested inside its main checkout, the main checkout's context file
/// that the worktree's own copy shadows. Returned in canonical form.
fn find_shadowed_context_file(cwd: &Path) -> Option<PathBuf> {
    let git = find_git_paths(cwd)?;
    let common_git_dir = canonicalize(&git.common_git_dir);
    let worktree_root = canonicalize(&git.repo_dir);
    let main_root = common_git_dir.parent()?.to_path_buf();
    if worktree_root == main_root || !worktree_root.starts_with(&main_root) {
        return None;
    }
    if canonicalize(&main_root.join(".git")) != common_git_dir {
        return None;
    }
    let worktree_file = context_file_in(&worktree_root)?;
    Some(main_root.join(worktree_file.path.file_name()?))
}

/// The agent directory's context file, then one per directory from the filesystem root
/// down to `cwd`. Context files load whether or not the project is trusted.
pub fn load_project_context_files(cwd: &Path, agent_dir: &Path) -> Vec<ContextFile> {
    let cwd = absolute(cwd);
    let mut files: Vec<ContextFile> = context_file_in(&absolute(agent_dir)).into_iter().collect();
    let shadowed = find_shadowed_context_file(&cwd);
    let mut ancestors = Vec::new();
    for dir in cwd.ancestors() {
        let Some(file) = context_file_in(dir) else { continue };
        let is_shadowed = shadowed.as_ref().is_some_and(|shadowed| {
            file.path.file_name().is_some_and(|name| canonicalize(dir).join(name) == *shadowed)
        });
        let seen = files.iter().chain(&ancestors).any(|existing: &ContextFile| existing.path == file.path);
        if !is_shadowed && !seen {
            ancestors.push(file);
        }
    }
    ancestors.reverse();
    files.extend(ancestors);
    files
}
