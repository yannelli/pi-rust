//! Gantt chart grammar. Tasks resolve `after` and `until` references in any order;
//! an unknown reference, a bad date or a cycle rejects the chart.

use std::collections::HashMap;

use super::dates::{parse_date, parse_duration, parse_interval, weekday, weekday_named};
use super::labels::clean_label;
use super::parse_flow::{header_kind, statements_of};
use super::strings::{first_word, trim, words};

/// Charts with more tasks than this are refused.
const MAX_TASKS: usize = 512;
/// Bound on the days an `excludes` rule may push one task's end.
const MAX_EXCLUDED_STEPS: usize = 10_000;

#[derive(Clone, Copy, Default)]
pub(super) struct TaskFlags {
    pub done: bool,
    pub active: bool,
    pub crit: bool,
    pub milestone: bool,
}

pub(super) struct Task {
    pub name: String,
    pub section: Option<usize>,
    pub start: f64,
    pub end: f64,
    pub flags: TaskFlags,
}

pub(super) struct Gantt {
    pub title: Option<String>,
    pub sections: Vec<String>,
    pub tasks: Vec<Task>,
    pub axis_format: Option<String>,
    pub tick_interval: Option<f64>,
    /// False when no task names a date, so the axis counts days from the start.
    pub dated: bool,
}

enum StartSpec {
    Previous,
    At(f64),
    After(Vec<String>),
}

enum EndSpec {
    At(f64),
    Duration(f64),
    Until(Vec<String>),
}

struct Spec {
    id: Option<String>,
    start: StartSpec,
    end: EndSpec,
}

/// Days an `excludes` rule skips.
#[derive(Default)]
struct Excludes {
    weekdays: [bool; 7],
    days: Vec<i64>,
}

impl Excludes {
    fn is_empty(&self) -> bool {
        !self.weekdays.contains(&true) && self.days.is_empty()
    }

    fn skips(&self, day: i64) -> bool {
        self.weekdays[weekday(day)] || self.days.contains(&day)
    }

    /// Push `end` one day later for every excluded day in `start..end` (Mermaid's
    /// `fixTaskDates`).
    fn extend(&self, start: f64, mut end: f64) -> f64 {
        let mut cur = start;
        for _ in 0..MAX_EXCLUDED_STEPS {
            if cur >= end {
                break;
            }
            if self.skips(cur.floor() as i64) {
                end += 1.0;
            }
            cur += 1.0;
        }
        end
    }
}

pub(super) fn parse_gantt(src: &str) -> Option<Gantt> {
    let statements = statements_of(src);
    if header_kind(&statements)? != "gantt" {
        return None;
    }
    let mut gantt = Gantt {
        title: None,
        sections: Vec::new(),
        tasks: Vec::new(),
        axis_format: None,
        tick_interval: None,
        dated: false,
    };
    let mut specs: Vec<Spec> = Vec::new();
    let mut date_format = String::from("YYYY-MM-DD");
    let mut excludes_raw: Vec<String> = Vec::new();
    let mut inclusive_end = false;

    for st in &statements[1..] {
        let keyword = first_word(st);
        let rest = trim(&st[keyword.len()..]);
        match keyword.to_ascii_lowercase().as_str() {
            "title" => gantt.title = Some(clean_label(rest)).filter(|t| !t.is_empty()),
            "section" => gantt.sections.push(clean_label(rest)),
            "dateformat" => date_format = rest.to_string(),
            "axisformat" => gantt.axis_format = Some(rest.to_string()),
            "tickinterval" => gantt.tick_interval = Some(parse_interval(rest)?),
            "excludes" => excludes_raw.push(rest.to_string()),
            "inclusiveenddates" => inclusive_end = true,
            "includes" | "todaymarker" | "topaxis" | "weekday" | "weekend" | "displaymode" | "click" => {}
            k if k.starts_with("acctitle") || k.starts_with("accdescr") => {}
            _ => {
                if gantt.tasks.len() >= MAX_TASKS {
                    return None;
                }
                let (name, meta) = st.split_once(':')?;
                let name = clean_label(name);
                if name.is_empty() {
                    return None;
                }
                let (flags, spec) = parse_task_meta(meta, &date_format, inclusive_end)?;
                gantt.dated |= matches!(spec.start, StartSpec::At(_));
                let section = gantt.sections.len().checked_sub(1);
                gantt.tasks.push(Task { name, section, start: 0.0, end: 0.0, flags });
                specs.push(spec);
            }
        }
    }
    if gantt.tasks.is_empty() {
        return None;
    }

    let mut excludes = Excludes::default();
    for word in excludes_raw.iter().flat_map(|raw| raw.split(',').flat_map(words)) {
        if word.eq_ignore_ascii_case("weekends") {
            excludes.weekdays[0] = true;
            excludes.weekdays[6] = true;
        } else if let Some(day) = weekday_named(word) {
            excludes.weekdays[day] = true;
        } else {
            excludes.days.push(parse_date(word, &date_format)?.floor() as i64);
        }
    }
    resolve(&mut gantt.tasks, &specs, &excludes)?;
    Some(gantt)
}

