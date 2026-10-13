//! Editing text in an external editor (upstream `modes/interactive/external-editor.ts`).

use std::io::Write;
use std::process::Stdio;

/// How an external edit ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExternalEditorResult {
    /// The editor exited with status 0; the file content without a final newline.
    Complete(String),
    /// The editor could not start or exited with an error.
    Failed,
}

/// The configured editor command: `$VISUAL`, `$EDITOR`, then `notepad` or `nano`.
pub fn default_editor_command() -> String {
    ["VISUAL", "EDITOR"]
        .iter()
        .filter_map(|name| std::env::var(name).ok())
        .find(|value| !value.is_empty())
        .unwrap_or_else(|| if cfg!(windows) { "notepad" } else { "nano" }.to_string())
}

/// A fresh `pi-editor-*` directory under the temp dir, removed on drop.
struct TempDirectory(std::path::PathBuf);

impl TempDirectory {
    fn create() -> std::io::Result<Self> {
        let base = std::env::temp_dir();
        for attempt in 0u32..100 {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.subsec_nanos())
                .unwrap_or_default();
            let name = format!("pi-editor-{:x}{:08x}{attempt}", std::process::id(), nanos);
            let path = base.join(name);
            match std::fs::create_dir(&path) {
                Ok(()) => return Ok(Self(path)),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
        Err(std::io::Error::other("could not create a temporary directory"))
    }
}

impl Drop for TempDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Writes `content` to a temporary `prompt.md`, runs `command` on it with the terminal attached,
/// and reads the result back. The command is split on spaces; Windows runs it through the shell.
pub async fn edit_in_external_editor(command: &str, content: &str) -> ExternalEditorResult {
    let Ok(directory) = TempDirectory::create() else {
        return ExternalEditorResult::Failed;
    };
    let path = directory.0.join("prompt.md");
    if std::fs::write(&path, content).is_err() {
        return ExternalEditorResult::Failed;
    }
    let mut stdout = std::io::stdout();
    let _ = write!(stdout, "Launching external editor: {command}\nPi will resume when the editor exits.\n");
    let _ = stdout.flush();

    let mut parts = command.split(' ');
    let editor = parts.next().unwrap_or_default();
    let mut child = if cfg!(windows) {
        let mut shell = tokio::process::Command::new("cmd");
        shell.arg("/C").arg(editor).args(parts).arg(&path);
        shell
    } else {
        let mut direct = tokio::process::Command::new(editor);
        direct.args(parts).arg(&path);
        direct
    };
    child.stdin(Stdio::inherit()).stdout(Stdio::inherit()).stderr(Stdio::inherit());
    match child.status().await {
        Ok(status) if status.success() => {}
        _ => return ExternalEditorResult::Failed,
    }
    match std::fs::read_to_string(&path) {
        Ok(text) => {
            let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
            ExternalEditorResult::Complete(text.strip_suffix('\n').unwrap_or(text).to_string())
        }
        Err(_) => ExternalEditorResult::Failed,
    }
}
