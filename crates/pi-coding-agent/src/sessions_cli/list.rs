//! `pi sessions list`: the sessions of this project or of every project, newest first.

use jiff::tz::TimeZone;
use serde::Serialize;

use super::format::{format_cost, local_time, one_line, shorten_home, table};
use super::scan::session_cost;
use super::{Flags, SessionScope};
use crate::cli::CommandOutput;
use crate::session::{SessionInfo, format_iso};

/// Short ids have at least this many characters.
pub const MIN_SHORT_ID: usize = 8;
/// Names and first prompts are cut to this many cells.
pub const TITLE_WIDTH: usize = 60;

/// One row of `pi sessions list --json`.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ListedSession {
    pub id: String,
    /// The shortest prefix (at least 8 characters) that tells the listed sessions apart.
    pub short_id: String,
    pub path: String,
    pub cwd: String,
    pub name: Option<String>,
    pub first_message: String,
    /// ISO 8601 header time.
    pub created: String,
    /// ISO 8601 time of the last user or assistant message.
    pub modified: String,
    pub message_count: usize,
    pub cost: f64,
}

fn common_prefix(a: &str, b: &str) -> usize {
    a.chars().zip(b.chars()).take_while(|(x, y)| x == y).count()
}

/// The prefix length that tells every id in `ids` apart, but at least [`MIN_SHORT_ID`].
pub fn short_id_len<'a>(ids: impl IntoIterator<Item = &'a str>) -> usize {
    let mut ids: Vec<&str> = ids.into_iter().collect();
    ids.sort_unstable();
    ids.windows(2).map(|pair| common_prefix(pair[0], pair[1]) + 1).fold(MIN_SHORT_ID, usize::max)
}

pub fn short_id(id: &str, len: usize) -> String {
    id.chars().take(len).collect()
}

impl ListedSession {
    pub fn new(info: &SessionInfo, short_len: usize) -> Self {
        Self {
            id: info.id.clone(),
            short_id: short_id(&info.id, short_len),
            path: info.path.display().to_string(),
            cwd: info.cwd.clone(),
            name: info.name.clone(),
            first_message: info.first_message.clone(),
            created: format_iso(info.created),
            modified: format_iso(info.modified),
            message_count: info.message_count,
            cost: session_cost(&info.path),
        }
    }
}

/// A one-line summary for confirmation prompts and delete reports: short id, last activity, and title.
pub fn summary_line(info: &SessionInfo, short_len: usize, zone: &TimeZone) -> String {
    let id = short_id(&info.id, short_len);
    format!("{id}  {}  {}", local_time(info.modified, zone), one_line(info.display_name(), TITLE_WIDTH))
}

pub(super) fn list(flags: &Flags, scope: &SessionScope) -> CommandOutput {
    let sessions = scope.sessions(flags.all);
    let short_len = short_id_len(sessions.iter().map(|info| info.id.as_str()));
    let rows: Vec<ListedSession> = sessions
        .iter()
        .take(flags.limit.unwrap_or(usize::MAX))
        .map(|info| ListedSession::new(info, short_len))
        .collect();
    if flags.json {
        let stdout = format!("{}\n", serde_json::to_string_pretty(&rows).unwrap_or_else(|_| "[]".into()));
        return CommandOutput { stdout, ..CommandOutput::default() };
    }
    if rows.is_empty() {
        let place = if flags.all { "" } else { " for this project" };
        return CommandOutput { stdout: format!("No sessions found{place}.\n"), ..CommandOutput::default() };
    }
    let zone = TimeZone::system();
    let mut columns = vec![("ID", false), ("MODIFIED", false), ("MSGS", true), ("COST", true)];
    if flags.all {
        columns.push(("PROJECT", false));
    }
    columns.push(("NAME / FIRST PROMPT", false));
    let cells: Vec<Vec<String>> = rows
        .iter()
        .zip(&sessions)
        .map(|(row, info)| {
            let mut cells = vec![row.short_id.clone(), local_time(info.modified, &zone), row.message_count.to_string()];
            cells.push(format_cost(row.cost));
            if flags.all {
                cells.push(shorten_home(&row.cwd));
            }
            cells.push(one_line(info.display_name(), TITLE_WIDTH));
            cells
        })
        .collect();
    let mut stdout = table(&columns, &cells);
    if rows.len() < sessions.len() {
        stdout.push_str(&format!("({} of {} sessions; --limit)\n", rows.len(), sessions.len()));
    }
    CommandOutput { stdout, ..CommandOutput::default() }
}
