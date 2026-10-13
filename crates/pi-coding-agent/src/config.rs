//! Agent directory, settings, and context files. Paths and file names match the
//! TypeScript pi, so both implementations share `~/.pi/agent`.

use std::path::{Path, PathBuf};

use crate::resources::brand::{agent_dir_env_var, default_agent_dir};

/// The agent directory variable of the default brand; [`agent_dir_env_var`] follows a rebrand.
pub const ENV_AGENT_DIR: &str = "PI_CODING_AGENT_DIR";
/// Session directory override of the default brand; `--session-dir` wins over it, and it wins
/// over `sessionDir`. [`crate::resources::brand::session_dir_env_var`] follows a rebrand.
pub const ENV_SESSION_DIR: &str = "PI_CODING_AGENT_SESSION_DIR";

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

/// `$PI_CODING_AGENT_DIR` or `~/.pi/agent`, with the brand's variable and directory names.
pub fn agent_dir() -> PathBuf {
    match std::env::var(agent_dir_env_var()) {
        Ok(dir) if !dir.is_empty() => expand_tilde(&dir),
        _ => default_agent_dir(&home_dir()),
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

/// `PI_EXPERIMENTAL=1` enables experimental features (upstream `areExperimentalFeaturesEnabled`).
pub fn experimental_features_enabled() -> bool {
    pi_ai::env::process_env_value("PI_EXPERIMENTAL").as_deref() == Some("1")
}

pub use crate::resources::ContextFile;

/// The global context file, then one per directory from the filesystem root down to `cwd`.
pub fn load_context_files(cwd: &Path, agent_dir: &Path) -> Vec<ContextFile> {
    crate::resources::load_project_context_files(cwd, agent_dir)
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
