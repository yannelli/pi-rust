//! Detecting the remote system and deploying the daemon there, named by its SHA-256 and checked before every start.

use std::path::{Path, PathBuf};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;

use super::{SshError, SshTarget, UPLOAD_TIMEOUT, run_ssh};

/// A remote operating system the daemon is built for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RemoteSystem {
    Linux,
    Android,
    Darwin,
    Windows,
}

impl RemoteSystem {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Linux => "linux",
            Self::Android => "android",
            Self::Darwin => "darwin",
            Self::Windows => "windows",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RemoteArch {
    X64,
    Arm64,
}

impl RemoteArch {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::X64 => "x64",
            Self::Arm64 => "arm64",
        }
    }
}

/// Windows: the shell `sshd` runs commands with (`DefaultShell`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowsShell {
    Cmd,
    PowerShell,
}

/// A remote system and what detection learned about it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemotePlatform {
    pub platform: RemoteSystem,
    pub arch: RemoteArch,
    /// The remote home directory, in the remote system's own spelling.
    pub home: String,
    pub shell: Option<WindowsShell>,
    /// Problems worth telling the owner about, such as Termux without `termux-exec`.
    pub warnings: Vec<String>,
}

/// POSIX detection, run by the remote login shell; a Windows host answers through PowerShell instead.
const POSIX_PROBE: &str = r#"sh -c 'echo PI-ENV-PROBE; uname -s; uname -m; uname -o 2>/dev/null || echo -; printf "%s\n" "$HOME" "${TMPDIR:--}" "${LD_PRELOAD:--}"'"#;

fn powershell(script: &str) -> String {
    let utf16: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
    format!("powershell -NoProfile -NonInteractive -EncodedCommand {}", STANDARD.encode(utf16))
}

fn windows_probe() -> String {
    powershell(
        "'PI-ENV-PROBE'; 'Windows'; [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString(); '-'; $HOME; '-'; '-'",
    )
}

fn normalize_arch(machine: &str) -> Result<RemoteArch, SshError> {
    match machine.to_ascii_lowercase().as_str() {
        "x86_64" | "amd64" | "x64" => Ok(RemoteArch::X64),
        "aarch64" | "arm64" => Ok(RemoteArch::Arm64),
        _ => Err(SshError::Other(format!("Unsupported remote architecture: {machine}"))),
    }
}

struct Probe {
    system: String,
    machine: String,
    os: String,
    home: String,
    tmpdir: String,
    preload: String,
}

fn parse_probe(target: &SshTarget, output: &str) -> Result<Probe, SshError> {
    // Login shells may print a banner first.
    let lines: Vec<&str> = output.split('\n').map(|line| line.strip_suffix('\r').unwrap_or(line)).collect();
    let Some(start) = lines.iter().position(|line| *line == "PI-ENV-PROBE") else {
        return Err(SshError::Other(format!("Unexpected answer from {}: {}", target.host, output.trim())));
    };
    let field =
        |index: usize, default: &str| lines.get(start + 1 + index).map_or(default.to_string(), |s| s.to_string());
    Ok(Probe {
        system: field(0, ""),
        machine: field(1, ""),
        os: field(2, ""),
        home: field(3, ""),
        tmpdir: field(4, "-"),
        preload: field(5, "-"),
    })
}

/// Termux works only with its own `TMPDIR` and with `termux-exec`, which makes `#!/usr/bin/env` shebangs work.
fn termux_warnings(probe: &Probe) -> Vec<String> {
    let mut warnings = Vec::new();
    if probe.tmpdir == "-" {
        warnings.push("TMPDIR is not set; Termux's sshd normally sets it to $PREFIX/tmp.".into());
    }
    if !probe.preload.contains("termux-exec") {
        warnings.push("termux-exec is not loaded (LD_PRELOAD); scripts with #!/usr/bin/env shebangs will fail.".into());
    }
    warnings
        .push("Android may suspend Termux; run termux-wake-lock on the device to keep the connection alive.".into());
    warnings
}

