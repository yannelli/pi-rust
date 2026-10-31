//! Install an ACP registry agent as an `acpAgents` entry: store an `npx` or `uvx` command, or
//! download, check, and unpack the binary archive for this platform.

use std::ffi::OsStr;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use super::entry::AcpAgentEntry;
use super::registry::{BinaryTarget, RegistryAgent, platform_key};

const MAX_DOWNLOAD_BYTES: usize = 1 << 30;

/// `<agent_dir>/acp/agents`, the parent of every binary install.
pub fn agents_root(agent_dir: &Path) -> PathBuf {
    agent_dir.join("acp").join("agents")
}

pub async fn install(agent: &RegistryAgent, agent_dir: &Path) -> Result<AcpAgentEntry, String> {
    let path = std::env::var_os("PATH");
    let home = pi_mcp::runtime::transport::home_dir();
    install_with(&pi_ai::http::shared_client(), agent, agent_dir, path.as_deref(), home.as_deref()).await
}

/// Uses a binary for this platform first, then `npx`, then `uvx`. `path` is the PATH to search
/// for a built-in program such as `kiro-cli`, then its install folders under `home`.
pub(crate) async fn install_with(
    client: &reqwest::Client,
    agent: &RegistryAgent,
    agent_dir: &Path,
    path: Option<&OsStr>,
    home: Option<&Path>,
) -> Result<AcpAgentEntry, String> {
    let non_empty = |value: &str| (!value.is_empty()).then(|| value.to_string());
    let mut entry = AcpAgentEntry {
        id: agent.id.clone(),
        name: agent.name.clone(),
        version: non_empty(&agent.version),
        description: non_empty(&agent.description),
        command: String::new(),
        args: Vec::new(),
        env: Default::default(),
        enabled: true,
        source: if agent.builtin { "builtin" } else { "registry" }.into(),
        install_dir: None,
        login_command: None,
    };
    let distribution = &agent.distribution;
    if let Some(target) = platform_key().and_then(|key| distribution.binary.get(key)) {
        let dir = install_binary(client, agent, target, agent_dir).await?;
        let command = dir.join(relative_path(&target.cmd)?);
        entry.command = std::path::absolute(&command).unwrap_or(command).display().to_string();
        entry.args = target.args.clone();
        entry.env = target.env.clone();
        entry.install_dir = Some(dir.display().to_string());
    } else if let Some(npx) = &distribution.npx {
        entry.command = "npx".into();
        entry.args = ["-y".to_string(), npx.package.clone()].into_iter().chain(npx.args.iter().cloned()).collect();
        entry.env = npx.env.clone();
    } else if let Some(uvx) = &distribution.uvx {
        entry.command = "uvx".into();
        entry.args = std::iter::once(uvx.package.clone()).chain(uvx.args.iter().cloned()).collect();
        entry.env = uvx.env.clone();
    } else if let Some(local) = &agent.local {
        entry.command = match find_on_path(&local.program, path) {
            Some(_) => local.program.clone(),
            None => match find_in_install_dirs(local, home) {
                Some(found) => found.display().to_string(),
                None => return Err(format!("{} is not on PATH. {}", local.program, local.install_hint)),
            },
        };
        entry.args = local.args.clone();
        entry.login_command = (!local.login.is_empty()).then(|| local.login.clone());
    } else {
        let platform = platform_key().unwrap_or("this platform");
        return Err(format!("{} has no distribution for {platform}", agent.name));
    }
    Ok(entry)
}

/// Remove the install directory of `entry` when it is under [`agents_root`].
pub fn uninstall(entry: &AcpAgentEntry, agent_dir: &Path) -> Result<(), String> {
    let Some(dir) = entry.install_dir.as_deref() else { return Ok(()) };
    let (Ok(dir), Ok(root)) = (std::fs::canonicalize(dir), std::fs::canonicalize(agents_root(agent_dir))) else {
        return Ok(());
    };
    if dir == root || !dir.starts_with(&root) {
        return Ok(());
    }
    std::fs::remove_dir_all(&dir).map_err(|error| format!("Could not remove {}: {error}", dir.display()))?;
    if let Some(parent) = dir.parent().filter(|parent| *parent != root) {
        let _ = std::fs::remove_dir(parent);
    }
    Ok(())
}

