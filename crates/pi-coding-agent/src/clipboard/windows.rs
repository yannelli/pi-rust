//! The Windows clipboard through PowerShell, natively and from WSL through interop.
//! Text goes through a UTF-8 file because clip.exe and PowerShell stdin decode piped
//! bytes with the console code page, which mangles non-ASCII text.

use std::path::{Path, PathBuf};
use std::time::Duration;

use super::ClipboardContext;
use crate::export::session_file::random_hex;

const WSLPATH_TIMEOUT: Duration = Duration::from_millis(1000);
const POWERSHELL_TIMEOUT: Duration = Duration::from_millis(5000);

/// Removes the temporary file when dropped.
struct TempFile(PathBuf);

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn temp_file(extension: &str) -> TempFile {
    TempFile(std::env::temp_dir().join(format!("pi-wsl-clip-{}.{extension}", random_hex(32))))
}

fn write_private(path: &Path, text: &str) -> std::io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    std::io::Write::write_all(&mut options.open(path)?, text.as_bytes())
}

fn quote(path: &str) -> String {
    path.replace('\'', "''")
}

fn powershell(context: &ClipboardContext<'_>, script: String) -> Option<Vec<u8>> {
    let args = ["-NoProfile".to_string(), "-Command".to_string(), script];
    context.runner.run("powershell.exe", &args, None, POWERSHELL_TIMEOUT)
}

fn windows_path(context: &ClipboardContext<'_>, path: &Path, from_wsl: bool) -> Option<String> {
    if !from_wsl {
        return Some(path.display().to_string());
    }
    let args = ["-w".to_string(), path.display().to_string()];
    let output = context.runner.run("wslpath", &args, None, WSLPATH_TIMEOUT)?;
    Some(String::from_utf8_lossy(&output).trim().to_string()).filter(|path| !path.is_empty())
}

/// `Set-Clipboard` from a temporary UTF-8 file; `from_wsl` converts the path through `wslpath`.
pub(super) fn copy_text(context: &ClipboardContext<'_>, text: &str, from_wsl: bool) -> bool {
    let file = temp_file("txt");
    if write_private(&file.0, text).is_err() {
        return false;
    }
    let Some(win_path) = windows_path(context, &file.0, from_wsl) else { return false };
    let script = format!(
        "Set-Clipboard -Value ([System.IO.File]::ReadAllText('{}', [System.Text.Encoding]::UTF8))",
        quote(&win_path)
    );
    powershell(context, script).is_some()
}

/// `Get-Clipboard -Raw` written as UTF-8 without a trailing newline.
pub(super) fn read_text(context: &ClipboardContext<'_>) -> Option<Vec<u8>> {
    let script = "[Console]::OutputEncoding = [System.Text.Encoding]::UTF8; \
                  $t = Get-Clipboard -Raw; if ($t) { [Console]::Out.Write($t) }";
    powershell(context, script.to_string())
}

/// Save the clipboard image as PNG through System.Windows.Forms. `None` when there is none.
pub(super) fn read_image_png(context: &ClipboardContext<'_>, from_wsl: bool) -> Option<Vec<u8>> {
    let file = temp_file("png");
    let win_path = windows_path(context, &file.0, from_wsl)?;
    let script = [
        "Add-Type -AssemblyName System.Windows.Forms".to_string(),
        "Add-Type -AssemblyName System.Drawing".to_string(),
        format!("$path = '{}'", quote(&win_path)),
        "$img = [System.Windows.Forms.Clipboard]::GetImage()".to_string(),
        "if ($img) { $img.Save($path, [System.Drawing.Imaging.ImageFormat]::Png); Write-Output 'ok' } else { Write-Output 'empty' }"
            .to_string(),
    ]
    .join("; ");
    let output = powershell(context, script)?;
    if String::from_utf8_lossy(&output).trim() != "ok" {
        return None;
    }
    std::fs::read(&file.0).ok().filter(|bytes| !bytes.is_empty())
}