/// Read `[tags,] [[id,] start,] end` after a task's colon.
fn parse_task_meta(meta: &str, date_format: &str, inclusive_end: bool) -> Option<(TaskFlags, Spec)> {
    let mut items: Vec<&str> = meta.split(',').map(trim).collect();
    let mut flags = TaskFlags::default();
    while let Some(first) = items.first() {
        match first.to_ascii_lowercase().as_str() {
            "done" => flags.done = true,
            "active" => flags.active = true,
            "crit" => flags.crit = true,
            "milestone" => flags.milestone = true,
            _ => break,
        }
        items.remove(0);
    }
    let (id, start, end) = match items.as_slice() {
        [end] => (None, StartSpec::Previous, *end),
        [start, end] => (None, parse_start(start, date_format)?, *end),
        [id, start, end] if !id.is_empty() => (Some(id.to_string()), parse_start(start, date_format)?, *end),
        _ => return None,
    };
    let end = parse_end(end, date_format, inclusive_end)?;
    Some((flags, Spec { id, start, end }))
}

/// Task ids after a reference keyword such as `after`, if `item` starts with it.
fn references(item: &str, keyword: &str) -> Option<Vec<String>> {
    let ws = words(item);
    match ws.split_first() {
        Some((head, ids)) if head.eq_ignore_ascii_case(keyword) && !ids.is_empty() => {
            Some(ids.iter().map(|id| id.to_string()).collect())
        }
        _ => None,
    }
}

fn parse_start(item: &str, date_format: &str) -> Option<StartSpec> {
    if let Some(ids) = references(item, "after") {
        return Some(StartSpec::After(ids));
    }
    parse_date(item, date_format).map(StartSpec::At)
}

fn parse_end(item: &str, date_format: &str, inclusive_end: bool) -> Option<EndSpec> {
    if let Some(ids) = references(item, "until") {
        return Some(EndSpec::Until(ids));
    }
    if let Some(t) = parse_date(item, date_format) {
        return Some(EndSpec::At(if inclusive_end { t + 1.0 } else { t }));
    }
    parse_duration(item).map(EndSpec::Duration)
}

/// Fill in every task's start and end, repeating passes until references settle.
fn resolve(tasks: &mut [Task], specs: &[Spec], excludes: &Excludes) -> Option<()> {
    let ids: HashMap<&str, usize> =
        specs.iter().enumerate().filter_map(|(i, spec)| Some((spec.id.as_deref()?, i))).collect();
    let lookup = |list: &[String]| list.iter().map(|id| ids.get(id.as_str()).copied()).collect::<Option<Vec<_>>>();
    let mut times: Vec<Option<(f64, f64)>> = vec![None; specs.len()];
    let mut pending = specs.len();
    while pending > 0 {
        let before = pending;
        for i in 0..specs.len() {
            if times[i].is_some() {
                continue;
            }
            let start = match &specs[i].start {
                StartSpec::At(t) => Some(*t),
                StartSpec::Previous if i == 0 => Some(0.0),
                StartSpec::Previous => times[i - 1].map(|(_, end)| end),
                StartSpec::After(list) => latest(lookup(list)?.iter().map(|&k| times[k].map(|(_, end)| end))),
            };
            let Some(start) = start else { continue };
            let end = match &specs[i].end {
                EndSpec::At(t) => Some(*t),
                EndSpec::Duration(days) if excludes.is_empty() => Some(start + days),
                EndSpec::Duration(days) => Some(excludes.extend(start, start + days)),
                EndSpec::Until(list) => earliest(lookup(list)?.iter().map(|&k| times[k].map(|(start, _)| start))),
            };
            if let Some(end) = end {
                times[i] = Some((start, end.max(start)));
                pending -= 1;
            }
        }
        if pending == before {
            return None;
        }
    }
    for (task, time) in tasks.iter_mut().zip(times) {
        (task.start, task.end) = time?;
    }
    Some(())
}

/// The largest value, or `None` while any is unresolved.
fn latest(mut values: impl Iterator<Item = Option<f64>>) -> Option<f64> {
    values.try_fold(f64::NEG_INFINITY, |acc, v| v.map(|v| acc.max(v)))
}

fn earliest(mut values: impl Iterator<Item = Option<f64>>) -> Option<f64> {
    values.try_fold(f64::INFINITY, |acc, v| v.map(|v| acc.min(v)))
}
