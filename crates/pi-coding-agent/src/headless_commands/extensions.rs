//! `/extensions [list|status|enable <ref>|disable <ref>]`: `list`, `enable`, and `disable` run
//! `pi extensions` on the session's directories, a change reloads the session's extensions, and
//! `status` shows what the session loaded.

use std::sync::Arc;

use super::{HeadlessOutput, fenced, reload};
use crate::agent_session::{AgentSession, BUILTIN_EXTENSIONS};
use crate::packages::{APPLY_HINT, CommandOutput, PackageCommandOptions, run_config_command};
use crate::resources::source_info::BUILTIN_PATH_PREFIX;

const USAGE: &str = "Usage: /extensions [list | status | enable <name-or-path> | disable <name-or-path>]";

pub(super) async fn run(session: &Arc<AgentSession>, args: &str) -> HeadlessOutput {
    let mut words = args.split_whitespace().map(String::from);
    match words.next().as_deref() {
        None | Some("list") => {
            let (code, output) = pi_extensions(session, "list", words.collect()).await;
            HeadlessOutput::plain(if code == 0 { fenced(&output) } else { output })
        }
        Some("status") => HeadlessOutput::plain(fenced(&status(session))),
        Some(action @ ("enable" | "disable")) => change(session, action, words.collect()).await,
        Some(_) => HeadlessOutput::plain(USAGE),
    }
}

/// Run `pi extensions <action> <args>`; returns the exit code and its output.
async fn pi_extensions(session: &AgentSession, action: &str, args: Vec<String>) -> (i32, String) {
    let (cwd, agent_dir, home) = {
        let services = session.services();
        let home = services.resources().options.home.clone();
        (services.cwd.clone(), services.agent_dir.clone(), home)
    };
    let mut options = PackageCommandOptions::new(cwd, agent_dir);
    options.home = home;
    options.builtin_extensions = BUILTIN_EXTENSIONS.iter().map(|name| name.to_string()).collect();
    let (stdout, out) = CommandOutput::capture();
    let (stderr, err) = CommandOutput::capture();
    (options.stdout, options.stderr) = (stdout, stderr);
    let command: Vec<String> = ["extensions", action].into_iter().map(String::from).chain(args).collect();
    let code = run_config_command(&command, &options, None).await.unwrap_or(1);
    let read = |buffer: &std::sync::Mutex<Vec<u8>>| {
        String::from_utf8_lossy(&buffer.lock().unwrap_or_else(|poisoned| poisoned.into_inner())).trim().to_string()
    };
    let (out, err) = (read(&out), read(&err));
    let text = match (out.is_empty(), err.is_empty()) {
        (_, true) => out,
        (true, false) => err,
        (false, false) => format!("{out}\n{err}"),
    };
    (code, text)
}

/// Save the change as `pi extensions enable|disable` does, then reload the session.
async fn change(session: &Arc<AgentSession>, action: &str, args: Vec<String>) -> HeadlessOutput {
    if args.first().is_none_or(|reference| reference.starts_with('-')) {
        return HeadlessOutput::plain(USAGE);
    }
    let (code, output) = pi_extensions(session, action, args).await;
    if code != 0 {
        return HeadlessOutput::plain(output);
    }
    let saved = output.strip_suffix(APPLY_HINT).unwrap_or(&output).to_string();
    let reloaded = reload(session).await;
    HeadlessOutput { text: format!("{saved}\n{}", reloaded.text), reloaded: reloaded.reloaded }
}

/// The session's extensions with their load errors, and the built-in extensions on or off.
fn status(session: &AgentSession) -> String {
    let runner = session.extension_runner();
    let errors = runner.as_ref().map(|runner| runner.load_errors()).unwrap_or_default();
    let mut lines = vec!["Extensions in this session:".to_string()];
    let mut listed = Vec::new();
    {
        let services = session.services();
        let loader = services.resources();
        for entry in &loader.resources().extensions {
            let path = entry.path.display().to_string();
            if path.starts_with(BUILTIN_PATH_PREFIX) {
                continue;
            }
            let state = match errors.iter().find(|error| error.path == path) {
                Some(error) => format!("failed: {}", error.error),
                None if runner.is_some() => "loaded".to_string(),
                None => "not loaded".to_string(),
            };
            lines.push(format!("  {path}  ({})  {state}", entry.source_info.source));
            listed.push(path);
        }
    }
    if let Some(runner) = &runner {
        for extension in &runner.registry().extensions {
            if !listed.contains(&extension.path) {
                lines.push(format!("  {}  loaded", extension.path));
                listed.push(extension.path.clone());
            }
        }
    }
    for error in errors.iter().filter(|error| !listed.contains(&error.path)) {
        lines.push(format!("  {}  failed: {}", error.path, error.error));
    }
    if lines.len() == 1 {
        lines.push("  none".into());
    }
    lines.push(String::new());
    lines.push("Built-in extensions ([x] on, [ ] off):".into());
    for name in BUILTIN_EXTENSIONS {
        let mark = if session.builtin_extension_enabled(name) { "[x]" } else { "[ ]" };
        lines.push(format!("  {mark} {name}"));
    }
    lines.join("\n")
}
