//! The text form of the `pi doctor` report.

use pi_ai::auth::AuthType;

use super::DoctorReport;
use crate::resources::brand::APP_NAME;

fn row(text: &mut String, label: &str, value: &str) {
    text.push_str(&format!("  {label:<18}{value}\n"));
}

fn yes_no(flag: bool) -> &'static str {
    if flag { "yes" } else { "no" }
}

fn section(text: &mut String, title: &str) {
    text.push_str(&format!("\n{title}\n"));
}

pub fn render_text(report: &DoctorReport) -> String {
    let mut text = format!("{APP_NAME} {} ({})\n", report.version, report.target);
    let executable = report.install.executable.as_deref().unwrap_or("unknown");
    row(&mut text, "executable", &format!("{executable} ({} install)", report.install.method));
    match report.path.found.as_slice() {
        [] => row(&mut text, "on PATH", "none"),
        found => {
            for (index, path) in found.iter().enumerate() {
                let note = match (index, report.path.current_index == Some(index)) {
                    (0, true) => format!(" (runs as `{APP_NAME}`, this one)"),
                    (0, false) => format!(" (runs as `{APP_NAME}`)"),
                    (_, true) => " (this one)".into(),
                    _ => String::new(),
                };
                row(&mut text, if index == 0 { "on PATH" } else { "" }, &format!("{path}{note}"));
            }
        }
    }

    section(&mut text, "Config");
    let dir_state = if report.config.agent_dir_exists { "" } else { " (missing)" };
    row(&mut text, "config dir", &format!("{}{dir_state}", report.config.agent_dir));
    for file in &report.config.files {
        let state = match (file.exists, file.errors.len()) {
            (false, _) => "not found".to_string(),
            (true, 0) => "ok".to_string(),
            (true, count) => format!("{count} error{}", if count == 1 { "" } else { "s" }),
        };
        row(&mut text, &file.name, &format!("{} ({state})", file.path));
    }

    section(&mut text, "Credentials");
    if report.auth.providers.is_empty() {
        row(&mut text, "none", "");
    }
    for provider in &report.auth.providers {
        let kind = if provider.auth_type == AuthType::OAuth { "OAuth" } else { "API key" };
        row(&mut text, &provider.provider, &format!("{kind} from {}", provider.source));
    }

    section(&mut text, "MCP servers");
    if report.mcp.servers.is_empty() {
        row(&mut text, "none", "");
    }
    for server in &report.mcp.servers {
        let mut notes: Vec<String> = server.scope.iter().map(ToString::to_string).collect();
        let target = match (&server.command, &server.resolved) {
            (Some(command), Some(resolved)) => format!("{command} -> {resolved}"),
            (Some(command), None) => {
                notes.push("not found".into());
                command.clone()
            }
            (None, _) => server.origin.clone().unwrap_or_default(),
        };
        if !server.enabled {
            notes.push("disabled".into());
        }
        row(&mut text, &server.name, &format!("{} {target} ({})", server.transport, notes.join(", ")));
    }

    section(&mut text, "Terminal");
    let terminal = &report.terminal;
    let unset = |value: &Option<String>| value.clone().unwrap_or_else(|| "(unset)".into());
    row(&mut text, "TERM", &unset(&terminal.term));
    row(&mut text, "TERM_PROGRAM", &unset(&terminal.term_program));
    row(&mut text, "colors", &terminal.color_depth);
    row(&mut text, "images", terminal.image_protocol.as_deref().unwrap_or("none"));
    row(&mut text, "hyperlinks", yes_no(terminal.hyperlinks));
    row(&mut text, "keyboard protocol", "negotiated at startup (not queried here)");
    row(&mut text, "interactive", yes_no(terminal.interactive));

    if !report.warnings.is_empty() {
        section(&mut text, "Warnings");
        for warning in &report.warnings {
            text.push_str(&format!("  - {warning}\n"));
        }
    }
    match report.problems.len() {
        0 => text.push_str("\nNo problems found.\n"),
        count => {
            section(&mut text, &format!("Problems ({count})"));
            for problem in &report.problems {
                text.push_str(&format!("  - {problem}\n"));
            }
        }
    }
    text
}