/// Which system the target runs: `uname` through the login shell, or PowerShell on Windows.
pub async fn detect_platform(target: &SshTarget) -> Result<RemotePlatform, SshError> {
    let posix = run_ssh(target, POSIX_PROBE, None, None, None).await.and_then(|output| parse_probe(target, &output));
    let mut probe = match posix {
        Ok(probe) => probe,
        Err(error @ (SshError::HostKeyUnknown(_) | SshError::HostKeyChanged(_))) => return Err(error),
        // cmd.exe or PowerShell as the remote shell: no `sh`, or one that gets the probe's quotes wrong.
        Err(_) => parse_probe(target, &run_ssh(target, &windows_probe(), None, None, None).await?)?,
    };
    // Git Bash as Windows' default SSH shell: ask PowerShell for Windows' own architecture and home spelling.
    if ["MINGW", "MSYS", "CYGWIN"].iter().any(|prefix| probe.system.starts_with(prefix)) {
        probe = parse_probe(target, &run_ssh(target, &windows_probe(), None, None, None).await?)?;
    }
    let arch = normalize_arch(&probe.machine)?;
    let platform =
        |platform, shell, warnings| RemotePlatform { platform, arch, home: probe.home.clone(), shell, warnings };
    match probe.system.as_str() {
        "Windows" => {
            // cmd.exe expands %OS%; PowerShell prints it as is.
            let cmd = run_ssh(target, "echo %OS%", None, None, None).await?.contains("Windows_NT");
            let shell = if cmd { WindowsShell::Cmd } else { WindowsShell::PowerShell };
            Ok(platform(RemoteSystem::Windows, Some(shell), Vec::new()))
        }
        "Darwin" => Ok(platform(RemoteSystem::Darwin, None, Vec::new())),
        "Linux" if probe.os == "Android" => Ok(platform(RemoteSystem::Android, None, termux_warnings(&probe))),
        "Linux" => Ok(platform(RemoteSystem::Linux, None, Vec::new())),
        system => Err(SshError::Other(format!("Unsupported remote system: {system}"))),
    }
}

/// The default daemon directory: `pi-env-daemons` next to the current executable. Upstream ships the daemons in its
/// npm package's `bin/`; a Rust application installs them next to itself or passes `daemon_dir`.
pub fn default_daemon_dir() -> PathBuf {
    let exe = std::env::current_exe().unwrap_or_default();
    exe.parent().unwrap_or(Path::new(".")).join("pi-env-daemons")
}

/// The daemon binary for a remote system: `<daemon_dir>/pi-env-<platform>-<arch>/pi-env[.exe]`.
pub fn packaged_daemon(remote: &RemotePlatform, daemon_dir: Option<&Path>) -> PathBuf {
    let dir = daemon_dir.map_or_else(default_daemon_dir, Path::to_path_buf);
    let name = format!("pi-env-{}-{}", remote.platform.as_str(), remote.arch.as_str());
    dir.join(name).join(if remote.platform == RemoteSystem::Windows { "pi-env.exe" } else { "pi-env" })
}

