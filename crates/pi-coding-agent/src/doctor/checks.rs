//! The install, `PATH`, MCP, and terminal parts of `pi doctor`.

use std::ffi::OsStr;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use pi_tui::terminal_image::{ImageProtocol, detect_capabilities};

use super::{DoctorInputs, InstallReport, McpReport, McpServerReport, PathReport, TerminalReport};
use crate::resources::brand::APP_NAME;
use crate::resources::trust::ProjectTrustStore;
use crate::update::{InstallMethod, detect_install_method};

/// The target this binary was built for, such as `x86_64-unknown-linux-gnu`.
pub fn target_triple() -> String {
    let arch = std::env::consts::ARCH;
    let env = if cfg!(target_env = "musl") {
        "-musl"
    } else if cfg!(target_env = "gnu") {
        "-gnu"
    } else if cfg!(target_env = "msvc") {
        "-msvc"
    } else {
        ""
    };
    let system = match std::env::consts::OS {
        "macos" => "apple-darwin".to_string(),
        "windows" => "pc-windows".to_string(),
        "android" => "linux-android".to_string(),
        other => format!("unknown-{other}"),
    };
    format!("{arch}-{system}{env}")
}

/// File names a command lookup tries: `name`, plus each `PATHEXT` extension on Windows.
fn candidates(name: &str) -> Vec<String> {
    if !cfg!(windows) || Path::new(name).extension().is_some() {
        return vec![name.to_string()];
    }
    let extensions = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into());
    extensions
        .split(';')
        .filter(|extension| !extension.is_empty())
        .map(|extension| format!("{name}{extension}"))
        .collect()
}

fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        path.metadata().is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

/// Every executable named `name` on `path_var`, in lookup order.
pub fn find_on_path(name: &str, path_var: Option<&OsStr>) -> Vec<PathBuf> {
    let Some(path_var) = path_var else { return Vec::new() };
    let mut found = Vec::new();
    for dir in std::env::split_paths(path_var).filter(|dir| !dir.as_os_str().is_empty()) {
        for candidate in candidates(name) {
            let path = dir.join(candidate);
            if is_executable(&path) && !found.contains(&path) {
                found.push(path);
            }
        }
    }
    found
}

/// Where `command` runs from: a path relative to `cwd`, or the first match on `path_var`.
pub fn find_command(command: &str, cwd: &Path, path_var: Option<&OsStr>) -> Option<PathBuf> {
    if command.is_empty() {
        return None;
    }
    if Path::new(command).components().count() > 1 {
        let path = cwd.join(command);
        return candidates(&path.display().to_string()).into_iter().map(PathBuf::from).find(|path| is_executable(path));
    }
    find_on_path(command, path_var).into_iter().next()
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

pub(super) fn install(inputs: &DoctorInputs) -> InstallReport {
    let method = match &inputs.executable {
        Some(executable) => match detect_install_method(executable, inputs.cargo_home.as_deref(), &inputs.home) {
            InstallMethod::Cargo => "cargo",
            InstallMethod::Release => "release",
        },
        None => "unknown",
    };
    InstallReport {
        method: method.into(),
        executable: inputs.executable.as_ref().map(|path| path.display().to_string()),
    }
}

pub(super) fn path(inputs: &DoctorInputs) -> PathReport {
    let found = find_on_path(APP_NAME, inputs.path_var.as_deref());
    let current = |path: &PathBuf| inputs.executable.as_ref().is_some_and(|executable| same_file(path, executable));
    let position = found.iter().position(current);
    PathReport {
        found: found.iter().map(|path| path.display().to_string()).collect(),
        current_index: position,
        on_path: position.is_some(),
        shadowed: position.is_some_and(|position| position > 0),
    }
}

/// `scheme://host:port` of `url`; paths and queries can hold tokens.
fn url_origin(url: &str) -> Option<String> {
    url::Url::parse(url).ok().map(|url| url.origin().ascii_serialization()).filter(|origin| origin != "null")
}

pub(super) fn mcp(inputs: &DoctorInputs, problems: &mut Vec<String>) -> McpReport {
    let trusted = ProjectTrustStore::new(&inputs.agent_dir).get(&inputs.cwd).ok().flatten() == Some(true);
    let loaded = pi_mcp::load_mcp_config(&inputs.agent_dir, &inputs.cwd, trusted);
    problems.extend(loaded.errors.iter().map(|error| format!("MCP config {error}")));
    let mut servers = Vec::new();
    for entry in &loaded.servers {
        let config = &entry.config;
        let mut server = McpServerReport {
            name: entry.name.clone(),
            scope: entry.scope,
            enabled: config.is_enabled(),
            transport: if config.is_http() { "http" } else { "stdio" }.into(),
            command: None,
            resolved: None,
            origin: config.url.as_deref().and_then(url_origin),
        };
        if !config.is_http() {
            let command = pi_mcp::runtime::transport::expand_home(config.command.as_deref().unwrap_or_default());
            let cwd = inputs.cwd.join(pi_mcp::runtime::transport::expand_home(config.cwd.as_deref().unwrap_or(".")));
            // A literal PATH in the server's env replaces the inherited one.
            let env_path = config
                .env_pairs()
                .into_iter()
                .find(|(key, value)| *key == "PATH" && !value.contains('$') && !value.starts_with('!'));
            let path_var = env_path.map(|(_, value)| std::ffi::OsString::from(value)).or(inputs.path_var.clone());
            server.resolved = find_command(&command, &cwd, path_var.as_deref()).map(|path| path.display().to_string());
            if server.resolved.is_none() && server.enabled {
                problems.push(format!("MCP server \"{}\": command \"{command}\" not found", entry.name));
            }
            server.command = Some(command);
        }
        servers.push(server);
    }
    McpReport { project_trusted: trusted, servers, errors: loaded.errors }
}

pub(super) fn terminal() -> TerminalReport {
    let var = |name: &str| std::env::var(name).ok().filter(|value| !value.is_empty());
    let capabilities = detect_capabilities();
    let image_protocol = capabilities.images.map(|protocol| match protocol {
        ImageProtocol::Kitty => "kitty".to_string(),
        ImageProtocol::Iterm2 => "iterm2".to_string(),
    });
    TerminalReport {
        term: var("TERM"),
        term_program: var("TERM_PROGRAM"),
        colorterm: var("COLORTERM"),
        interactive: std::io::stdin().is_terminal() && std::io::stdout().is_terminal(),
        color_depth: if capabilities.true_color { "truecolor" } else { "256" }.into(),
        image_protocol,
        hyperlinks: capabilities.hyperlinks,
        keyboard_protocol: None,
    }
}
