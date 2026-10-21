//! `pi sessions rm` and `pi sessions prune`: delete session files after a confirmation, or
//! with `--yes`. A target that does not resolve to exactly one session stops the whole command.

use std::path::Path;

use jiff::tz::TimeZone;

use super::format::{format_bytes, plural};
use super::list::{short_id_len, summary_line};
use super::{Confirm, Flags, SessionScope};
use crate::cli::CommandOutput;
use crate::resources::brand::APP_NAME;
use crate::session::SessionInfo;
use crate::session::ids::resolve_path_from;
use crate::session::listing::build_session_info;

fn failure(stderr: String) -> CommandOutput {
    CommandOutput { code: 1, stdout: String::new(), stderr }
}

/// Session lists read once per command.
struct Lookup<'a> {
    scope: &'a SessionScope,
    all: bool,
    project: Option<Vec<SessionInfo>>,
    everything: Option<Vec<SessionInfo>>,
}

impl Lookup<'_> {
    fn list(&mut self, all: bool) -> &[SessionInfo] {
        let scope = self.scope;
        let slot = if all { &mut self.everything } else { &mut self.project };
        slot.get_or_insert_with(|| scope.sessions(all))
    }

    /// Exact id, else a unique prefix: in this project first (unless `--all`), then everywhere.
    fn resolve(&mut self, target: &str) -> Result<SessionInfo, String> {
        if target.contains(['/', '\\']) || target.ends_with(".jsonl") {
            let path = resolve_path_from(Path::new(target), &self.scope.cwd);
            return build_session_info(&path).ok_or_else(|| format!("Not a session file: {}", path.display()));
        }
        if target.is_empty() {
            return Err("Empty session id".into());
        }
        let scopes: &[bool] = if self.all { &[true] } else { &[false, true] };
        for &all in scopes {
            let sessions = self.list(all);
            if let Some(exact) = sessions.iter().find(|info| info.id == target) {
                return Ok(exact.clone());
            }
            let matches: Vec<&SessionInfo> = sessions.iter().filter(|info| info.id.starts_with(target)).collect();
            match matches.as_slice() {
                [] => continue,
                [only] => return Ok((*only).clone()),
                several => return Err(ambiguous(target, several)),
            }
        }
        Err(format!("No session found matching '{target}'"))
    }
}

fn ambiguous(target: &str, matches: &[&SessionInfo]) -> String {
    let short_len = short_id_len(matches.iter().map(|info| info.id.as_str()));
    let zone = TimeZone::system();
    let lines: Vec<String> = matches.iter().map(|info| format!("  {}", summary_line(info, short_len, &zone))).collect();
    format!("'{target}' matches {}; use a longer prefix:\n{}", plural(matches.len(), "session"), lines.join("\n"))
}

fn file_size(path: &Path) -> u64 {
    std::fs::metadata(path).map(|metadata| metadata.len()).unwrap_or(0)
}

/// Ask before deleting, unless `--yes`. `Err` holds the output that ends the command.
fn confirmed(flags: &Flags, confirm: Option<Confirm<'_>>, listing: &str, count: usize) -> Result<(), CommandOutput> {
    if flags.yes {
        return Ok(());
    }
    let Some(confirm) = confirm else {
        return Err(failure(format!(
            "Error: Refusing to delete {} without confirmation: stdin or stdout is not a terminal. \
             Pass --yes to delete.\n",
            plural(count, "session")
        )));
    };
    let question = format!("{listing}Delete {}? [y/N] ", plural(count, "session"));
    if confirm(&question) {
        Ok(())
    } else {
        Err(CommandOutput { stdout: "Aborted.\n".into(), ..CommandOutput::default() })
    }
}