fn quote_posix(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

fn quote_powershell(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

/// Where a daemon with this content lives on the remote machine: named by its SHA-256, so versions never collide.
fn daemon_path(remote: &RemotePlatform, sha256: &str) -> String {
    let name = format!("pi-env-{}", &sha256[..32]);
    match remote.platform {
        RemoteSystem::Windows => format!("{}\\.pi\\mobile\\tools\\{name}.exe", remote.home),
        _ => format!("{}/.pi/mobile/tools/{name}", remote.home),
    }
}

const POSIX_HASH: &str = r#"hash() { if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | cut -d' ' -f1; elif command -v shasum >/dev/null 2>&1; then shasum -a 256 "$1" | cut -d' ' -f1; elif command -v openssl >/dev/null 2>&1; then openssl dgst -sha256 "$1" | sed 's/.*= //'; else echo none; fi; }"#;

/// Whether the remote file has this content: `present`, `missing`, or (POSIX without a hash tool) `nohash`.
async fn check_daemon(
    target: &SshTarget,
    remote: &RemotePlatform,
    file: &str,
    sha256: &str,
) -> Result<String, SshError> {
    if remote.platform == RemoteSystem::Windows {
        let check = powershell(&format!(
            "$f = {}; if ((Test-Path -LiteralPath $f) -and ((Get-FileHash -Algorithm SHA256 -LiteralPath $f).Hash.ToLower() -eq '{sha256}')) {{ 'present' }} else {{ 'missing' }}",
            quote_powershell(file)
        ));
        let present = run_ssh(target, &check, None, None, None).await?.trim().ends_with("present");
        return Ok(if present { "present" } else { "missing" }.into());
    }
    let check = format!(
        "{POSIX_HASH}; f={}; if [ -f \"$f\" ] && [ \"$(hash \"$f\")\" = {sha256} ]; then echo present; elif [ \"$(hash /dev/null)\" = none ]; then echo nohash; else echo missing; fi",
        quote_posix(file)
    );
    let output = run_ssh(target, &format!("sh -c {}", quote_posix(&check)), None, None, None).await?;
    Ok(output.trim().split('\n').next_back().unwrap_or("missing").to_string())
}

fn windows_upload(file: &str, sha256: &str) -> String {
    powershell(
        &[
            "$ErrorActionPreference = 'Stop'".to_string(),
            format!("$f = {}", quote_powershell(file)),
            "$d = Split-Path -Parent $f".into(),
            "New-Item -ItemType Directory -Force -Path $d | Out-Null".into(),
            "$t = Join-Path $d ('.pi-env-' + [guid]::NewGuid().ToString() + '.tmp')".into(),
            // PowerShell reads redirected stdin as text lines for `$input`, so the binary comes as base64 lines up
            // to an end marker: Windows' sshd may never pass on the end of stdin.
            "$text = New-Object System.Text.StringBuilder".into(),
            "foreach ($line in $input) { if ($line -eq 'PI-ENV-END') { break }; [void]$text.Append($line) }".into(),
            "$bytes = [Convert]::FromBase64String($text.ToString())".into(),
            "$out = [IO.File]::Open($t, 'CreateNew', 'Write', 'None'); $out.Write($bytes, 0, $bytes.Length); $out.Close()".into(),
            format!("if ((Get-FileHash -Algorithm SHA256 -LiteralPath $t).Hash.ToLower() -ne '{sha256}') {{ Remove-Item -LiteralPath $t; throw 'pi-env upload is corrupt' }}"),
            // A running daemon or a virus scanner can hold the old file for a moment.
            "for ($i = 0; ; $i++) { try { Move-Item -Force -LiteralPath $t -Destination $f; break } catch { if ($i -ge 20) { throw }; Start-Sleep -Milliseconds 250 } }".into(),
            // Older daemons; one that is running stays until it exits.
            "Get-ChildItem -LiteralPath $d -Filter 'pi-env-*.exe' | Where-Object { $_.FullName -ne $f } | ForEach-Object { Remove-Item -LiteralPath $_.FullName -ErrorAction SilentlyContinue }".into(),
            "'deployed'".into(),
        ]
        .join("; "),
    )
}

fn posix_upload(file: &str, sha256: &str) -> String {
    [
        "set -e".to_string(),
        POSIX_HASH.into(),
        format!("f={}", quote_posix(file)),
        "d=$(dirname \"$f\")".into(),
        "mkdir -p \"$d\"".into(),
        "chmod 700 \"$d\"".into(),
        "t=$(mktemp \"$d/.pi-env.XXXXXX\")".into(),
        "cat > \"$t\"".into(),
        format!("if [ \"$(hash \"$t\")\" != {sha256} ]; then rm -f \"$t\"; echo \"pi-env upload is corrupt\" >&2; exit 1; fi"),
        "chmod 700 \"$t\"".into(),
        "mv -f \"$t\" \"$f\"".into(),
        // Older daemons; running ones keep their file open until they exit.
        "for old in \"$d\"/pi-env-*; do [ \"$old\" = \"$f\" ] || rm -f \"$old\"; done".into(),
        "echo deployed".into(),
    ]
    .join("\n")
}

/// Make sure the daemon is on the remote machine, checked by its SHA-256 before it ever runs, and remove daemons of
/// other contents. The upload goes to a new temporary file and is renamed into place only once its hash matches.
/// Returns the remote path of the binary.
pub async fn deploy_daemon(target: &SshTarget, remote: &RemotePlatform, binary: &Path) -> Result<String, SshError> {
    let Ok(bytes) = tokio::fs::read(binary).await else {
        let (platform, arch) = (remote.platform.as_str(), remote.arch.as_str());
        return Err(SshError::Other(format!("No pi-env daemon for {platform}-{arch} at {}", binary.display())));
    };
    let digest = ring::digest::digest(&ring::digest::SHA256, &bytes);
    let sha256: String = digest.as_ref().iter().map(|byte| format!("{byte:02x}")).collect();
    let file = daemon_path(remote, &sha256);
    match check_daemon(target, remote, &file, &sha256).await?.as_str() {
        "present" => return Ok(file),
        "nohash" => {
            let message = format!("{} has no sha256sum, shasum or openssl to check pi-env", target.host);
            return Err(SshError::Other(message));
        }
        _ => {}
    }
    if remote.platform == RemoteSystem::Windows {
        let encoded = STANDARD.encode(&bytes);
        let mut lines: String =
            encoded.as_bytes().chunks(76).map(|chunk| format!("{}\n", String::from_utf8_lossy(chunk))).collect();
        lines.push_str("PI-ENV-END\n");
        run_ssh(target, &windows_upload(&file, &sha256), Some(lines.into_bytes()), None, Some(UPLOAD_TIMEOUT)).await?;
        return Ok(file);
    }
    let upload = format!("sh -c {}", quote_posix(&posix_upload(&file, &sha256)));
    run_ssh(target, &upload, Some(bytes), None, Some(UPLOAD_TIMEOUT)).await?;
    Ok(file)
}

/// The remote command that starts the daemon at `file`, before the `serve` arguments.
pub(crate) fn launch_command(remote: &RemotePlatform, file: &str, login_shell: bool) -> String {
    if remote.platform == RemoteSystem::Windows {
        // PowerShell runs a quoted path only with the call operator; cmd.exe keeps one pair of quotes around a program.
        if remote.shell == Some(WindowsShell::PowerShell) {
            return format!("& {}", quote_powershell(file));
        }
        return if file.contains(' ') { format!("\"{file}\"") } else { file.to_string() };
    }
    // `$0` is the daemon, `"$@"` its arguments; `$SHELL` is the login shell's own name for itself.
    if login_shell {
        return format!("exec \"$SHELL\" -lc 'exec \"$0\" \"$@\"' {}", quote_posix(file));
    }
    quote_posix(file)
}
