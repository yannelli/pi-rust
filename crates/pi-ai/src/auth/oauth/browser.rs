//! Opening a URL in the platform browser, as upstream's `openBrowser`.

use std::process::{Command, Stdio};

/// The launcher command for this platform. Windows uses `rundll32` rather than
/// `cmd /c start`: cmd.exe re-parses `&`, `|`, and `^` in the URL, which would make
/// a crafted URL run commands. No shell is involved on any platform.
pub fn browser_command(target: &str) -> (&'static str, Vec<String>) {
    if cfg!(target_os = "macos") {
        ("open", vec![target.to_string()])
    } else if cfg!(windows) {
        ("rundll32", vec!["url.dll,FileProtocolHandler".to_string(), target.to_string()])
    } else {
        ("xdg-open", vec![target.to_string()])
    }
}

/// Opens a URL or file with the default handler. Best effort: launcher failures (for
/// example a missing `xdg-open` on a headless machine) are ignored, because callers
/// also show the URL to the user.
pub fn open_browser(target: &str) {
    let (program, args) = browser_command(target);
    let mut command = Command::new(program);
    command.args(args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    detach(&mut command);
    if let Ok(mut child) = command.spawn() {
        // Reap the launcher so it does not linger as a zombie process.
        std::thread::spawn(move || {
            let _ = child.wait();
        });
    }
}

#[cfg(unix)]
fn detach(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    command.process_group(0);
}

#[cfg(windows)]
fn detach(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
}

#[cfg(not(any(unix, windows)))]
fn detach(_command: &mut Command) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launcher_passes_the_url_as_one_argument() {
        let url = "https://example.com/?a=1&b=2|x";
        let (program, args) = browser_command(url);
        assert!(["open", "rundll32", "xdg-open"].contains(&program));
        assert_eq!(args.last().map(String::as_str), Some(url));
    }
}
