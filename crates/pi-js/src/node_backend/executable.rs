//! Finding the `node` binary and checking its version.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::error::JsError;

/// The oldest Node major version the Node backend runs on.
pub const MIN_NODE_MAJOR: u32 = 22;

/// Environment variable with the path of the `node` binary to use.
pub const NODE_PATH_ENV: &str = "PI_NODE_PATH";

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct NodeVersion {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl NodeVersion {
    /// Parse `v22.15.0` or `22.15.0`.
    pub fn parse(text: &str) -> Option<Self> {
        let mut parts = text.trim().trim_start_matches('v').split('.').map(|part| part.parse::<u32>().ok());
        Some(Self { major: parts.next()??, minor: parts.next()??, patch: parts.next().flatten().unwrap_or(0) })
    }

    /// `module.registerHooks` exists from Node 22.15 and 23.5.
    pub fn has_register_hooks(&self) -> bool {
        match self.major {
            22 => self.minor >= 15,
            23 => self.minor >= 5,
            major => major > 23,
        }
    }
}

impl std::fmt::Display for NodeVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "v{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// `PI_NODE_PATH` when set, else the first `node` on `PATH`.
pub fn find_executable() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os(NODE_PATH_ENV).filter(|path| !path.is_empty()) {
        return Some(PathBuf::from(path));
    }
    let names: &[&str] = if cfg!(windows) { &["node.exe", "node.cmd", "node"] } else { &["node"] };
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .flat_map(|dir| names.iter().map(move |name| dir.join(name)))
        .find(|candidate| candidate.is_file())
}

/// Run `node --version` and check it is at least [`MIN_NODE_MAJOR`].
pub fn probe(executable: &Path) -> Result<NodeVersion, JsError> {
    let output =
        Command::new(executable).arg("--version").stdin(Stdio::null()).stderr(Stdio::null()).output().map_err(
            |error| {
                JsError::new(format!("cannot run {}: {error}", executable.display())).with_code("ERR_PI_NODE_MISSING")
            },
        )?;
    let text = String::from_utf8_lossy(&output.stdout);
    let version = NodeVersion::parse(&text).ok_or_else(|| {
        JsError::new(format!("{} --version printed {:?}", executable.display(), text.trim()))
            .with_code("ERR_PI_NODE_MISSING")
    })?;
    if version.major < MIN_NODE_MAJOR {
        return Err(JsError::new(format!(
            "Node {version} at {} is older than the minimum, Node {MIN_NODE_MAJOR}",
            executable.display()
        ))
        .with_code("ERR_PI_NODE_VERSION"));
    }
    Ok(version)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_versions_and_hook_support() {
        let version = NodeVersion::parse("v22.14.0\n").unwrap();
        assert_eq!(version, NodeVersion { major: 22, minor: 14, patch: 0 });
        assert!(!version.has_register_hooks());
        assert!(NodeVersion::parse("v22.15.1").unwrap().has_register_hooks());
        assert!(!NodeVersion::parse("v23.4.0").unwrap().has_register_hooks());
        assert!(NodeVersion::parse("v24.0.0").unwrap().has_register_hooks());
        assert_eq!(NodeVersion::parse("garbage"), None);
    }
}
