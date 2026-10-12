//! Agent directory, settings, and context files. Paths and file names match the
//! TypeScript pi, so both implementations share `~/.pi/agent`.

use std::path::{Path, PathBuf};

use pi_ai::ThinkingLevel;
use serde::{Deserialize, Serialize};

pub const ENV_AGENT_DIR: &str = "PI_CODING_AGENT_DIR";
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

/// The subset of `settings.json` this port reads. Unknown keys are preserved on save.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_thinking_level: Option<ThinkingLevel>,
    #[serde(flatten)]
    pub other: serde_json::Map<String, serde_json::Value>,
}

impl Settings {
    /// Global settings overlaid by `<cwd>/.pi/settings.json`.
    pub fn load(agent_dir: &Path, cwd: &Path) -> Self {
        let mut settings = read_settings(&settings_path(agent_dir));
        let project = read_settings(&cwd.join(".pi").join("settings.json"));
        settings.default_provider = project.default_provider.or(settings.default_provider);
        settings.default_model = project.default_model.or(settings.default_model);
        settings.default_thinking_level = project.default_thinking_level.or(settings.default_thinking_level);
        settings
    }

    /// Save the default model and thinking level to the global settings file.
    pub fn save_defaults(agent_dir: &Path, provider: &str, model: &str, thinking: ThinkingLevel) -> anyhow::Result<()> {
        let path = settings_path(agent_dir);
        let mut settings = read_settings(&path);
        settings.default_provider = Some(provider.into());
        settings.default_model = Some(model.into());
        settings.default_thinking_level = Some(thinking);
        std::fs::create_dir_all(agent_dir)?;
        std::fs::write(&path, serde_json::to_string_pretty(&settings)? + "\n")?;
        Ok(())
    }
}

fn read_settings(path: &Path) -> Settings {
    std::fs::read_to_string(path).ok().and_then(|text| serde_json::from_str(&text).ok()).unwrap_or_default()
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

    #[test]
    fn settings_overlay_project_values_and_keep_unknown_keys() {
        let root = tempfile::tempdir().unwrap();
        let agent = root.path().join("agent");
        std::fs::create_dir_all(root.path().join(".pi")).unwrap();
        std::fs::create_dir_all(&agent).unwrap();
        std::fs::write(settings_path(&agent), r#"{"defaultProvider":"openai","defaultModel":"a","theme":"dark"}"#)
            .unwrap();
        std::fs::write(root.path().join(".pi/settings.json"), r#"{"defaultModel":"b"}"#).unwrap();
        let settings = Settings::load(&agent, root.path());
        assert_eq!(settings.default_provider.as_deref(), Some("openai"));
        assert_eq!(settings.default_model.as_deref(), Some("b"));
        Settings::save_defaults(&agent, "anthropic", "c", ThinkingLevel::High).unwrap();
        let saved: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(settings_path(&agent)).unwrap()).unwrap();
        assert_eq!(saved["theme"], "dark");
        assert_eq!(saved["defaultThinkingLevel"], "high");
    }
}
