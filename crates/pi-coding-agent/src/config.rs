//! Agent directory, settings, and context files. Paths and file names match the
//! TypeScript pi, so both implementations share `~/.pi/agent`.

use std::path::{Path, PathBuf};

pub const ENV_AGENT_DIR: &str = "PI_CODING_AGENT_DIR";
/// Session directory override; `--session-dir` wins over it, and it wins over `sessionDir`.
pub const ENV_SESSION_DIR: &str = "PI_CODING_AGENT_SESSION_DIR";
const CONTEXT_FILE_NAMES: [&str; 5] = ["AGENTS.override.md", "AGENTS.md", "AGENTS.MD", "CLAUDE.md", "CLAUDE.MD"];

pub fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Expand a leading `~` to the home directory.
pub fn expand_tilde(path: &str) -> PathBuf {
    match path.strip_prefix('~') {
        Some(rest) if rest.is_empty() || rest.starts_with('/') || rest.starts_with('\\') => {
            home_dir().join(rest.trim_start_matches(['/', '\\']))
        }
        _ => PathBuf::from(path),
    }
}

/// `$PI_CODING_AGENT_DIR` or `~/.pi/agent`.
pub fn agent_dir() -> PathBuf {
    match std::env::var(ENV_AGENT_DIR) {
        Ok(dir) if !dir.is_empty() => expand_tilde(&dir),
        _ => home_dir().join(".pi").join("agent"),
    }
}

pub fn models_path(agent_dir: &Path) -> PathBuf {
    agent_dir.join("models.json")
}

pub fn settings_path(agent_dir: &Path) -> PathBuf {
    agent_dir.join("settings.json")
}

pub fn auth_path(agent_dir: &Path) -> PathBuf {
    agent_dir.join("auth.json")
}

/// Whether an environment flag is set to `1`, `true`, or `yes`.
pub fn env_flag(name: &str) -> bool {
    pi_ai::env::process_env_value(name)
        .is_some_and(|value| value == "1" || value.eq_ignore_ascii_case("true") || value.eq_ignore_ascii_case("yes"))
}

/// Offline mode (`--offline` or `PI_OFFLINE`): no startup network operations.
pub fn offline() -> bool {
    env_flag("PI_OFFLINE")
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContextFile {
    pub path: PathBuf,
    pub content: String,
}

fn context_file_in(dir: &Path) -> Option<ContextFile> {
    CONTEXT_FILE_NAMES.iter().find_map(|name| {
        let path = dir.join(name);
        if !path.is_file() {
            return None;
        }
        let content = std::fs::read_to_string(&path).ok()?;
        Some(ContextFile { path, content: content.trim_start_matches('\u{feff}').to_string() })
    })
}

/// The global context file, then one per directory from the filesystem root down to `cwd`.
pub fn load_context_files(cwd: &Path, agent_dir: &Path) -> Vec<ContextFile> {
    let mut files = Vec::new();
    if let Some(global) = context_file_in(agent_dir) {
        files.push(global);
    }
    let mut ancestors: Vec<ContextFile> = cwd.ancestors().filter_map(context_file_in).collect();
    ancestors.reverse();
    for file in ancestors {
        if !files.iter().any(|existing| existing.path == file.path) {
            files.push(file);
        }
    }
    files
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_files_load_global_then_root_to_cwd() {
        let root = tempfile::tempdir().unwrap();
        let agent = root.path().join("agent");
        let nested = root.path().join("repo").join("sub");
        std::fs::create_dir_all(&agent).unwrap();
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(agent.join("AGENTS.md"), "global").unwrap();
        std::fs::write(root.path().join("repo").join("CLAUDE.md"), "\u{feff}repo").unwrap();
        std::fs::write(nested.join("AGENTS.md"), "sub").unwrap();
        std::fs::write(nested.join("CLAUDE.md"), "ignored").unwrap();
        let contents: Vec<String> = load_context_files(&nested, &agent).into_iter().map(|file| file.content).collect();
        assert_eq!(contents, ["global", "repo", "sub"]);
    }
}
