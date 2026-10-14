//! `pi experimental`: upstream's development entry points `experimental/cli.ts` (`server` and `client`),
//! `durable/main.ts`, and `vacation/main.ts`, behind `PI_EXPERIMENTAL=1` as upstream gates them.

use super::durable::{DurableApp, run_durable_main};

pub const EXPERIMENTAL_HELP: &str = "Usage: pi experimental <command> [options]

Commands:
  server    Run a foreground server
            --server-id <uuid>  --session-dir <dir>  --provider <name> --model <id>  -e <plugin package>
            --auth-token <token> | --auth-token-file <file>
  client    List Sessions, open the client TUI, or answer one prompt: client [options] [prompt]
            --connect unix:///<dir>/<server-id>.sock | radius://<server-id>
            --session-id <id> | -c, --continue | -r, --resume
            --provider <name> --model <id>  -e <plugin package>  --auth-token <token> | --auth-token-file <file>
  durable   Run the durable coding agent TUI (-c, --continue)
  vacation  Run the vacation planner TUI (-c, --continue)

Requires PI_EXPERIMENTAL=1.
";

/// Run `pi experimental ...`; `None` when experimental features are off or the command is another one.
pub async fn run_experimental_entry(raw: &[String]) -> Option<i32> {
    if raw.first().map(String::as_str) != Some("experimental") || !crate::config::experimental_features_enabled() {
        return None;
    }
    let args = &raw[1..];
    let app = match args.first().map(String::as_str) {
        Some("-h" | "--help") => {
            print!("{EXPERIMENTAL_HELP}");
            return Some(0);
        }
        Some("durable") => DurableApp::Coding,
        Some("vacation") => DurableApp::Vacation,
        _ => return Some(run_server_or_client(args).await),
    };
    match run_durable_main(app, &args[1..]).await {
        Ok(()) => Some(0),
        Err(error) => {
            eprintln!("Error: {error}");
            Some(1)
        }
    }
}

#[cfg(unix)]
async fn run_server_or_client(args: &[String]) -> i32 {
    super::commands::run_experimental_command(args).await
}

/// The server and client use Unix sockets, which this port supports on Unix only.
#[cfg(not(unix))]
async fn run_server_or_client(args: &[String]) -> i32 {
    match super::cli::parse(args) {
        Ok(_) => eprintln!("Error: The experimental server and client require a Unix platform"),
        Err(errors) => errors.iter().for_each(|error| eprintln!("Error: {error}")),
    }
    1
}
