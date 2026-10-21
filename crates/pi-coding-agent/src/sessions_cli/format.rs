//! Parsing and formatting shared by `pi sessions` and `pi stats`.

use jiff::Timestamp;
use jiff::civil::Date;
use jiff::tz::TimeZone;
use pi_tui::{truncate_to_width, visible_width};

const SECOND: i64 = 1_000;
const MINUTE: i64 = 60 * SECOND;
const HOUR: i64 = 60 * MINUTE;
const DAY: i64 = 24 * HOUR;

/// `30d`, `12h`, `90m`, `2w`, `45s`, or a sequence such as `1d12h`, in milliseconds.
pub fn parse_duration(text: &str) -> Result<i64, String> {
    let invalid =
        || format!("Invalid duration \"{text}\": use a number and a unit (s, m, h, d, w), such as 30d or 12h");
    let (mut total, mut digits, mut parts) = (0i64, String::new(), 0);
    for c in text.trim().chars() {
        if c.is_ascii_digit() {
            digits.push(c);
            continue;
        }
        let unit = match c {
            's' => SECOND,
            'm' => MINUTE,
            'h' => HOUR,
            'd' => DAY,
            'w' => 7 * DAY,
            _ => return Err(invalid()),
        };
        let value: i64 = digits.parse().map_err(|_| invalid())?;
        total = value.checked_mul(unit).and_then(|millis| total.checked_add(millis)).ok_or_else(invalid)?;
        digits.clear();
        parts += 1;
    }
    if parts == 0 || !digits.is_empty() {
        return Err(invalid());
    }
    Ok(total)
}

/// `YYYY-MM-DD HH:MM` in `zone`.
pub fn local_time(millis: i64, zone: &TimeZone) -> String {
    Timestamp::from_millisecond(millis)
        .map(|time| time.to_zoned(zone.clone()).strftime("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_default()
}

/// The calendar day of `millis` in `zone`.
pub fn local_date(millis: i64, zone: &TimeZone) -> Option<Date> {
    Timestamp::from_millisecond(millis).ok().map(|time| time.to_zoned(zone.clone()).date())
}

/// Dollars: cents from one cent up, four decimals below.
pub fn format_cost(cost: f64) -> String {
    if cost > 0.0 && cost < 0.01 { format!("${cost:.4}") } else { format!("${cost:.2}") }
}

pub fn format_bytes(bytes: u64) -> String {
    crate::tools::truncate::format_size(usize::try_from(bytes).unwrap_or(usize::MAX))
}

pub use crate::cache_stats::format_tokens;

/// `text` on one line without control characters, cut to `width` cells.
pub fn one_line(text: &str, width: usize) -> String {
    let clean: String = text.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    truncate_to_width(&clean.split_whitespace().collect::<Vec<_>>().join(" "), width)
}

/// `path` with the home directory as `~`.
pub fn shorten_home(path: &str) -> String {
    let home = crate::config::home_dir().display().to_string();
    match path.strip_prefix(home.as_str()) {
        Some(rest) if home.len() > 1 && (rest.is_empty() || rest.starts_with(['/', '\\'])) => format!("~{rest}"),
        _ => path.to_string(),
    }
}

/// Aligned columns two spaces apart under a header row; `(title, right_aligned)` per column.
pub fn table(columns: &[(&str, bool)], rows: &[Vec<String>]) -> String {
    let widths: Vec<usize> = (0..columns.len())
        .map(|index| {
            let cells = rows.iter().map(|row| visible_width(&row[index]));
            cells.chain([visible_width(columns[index].0)]).max().unwrap_or(0)
        })
        .collect();
    let line = |cells: Vec<&str>| {
        let parts: Vec<String> = cells
            .iter()
            .enumerate()
            .map(|(index, cell)| {
                let pad = " ".repeat(widths[index].saturating_sub(visible_width(cell)));
                if columns[index].1 { format!("{pad}{cell}") } else { format!("{cell}{pad}") }
            })
            .collect();
        format!("{}\n", parts.join("  ").trim_end())
    };
    let mut text = line(columns.iter().map(|(title, _)| *title).collect());
    for row in rows {
        text.push_str(&line(row.iter().map(String::as_str).collect()));
    }
    text
}

/// `count` followed by `noun`, with an `s` unless the count is one.
pub fn plural(count: usize, noun: &str) -> String {
    if count == 1 { format!("1 {noun}") } else { format!("{count} {noun}s") }
}