/// Delete `sessions`, reporting each file and the bytes freed.
fn delete(sessions: &[(SessionInfo, u64)], short_len: usize) -> CommandOutput {
    let zone = TimeZone::system();
    let mut output = CommandOutput::default();
    let (mut deleted, mut freed) = (0, 0);
    for (info, size) in sessions {
        match std::fs::remove_file(&info.path) {
            Ok(()) => {
                deleted += 1;
                freed += size;
                output.stdout.push_str(&format!("Deleted {}\n", summary_line(info, short_len, &zone)));
            }
            Err(error) => {
                output.code = 1;
                output.stderr.push_str(&format!("Error: Could not delete {}: {error}\n", info.path.display()));
            }
        }
    }
    let total = format!("Deleted {}, freed {}.\n", plural(deleted, "session"), format_bytes(freed));
    output.stdout.push_str(&total);
    output
}

fn listing(sessions: &[(SessionInfo, u64)], short_len: usize) -> String {
    let zone = TimeZone::system();
    let lines = sessions
        .iter()
        .map(|(info, size)| format!("  {}  ({})\n", summary_line(info, short_len, &zone), format_bytes(*size)));
    lines.collect()
}

pub(super) fn remove(flags: &Flags, scope: &SessionScope, confirm: Option<Confirm<'_>>) -> CommandOutput {
    if flags.positional.is_empty() {
        return failure(format!(
            "Error: No session given.\nUsage: {APP_NAME} sessions rm <id-prefix|path>... [--all] [--yes]\n"
        ));
    }
    let mut lookup = Lookup { scope, all: flags.all, project: None, everything: None };
    let mut targets: Vec<(SessionInfo, u64)> = Vec::new();
    let mut errors = Vec::new();
    for target in &flags.positional {
        match lookup.resolve(target) {
            Ok(info) if targets.iter().any(|(known, _)| known.path == info.path) => {}
            Ok(info) => {
                let size = file_size(&info.path);
                targets.push((info, size));
            }
            Err(error) => errors.push(format!("Error: {error}\n")),
        }
    }
    if !errors.is_empty() {
        return failure(errors.concat() + "Nothing was deleted.\n");
    }
    let short_len = short_id_len(targets.iter().map(|(info, _)| info.id.as_str()));
    let shown = listing(&targets, short_len);
    if let Err(output) = confirmed(flags, confirm, &shown, targets.len()) {
        return output;
    }
    delete(&targets, short_len)
}

pub(super) fn prune(flags: &Flags, scope: &SessionScope, confirm: Option<Confirm<'_>>) -> CommandOutput {
    let usage = format!("Usage: {APP_NAME} sessions prune --older-than <duration> [--all] [--dry-run] [--yes]\n");
    let Some(age) = flags.older_than else {
        return failure(format!("Error: prune needs --older-than, such as --older-than 30d\n{usage}"));
    };
    if let Some(extra) = flags.positional.first() {
        return failure(format!("Error: Unexpected argument {extra}\n{usage}"));
    }
    let cutoff = pi_ai::now_ms().saturating_sub(age);
    let sessions = scope.sessions(flags.all);
    let short_len = short_id_len(sessions.iter().map(|info| info.id.as_str()));
    let old: Vec<(SessionInfo, u64)> = sessions
        .into_iter()
        .filter(|info| info.modified < cutoff)
        .map(|info| {
            let size = file_size(&info.path);
            (info, size)
        })
        .collect();
    let bytes: u64 = old.iter().map(|(_, size)| size).sum();
    let place = if flags.all { "" } else { " in this project" };
    if old.is_empty() {
        return CommandOutput { stdout: format!("No sessions{place} are older than that.\n"), ..Default::default() };
    }
    let shown = listing(&old, short_len);
    if flags.dry_run {
        let count = plural(old.len(), "session");
        let stdout = format!("{shown}Would delete {count}{place}, freeing {}.\n", format_bytes(bytes));
        return CommandOutput { stdout, ..CommandOutput::default() };
    }
    if let Err(output) = confirmed(flags, confirm, &shown, old.len()) {
        return output;
    }
    delete(&old, short_len)
}