async fn install_binary(
    client: &reqwest::Client,
    agent: &RegistryAgent,
    target: &BinaryTarget,
    agent_dir: &Path,
) -> Result<PathBuf, String> {
    let version = if agent.version.is_empty() { "latest" } else { agent.version.as_str() };
    let root = agents_root(agent_dir).join(relative_component(&agent.id)?);
    let dir = root.join(relative_component(version)?);
    let cmd = relative_path(&target.cmd)?;
    let bytes = download(client, &target.archive).await?;
    if let Some(expected) = target.sha256.as_deref().map(str::trim).filter(|sha| !sha.is_empty())
        && !sha256_hex(&bytes).eq_ignore_ascii_case(expected)
    {
        return Err(format!("{} does not match the SHA-256 of the registry. Nothing was installed.", target.archive));
    }
    let staging = root.join(format!(".staging-{version}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&staging);
    let result = std::fs::create_dir_all(&staging)
        .map_err(|error| error.to_string())
        .and_then(|()| unpack(&bytes, &target.archive, &cmd, &staging))
        .and_then(|()| make_executable(&staging.join(&cmd), &target.cmd))
        .and_then(|()| {
            if dir.exists() {
                std::fs::remove_dir_all(&dir).map_err(|error| error.to_string())?;
            }
            std::fs::rename(&staging, &dir).map_err(|error| error.to_string())
        });
    if let Err(error) = result {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(format!("Could not install {}: {error}", agent.name));
    }
    Ok(dir)
}

async fn download(client: &reqwest::Client, url: &str) -> Result<Vec<u8>, String> {
    let failed = |error: &dyn std::fmt::Display| format!("Could not download {url}: {error}");
    let mut response = client
        .get(url)
        .header(reqwest::header::USER_AGENT, crate::platform::pi_user_agent(crate::platform::VERSION))
        .send()
        .await
        .map_err(|error| failed(&error))?;
    if !response.status().is_success() {
        return Err(failed(&format!("HTTP {}", response.status().as_u16())));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|error| failed(&error))? {
        if bytes.len() + chunk.len() > MAX_DOWNLOAD_BYTES {
            return Err(failed(&"the archive is larger than 1 GiB"));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn sha256_hex(bytes: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, bytes).as_ref().iter().map(|byte| format!("{byte:02x}")).collect()
}

/// A path with only normal components, so it stays inside the directory it is joined to.
fn safe_relative(path: &Path) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => out.push(part),
            Component::CurDir => {}
            _ => return None,
        }
    }
    (!out.as_os_str().is_empty()).then_some(out)
}

fn relative_path(value: &str) -> Result<PathBuf, String> {
    safe_relative(Path::new(value)).ok_or_else(|| format!("\"{value}\" is not a relative path in the archive"))
}

fn relative_component(value: &str) -> Result<&str, String> {
    let ok = !value.is_empty() && value != "." && value != ".." && !value.contains(['/', '\\', ':']);
    ok.then_some(value).ok_or_else(|| format!("\"{value}\" is not a valid directory name"))
}

/// Unpack by the file name of `url`. A name without an archive suffix is the program itself.
fn unpack(bytes: &[u8], url: &str, cmd: &Path, dest: &Path) -> Result<(), String> {
    let name = url.split(['?', '#']).next().unwrap_or_default().to_ascii_lowercase();
    if name.ends_with(".zip") {
        unzip(bytes, dest).map_err(|error| format!("Could not unpack {url}: {error}"))
    } else if name.ends_with(".tar.gz") || name.ends_with(".tgz") {
        untar(bytes, dest).map_err(|error| format!("Could not unpack {url}: {error}"))
    } else if name.ends_with(".tar.bz2") || name.ends_with(".tbz2") {
        Err(format!("{url} is a bzip2 archive, which pi cannot unpack"))
    } else {
        let target = dest.join(cmd);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        std::fs::write(&target, bytes).map_err(|error| error.to_string())
    }
}

fn untar(bytes: &[u8], dest: &Path) -> std::io::Result<()> {
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(bytes));
    for entry in archive.entries()? {
        let mut entry = entry?;
        let kind = entry.header().entry_type();
        let Some(relative) = safe_relative(&entry.path()?) else { continue };
        let target = dest.join(relative);
        if kind.is_dir() {
            std::fs::create_dir_all(&target)?;
        } else if kind.is_file() {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            entry.unpack(&target)?;
        }
    }
    Ok(())
}

fn unzip(bytes: &[u8], dest: &Path) -> Result<(), String> {
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes)).map_err(|error| error.to_string())?;
    for index in 0..archive.len() {
        let mut file = archive.by_index(index).map_err(|error| error.to_string())?;
        if file.is_symlink() {
            continue;
        }
        let Some(relative) = file.enclosed_name().and_then(|name| safe_relative(&name)) else { continue };
        let target = dest.join(relative);
        if file.is_dir() {
            std::fs::create_dir_all(&target).map_err(|error| error.to_string())?;
            continue;
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        let mut content = Vec::new();
        file.read_to_end(&mut content).map_err(|error| error.to_string())?;
        std::fs::write(&target, content).map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn make_executable(file: &Path, cmd: &str) -> Result<(), String> {
    if !file.is_file() {
        return Err(format!("the archive has no {cmd}"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o755)).map_err(|error| error.to_string())?;
    }
    Ok(())
}

/// `program` in the folders its installer uses, where `~` is `home`.
pub(crate) fn find_in_install_dirs(local: &super::registry::LocalCommand, home: Option<&Path>) -> Option<PathBuf> {
    let home = home?;
    let dirs = local.install_dirs.iter().map(|dir| pi_mcp::runtime::transport::expand_home_with(dir, Some(home)));
    let joined = std::env::join_paths(dirs).ok()?;
    find_on_path(&local.program, Some(&joined))
}

/// Whether installing `agent` needs its program, which is not installed yet.
pub fn needs_program(agent: &RegistryAgent, path: Option<&OsStr>) -> bool {
    let distribution = &agent.distribution;
    let packaged = platform_key().is_some_and(|key| distribution.binary.contains_key(key))
        || distribution.npx.is_some()
        || distribution.uvx.is_some();
    let Some(local) = agent.local.as_ref().filter(|_| !packaged) else { return false };
    let home = pi_mcp::runtime::transport::home_dir();
    find_on_path(&local.program, path).is_none() && find_in_install_dirs(local, home.as_deref()).is_none()
}

/// The first file named `program` (with `.exe` or `.cmd` on Windows) in the directories of `path`.
pub(crate) fn find_on_path(program: &str, path: Option<&OsStr>) -> Option<PathBuf> {
    let names: Vec<String> = if cfg!(windows) {
        vec![format!("{program}.exe"), format!("{program}.cmd"), program.to_string()]
    } else {
        vec![program.to_string()]
    };
    std::env::split_paths(path?)
        .flat_map(|dir| names.iter().map(move |name| dir.join(name)))
        .find(|file| file.is_file())
}
