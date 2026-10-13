//! `piConfig` rebranding. Upstream reads `name` and `configDir` from `package.json`;
//! a Rust fork sets `PI_CONFIG_NAME` and `PI_CONFIG_DIR` when it builds the binary.

use std::path::{Path, PathBuf};

/// The CLI name, `pi` unless the build sets `PI_CONFIG_NAME`.
pub const APP_NAME: &str = match option_env!("PI_CONFIG_NAME") {
    Some(name) => name,
    None => "pi",
};

/// The project and home configuration directory name, `.pi` unless the build sets `PI_CONFIG_DIR`.
pub const CONFIG_DIR_NAME: &str = match option_env!("PI_CONFIG_DIR") {
    Some(dir) => dir,
    None => ".pi",
};

/// The banner title: the custom name for a rebranded build, otherwise `π`.
pub fn app_title() -> &'static str {
    if option_env!("PI_CONFIG_NAME").is_some() { APP_NAME } else { "π" }
}

/// `PI_CODING_AGENT_DIR`, or `<NAME>_CODING_AGENT_DIR` for a rebranded build.
pub fn agent_dir_env_var() -> String {
    format!("{}_CODING_AGENT_DIR", APP_NAME.to_uppercase())
}

/// `PI_CODING_AGENT_SESSION_DIR`, or `<NAME>_CODING_AGENT_SESSION_DIR` for a rebranded build.
pub fn session_dir_env_var() -> String {
    format!("{}_CODING_AGENT_SESSION_DIR", APP_NAME.to_uppercase())
}

/// `<home>/<configDir>/agent`, the agent directory when the environment does not override it.
pub fn default_agent_dir(home: &Path) -> PathBuf {
    home.join(CONFIG_DIR_NAME).join("agent")
}

/// `<cwd>/<configDir>`, the project configuration directory.
pub fn project_config_dir(cwd: &Path) -> PathBuf {
    cwd.join(CONFIG_DIR_NAME)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_brand_matches_upstream() {
        if option_env!("PI_CONFIG_NAME").is_none() {
            assert_eq!(APP_NAME, "pi");
            assert_eq!(app_title(), "π");
            assert_eq!(agent_dir_env_var(), "PI_CODING_AGENT_DIR");
            assert_eq!(session_dir_env_var(), "PI_CODING_AGENT_SESSION_DIR");
        }
        if option_env!("PI_CONFIG_DIR").is_none() {
            assert_eq!(default_agent_dir(Path::new("/home/u")), Path::new("/home/u/.pi/agent"));
        }
    }
}
