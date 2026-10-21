//! `pi stats`: sessions, prompts, assistant turns, tokens, and cost over saved sessions, by
//! model, by provider, and by day.

use std::collections::{BTreeMap, HashSet};

use jiff::Span;
use jiff::civil::Date;
use jiff::tz::TimeZone;
use serde::Serialize;

use super::format::{format_cost, format_tokens, local_date, local_time, shorten_home, table};
use super::scan::{SessionEvent, scan_session_file};
use super::{Flags, SessionScope};
use crate::cli::CommandOutput;
use crate::session::usage::UsageTotals;
use crate::session::{SessionInfo, format_iso};

/// The day table covers today and the days before it, this many in all.
pub const DAYS: i64 = 14;

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenTotals {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub total: u64,
}

impl TokenTotals {
    fn add(&mut self, usage: &UsageTotals) {
        self.input += usage.input;
        self.output += usage.output;
        self.cache_read += usage.cache_read;
        self.cache_write += usage.cache_write;
        self.total += usage.tokens();
    }
}

/// Totals for one model (`provider/model`) or one provider.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Breakdown {
    pub name: String,
    pub sessions: usize,
    pub assistant_turns: usize,
    pub tokens: TokenTotals,
    pub cost: f64,
}

/// Totals for one local calendar day.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DayStats {
    /// `YYYY-MM-DD`.
    pub date: String,
    pub sessions: usize,
    pub prompts: usize,
    pub assistant_turns: usize,
    pub tokens: TokenTotals,
    pub cost: f64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StatsReport {
    /// `project` or `all`.
    pub scope: String,
    /// ISO 8601 start of the counted period, with `--since`.
    pub since: Option<String>,
    /// Sessions with at least one message or usage entry in the period.
    pub sessions: usize,
    pub prompts: usize,
    pub assistant_turns: usize,
    pub tokens: TokenTotals,
    pub cost: f64,
    /// Most expensive first.
    pub by_model: Vec<Breakdown>,
    pub by_provider: Vec<Breakdown>,
    /// Days of the last [`DAYS`] with activity, oldest first.
    pub by_day: Vec<DayStats>,
}

/// Add `event` to the row for `key`, counting each session once.
fn tally(rows: &mut BTreeMap<String, Breakdown>, seen: &mut HashSet<String>, key: &str, event: &SessionEvent) {
    let row =
        rows.entry(key.to_string()).or_insert_with(|| Breakdown { name: key.to_string(), ..Breakdown::default() });
    if seen.insert(key.to_string()) {
        row.sessions += 1;
    }
    row.assistant_turns += usize::from(event.assistant);
    if let Some(usage) = &event.usage {
        row.tokens.add(&usage.totals);
        row.cost += usage.totals.cost;
    }
}

fn sorted(rows: BTreeMap<String, Breakdown>) -> Vec<Breakdown> {
    let mut rows: Vec<Breakdown> = rows.into_values().collect();
    rows.sort_by(|a, b| b.cost.total_cmp(&a.cost).then(b.tokens.total.cmp(&a.tokens.total)).then(a.name.cmp(&b.name)));
    rows
}

/// Totals over `sessions`. `since` is the start of the period in milliseconds; `now` and
/// `zone` pick the days of the day table.
pub fn collect_stats(
    sessions: &[SessionInfo],
    all: bool,
    since: Option<i64>,
    now: i64,
    zone: &TimeZone,
) -> StatsReport {
    let today = local_date(now, zone);
    let first_day = today.and_then(|today| today.checked_sub(Span::new().days(DAYS - 1)).ok());
    let in_table =
        |date: &Date| first_day.is_some_and(|first| *date >= first) && today.is_some_and(|last| *date <= last);
    let mut report = StatsReport {
        scope: if all { "all" } else { "project" }.into(),
        since: since.map(format_iso),
        ..StatsReport::default()
    };
    let (mut models, mut providers, mut days) = (BTreeMap::new(), BTreeMap::new(), BTreeMap::<Date, DayStats>::new());
    for info in sessions {
        let (mut seen_models, mut seen_providers, mut seen_days) = (HashSet::new(), HashSet::new(), HashSet::new());
        let mut active = false;
        for event in scan_session_file(&info.path) {
            if since.is_some_and(|start| event.time.is_none_or(|time| time < start)) {
                continue;
            }
            active = true;
            report.prompts += usize::from(event.prompt);
            report.assistant_turns += usize::from(event.assistant);
            if let Some(usage) = &event.usage {
                report.tokens.add(&usage.totals);
                report.cost += usage.totals.cost;
                let provider = if usage.provider.is_empty() { "unknown" } else { usage.provider.as_str() };
                tally(&mut models, &mut seen_models, &usage.model, &event);
                tally(&mut providers, &mut seen_providers, provider, &event);
            }
            let Some(date) = event.time.and_then(|time| local_date(time, zone)).filter(in_table) else { continue };
            let day = days.entry(date).or_insert_with(|| DayStats { date: date.to_string(), ..DayStats::default() });
            if seen_days.insert(date) {
                day.sessions += 1;
            }
            day.prompts += usize::from(event.prompt);
            day.assistant_turns += usize::from(event.assistant);
            if let Some(usage) = &event.usage {
                day.tokens.add(&usage.totals);
                day.cost += usage.totals.cost;
            }
        }
        report.sessions += usize::from(active);
    }
    report.by_model = sorted(models);
    report.by_provider = sorted(providers);
    report.by_day = days.into_values().collect();
    report
}

