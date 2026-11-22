//! Helper stdout on Windows. Tokio reads an anonymous child pipe on a blocking thread, which
//! runtime shutdown waits for while a grandchild holds the pipe, so the reactor reads a named pipe.

use std::process::Stdio;
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};

/// An inbound, local-only pipe with an unguessable name and one instance, plus the child's
/// write end. Call it inside a Tokio runtime.
pub(super) fn pipe() -> std::io::Result<(NamedPipeServer, Stdio)> {
    let mut nonce = [0u8; 16];
    getrandom::fill(&mut nonce).map_err(std::io::Error::other)?;
    let name = format!(r"\\.\pipe\pi-dns-helper-{}-{:032x}", std::process::id(), u128::from_le_bytes(nonce));
    let server = ServerOptions::new()
        .first_pipe_instance(true)
        .access_inbound(true)
        .access_outbound(false)
        .reject_remote_clients(true)
        .max_instances(1)
        .create(&name)?;
    let writer = std::fs::OpenOptions::new().write(true).open(&name)?;
    Ok((server, Stdio::from(writer)))
}

#[cfg(test)]
mod tests {
    use super::super::bounded::{HELPER_DEADLINE, run};
    use super::super::system_root::system32;
    use std::process::Command;
    use std::time::{Duration, Instant};

    fn cmd(script: &str, deadline: Duration, limit: usize) -> Option<super::super::bounded::Output> {
        use std::os::windows::process::CommandExt;
        let mut command = Command::new(system32("cmd.exe"));
        command.args(["/d", "/c"]).raw_arg(script);
        run(command, deadline, limit)
    }

    #[test]
    fn returns_output_and_exit_status() {
        let output = cmd("echo DNS configuration", HELPER_DEADLINE, 64).unwrap();
        assert!(output.success);
        assert_eq!(output.stdout, b"DNS configuration\r\n");
        let output = cmd("echo partial& exit 3", HELPER_DEADLINE, 64).unwrap();
        assert_eq!((output.success, output.stdout.as_slice()), (false, b"partial\r\n".as_slice()));
        assert!(cmd("echo 0123456789", HELPER_DEADLINE, 8).is_none());
    }

    #[test]
    fn a_pipe_held_open_by_a_grandchild_stops_at_the_deadline() {
        // `start` may refuse a `\\?\GLOBALROOT` path, so pass the drive path that the kernel root resolves to.
        let powershell = std::fs::canonicalize(system32(r"WindowsPowerShell\v1.0\powershell.exe")).unwrap();
        let powershell = powershell.to_str().unwrap().trim_start_matches(r"\\?\");
        let script = format!(
            "start /b \"\" \"{powershell}\" -NoProfile -NonInteractive -Command Start-Sleep -Seconds 6& echo early"
        );
        let started = Instant::now();
        assert!(cmd(&script, Duration::from_secs(1), 64).is_none());
        assert!(started.elapsed() < Duration::from_secs(4), "{:?}", started.elapsed());
    }
}
