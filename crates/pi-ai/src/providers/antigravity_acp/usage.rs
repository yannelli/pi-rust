//! Antigravity quota through the server's `/usage` command, which the server answers itself
//! with its Google credential. pi reads the Markdown report.

use std::time::Duration;

use serde_json::{Value, json};

use super::connection::Incoming;
use super::runtime::{connect, rpc};
use crate::usage::{UsageWindow, iso_from_unix, unix_from_iso};

const SESSION_TIMEOUT: Duration = Duration::from_secs(120);
const USAGE_TIMEOUT: Duration = Duration::from_secs(60);
const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

/// Starts the server, opens a session, and sends `/usage`.
pub(crate) async fn usage() -> Result<Vec<UsageWindow>, String> {
    let cwd = std::env::temp_dir();
    let (connection, _server, _) = connect(&cwd).await?;
    let params = json!({"cwd": cwd, "mcpServers": []});
    let session =
        connection.request("session/new", params, SESSION_TIMEOUT).await.map_err(|error| match error.code {
            -32000 => "Antigravity needs a login. Run /login google-antigravity.".to_string(),
            _ => rpc(error),
        })?;
    let session_id = session["sessionId"].as_str().ok_or("Antigravity ACP returned no session id")?.to_string();
    let prompt = json!({"sessionId": session_id, "prompt": [{"type": "text", "text": "/usage"}]});
    let request = connection.request("session/prompt", prompt, USAGE_TIMEOUT);
    let mut request = std::pin::pin!(request);
    let mut incoming = connection.incoming.lock().await;
    let mut text = String::new();
    let mut take = |event: Incoming| match (event.method.as_str(), event.id) {
        ("session/update", _) => {
            let update = &event.params["update"];
            if update["sessionUpdate"] == "agent_message_chunk" {
                text.push_str(update.pointer("/content/text").and_then(Value::as_str).unwrap_or_default());
            }
        }
        (_, Some(id)) => connection.respond_error(id, -32601, "Method not supported by pi"),
        _ => {}
    };
    loop {
        tokio::select! {
            biased;
            event = incoming.recv() => match event {
                Some(event) => take(event),
                None => return Err(connection.closed_reason().unwrap_or_else(|| "Antigravity ACP server exited".into())),
            },
            reply = &mut request => {
                reply.map_err(rpc)?;
                break;
            }
        }
    }
    while let Ok(event) = incoming.try_recv() {
        take(event);
    }
    parse_report(&text)
}

/// `Oct 02, 2026, 08:22 UTC`, `Oct 02, 2026`, or an ISO time.
fn reset_time(text: &str) -> Option<String> {
    if let Some(seconds) = unix_from_iso(text) {
        return Some(iso_from_unix(seconds));
    }
    let mut parts = text.trim_end_matches(" UTC").split(", ");
    let mut date = parts.next()?.split_whitespace();
    let month_name = date.next()?;
    let month = MONTHS.iter().position(|name| *name == month_name)? + 1;
    let day: u32 = date.next()?.parse().ok()?;
    let year: u32 = parts.next()?.trim().parse().ok()?;
    let clock = parts.next().unwrap_or("00:00").trim();
    unix_from_iso(&format!("{year:04}-{month:02}-{day:02}T{clock}Z")).map(iso_from_unix)
}

fn window_minutes(name: &str) -> Option<u64> {
    let name = name.to_lowercase();
    if name.contains("week") {
        Some(10_080)
    } else if name.contains("daily") || name.contains("day") {
        Some(1440)
    } else if name.contains("5 hour") || name.contains("5-hour") || name.contains("5h") {
        Some(300)
    } else {
        None
    }
}

/// One `- **<bucket>:** 91% remaining (resets ...)` line under `- **<group>**`.
fn bucket(group: &str, line: &str) -> Option<UsageWindow> {
    let (name, status) = line.strip_prefix("- **")?.split_once(":**")?;
    let percent = status.split_once("% remaining")?.0.trim().parse::<f64>().ok()?;
    let resets = status.split_once("(resets ").and_then(|(_, rest)| rest.strip_suffix(')')).and_then(reset_time);
    let short = name.strip_suffix(" Remaining").unwrap_or(name);
    let short = short.strip_suffix(" Limit").unwrap_or(short);
    let label = if group.is_empty() { short.to_string() } else { format!("{group}: {short}") };
    Some(UsageWindow::new(label, 100.0 - percent, resets, window_minutes(name)))
}

/// The `### Quota Limits` section of the `/usage` report.
pub(super) fn parse_report(text: &str) -> Result<Vec<UsageWindow>, String> {
    let Some((_, limits)) = text.split_once("### Quota Limits") else {
        return Err("Antigravity ACP did not answer /usage. Update the server with /login google-antigravity.".into());
    };
    let (mut group, mut windows) = (String::new(), Vec::new());
    for line in limits.lines().map(str::trim_end).filter(|line| !line.trim().is_empty()) {
        let trimmed = line.trim_start();
        if let Some(reason) = trimmed.strip_prefix("- Unable to retrieve quota status (") {
            return Err(format!("Antigravity cannot read the quota: {}", reason.trim_end_matches(')')));
        }
        if trimmed.starts_with("- Not applicable") {
            return Err("Antigravity reports no quota for this login method".into());
        }
        if trimmed.starts_with("- data not available") {
            return Err("Antigravity has no quota data yet".into());
        }
        if line.starts_with("- **") && line.ends_with("**") && !line.contains(":**") {
            group = line.trim_start_matches("- **").trim_end_matches("**").to_string();
        } else if line.starts_with(' ') {
            windows.extend(bucket(&group, trimmed));
        }
    }
    Ok(windows)
}