fn indented(text: String) -> String {
    text.lines().map(|line| format!("  {line}\n")).collect()
}

fn breakdown_table(title: &str, rows: &[Breakdown]) -> String {
    let columns = [
        (title, false),
        ("SESSIONS", true),
        ("TURNS", true),
        ("INPUT", true),
        ("OUTPUT", true),
        ("CACHE READ", true),
        ("CACHE WRITE", true),
        ("COST", true),
    ];
    let cells: Vec<Vec<String>> = rows
        .iter()
        .map(|row| {
            let tokens = &row.tokens;
            vec![
                row.name.clone(),
                row.sessions.to_string(),
                row.assistant_turns.to_string(),
                format_tokens(tokens.input),
                format_tokens(tokens.output),
                format_tokens(tokens.cache_read),
                format_tokens(tokens.cache_write),
                format_cost(row.cost),
            ]
        })
        .collect();
    indented(table(&columns, &cells))
}

fn day_table(rows: &[DayStats]) -> String {
    let columns =
        [("DATE", false), ("SESSIONS", true), ("PROMPTS", true), ("TURNS", true), ("TOKENS", true), ("COST", true)];
    let cells: Vec<Vec<String>> = rows
        .iter()
        .map(|day| {
            vec![
                day.date.clone(),
                day.sessions.to_string(),
                day.prompts.to_string(),
                day.assistant_turns.to_string(),
                format_tokens(day.tokens.total),
                format_cost(day.cost),
            ]
        })
        .collect();
    indented(table(&columns, &cells))
}

/// The report as text; `heading` names the sessions it covers.
pub fn render_text(report: &StatsReport, heading: &str) -> String {
    let tokens = &report.tokens;
    let token_line = format!(
        "{} in, {} out, {} cache read, {} cache write",
        format_tokens(tokens.input),
        format_tokens(tokens.output),
        format_tokens(tokens.cache_read),
        format_tokens(tokens.cache_write)
    );
    let mut text = format!("{heading}\n\n");
    for (label, value) in [
        ("Sessions", report.sessions.to_string()),
        ("Prompts", report.prompts.to_string()),
        ("Assistant turns", report.assistant_turns.to_string()),
        ("Tokens", token_line),
        ("Cost", format_cost(report.cost)),
    ] {
        text.push_str(&format!("{label:<17}{value}\n"));
    }
    if !report.by_model.is_empty() {
        text.push_str(&format!("\nBy model\n{}", breakdown_table("MODEL", &report.by_model)));
        text.push_str(&format!("\nBy provider\n{}", breakdown_table("PROVIDER", &report.by_provider)));
    }
    if !report.by_day.is_empty() {
        text.push_str(&format!("\nLast {DAYS} days\n{}", day_table(&report.by_day)));
    }
    text
}

pub(super) fn stats(flags: &Flags, scope: &SessionScope) -> CommandOutput {
    let (now, zone) = (pi_ai::now_ms(), TimeZone::system());
    let since = flags.since.map(|age| now.saturating_sub(age));
    let report = collect_stats(&scope.sessions(flags.all), flags.all, since, now, &zone);
    let stdout = if flags.json {
        format!("{}\n", serde_json::to_string_pretty(&report).unwrap_or_default())
    } else {
        let cwd = shorten_home(&scope.cwd.display().to_string());
        let dir = scope.dir_override.as_ref().map(|dir| shorten_home(&dir.display().to_string()));
        let mut heading = match (dir, flags.all) {
            (None, true) => "All projects".to_string(),
            (Some(dir), true) => format!("All sessions in {dir}"),
            (None, false) => format!("Project {cwd}"),
            (Some(dir), false) => format!("Project {cwd} (sessions in {dir})"),
        };
        if let Some(start) = since {
            heading.push_str(&format!(", since {}", local_time(start, &zone)));
        }
        render_text(&report, &heading)
    };
    CommandOutput { stdout, ..CommandOutput::default() }
}
