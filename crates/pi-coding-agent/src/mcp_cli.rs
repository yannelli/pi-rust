//! `pi mcp add|remove|list|login|logout` (upstream `main.ts` and `extensions/mcp/cli.ts`): the
//! project's `.pi/mcp.json` counts only when the project is trusted, `login` opens the platform
//! browser, and on a terminal it also accepts the redirect URL pasted from the browser.

use std::io::IsTerminal;
use std::path::Path;
use std::sync::Arc;

use futures::FutureExt;
use pi_mcp::cli::ReadRedirectFn;
use pi_mcp::{McpCommandOptions, run_mcp_command};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio_util::sync::CancellationToken;

use crate::resources::trust::ProjectTrustStore;

const PASTE_PROMPT: &str = "If the browser cannot reach this machine, paste the URL it was redirected to: ";

/// Read one line from stdin, or `None` once `cancel` fires (the browser callback arrived or the
/// sign-in timed out) or stdin closes.
async fn read_redirect_url(cancel: CancellationToken) -> Option<String> {
    let mut stderr = tokio::io::stderr();
    let _ = stderr.write_all(PASTE_PROMPT.as_bytes()).await;
    let _ = stderr.flush().await;
    let mut line = String::new();
    let mut reader = BufReader::new(tokio::io::stdin());
    tokio::select! {
        () = cancel.cancelled() => None,
        read = reader.read_line(&mut line) => match read {
            Ok(count) if count > 0 => Some(line.trim().to_string()),
            _ => None,
        },
    }
}

/// Options of `pi mcp` for `cwd`: trust from `trust.json`, the browser, and the paste prompt.
pub fn mcp_command_options(cwd: &Path, agent_dir: &Path) -> McpCommandOptions {
    let mut options = McpCommandOptions::new(cwd, agent_dir);
    options.project_trusted = ProjectTrustStore::new(agent_dir).get(cwd).ok().flatten() == Some(true);
    options.open_url = Some(Arc::new(|url: &str| pi_ai::auth::oauth::open_browser(url)));
    if std::io::stdin().is_terminal() {
        let read: ReadRedirectFn = Arc::new(|cancel| read_redirect_url(cancel).boxed());
        options.read_redirect_url = Some(read);
    }
    options
}

/// Run `pi mcp ...` when `raw` starts with `mcp`; `None` for other commands.
pub async fn run_mcp_cli(raw: &[String], cwd: &Path, agent_dir: &Path) -> Option<i32> {
    if raw.first().map(String::as_str) != Some("mcp") {
        return None;
    }
    Some(run_mcp_command(&raw[1..], &mcp_command_options(cwd, agent_dir)).await)
}
