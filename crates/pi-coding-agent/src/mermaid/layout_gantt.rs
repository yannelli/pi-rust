//! Gantt chart layout: task names in a left column, bars on a shared time axis.
//! Spans of up to `CHART_W` days get whole columns per day; longer ones compress.

use super::canvas::{Canvas, draw_text};
use super::dates::format_date;
use super::labels::{MAX_LABEL, WRAP_WIDTH, fit_label};
use super::layout::MAX_CANVAS_CELLS;
use super::parse_gantt::{Gantt, Task};
use super::types::Cls;
use super::width::string_width;

/// Target columns for the time axis.
const CHART_W: f64 = 48.0;
/// Candidate tick intervals in days, from one minute to a year.
const NICE_TICKS: [f64; 16] = [
    1.0 / 1440.0,
    5.0 / 1440.0,
    15.0 / 1440.0,
    30.0 / 1440.0,
    1.0 / 24.0,
    3.0 / 24.0,
    6.0 / 24.0,
    12.0 / 24.0,
    1.0,
    2.0,
    7.0,
    14.0,
    28.0,
    91.0,
    182.0,
    365.0,
];
const MAX_TICKS: usize = 256;
const SECTION_INDENT: usize = 2;

/// Bar glyph and class: done is muted, crit is emphasised.
fn bar_style(task: &Task) -> (char, Cls) {
    let flags = task.flags;
    let glyph = if flags.done {
        '░'
    } else if flags.crit {
        '▓'
    } else if flags.active {
        '▒'
    } else {
        '█'
    };
    let cls = if flags.crit {
        Cls::Title
    } else if flags.done {
        Cls::EdgeLabel
    } else {
        Cls::Edge
    };
    (glyph, cls)
}

/// An axis label `dt` days after the start, for charts without dates.
fn relative_label(dt: f64, interval: f64) -> String {
    if interval >= 1.0 {
        format!("{}d", dt.round())
    } else if interval >= 1.0 / 24.0 {
        format!("{}h", (dt * 24.0).round())
    } else {
        format!("{}m", (dt * 1440.0).round())
    }
}

pub(super) fn layout_gantt(gantt: &Gantt) -> Option<Canvas> {
    let t0 = gantt.tasks.iter().map(|t| t.start).fold(f64::INFINITY, f64::min);
    let t1 = gantt.tasks.iter().map(|t| t.end).fold(t0, f64::max);
    let span = t1 - t0;
    let scale = match CHART_W / span {
        s if !s.is_finite() => 1.0,
        s if span >= 1.0 && s >= 1.0 => s.floor(),
        s => s,
    };
    let col = |t: f64| ((t - t0) * scale).round() as usize;
    let chart_w = col(t1).max(1);

    let names: Vec<String> = gantt.tasks.iter().map(|t| fit_label(&t.name, WRAP_WIDTH)).collect();
    let sections: Vec<String> = gantt.sections.iter().map(|s| fit_label(s, WRAP_WIDTH)).collect();
    let indent = |task: &Task| if task.section.is_some() { SECTION_INDENT } else { 0 };
    let mut label_w = 0;
    for (task, name) in gantt.tasks.iter().zip(&names) {
        label_w = label_w.max(indent(task) + string_width(name));
        if let Some(s) = task.section {
            label_w = label_w.max(string_width(&sections[s]));
        }
    }
    let x0 = label_w + 2;

    let label_at = |t: f64, interval: f64| match (&gantt.axis_format, gantt.dated) {
        (_, false) => relative_label(t - t0, interval),
        (Some(format), true) => format_date(t, format),
        (None, true) => format_date(t, if span < 2.0 { "%H:%M" } else { "%m-%d" }),
    };
    let base = gantt.tick_interval.unwrap_or_else(|| {
        let min_gap = string_width(&label_at(t1, 1.0)) as f64 + 2.0;
        NICE_TICKS.into_iter().find(|iv| iv * scale >= min_gap).unwrap_or(365.0)
    });
    let min_gap = string_width(&label_at(t1, base)).max(string_width(&label_at(t0, base))) as f64 + 2.0;
    let interval = base * (min_gap / (base * scale)).ceil().max(1.0);

    let mut ticks: Vec<(usize, String)> = Vec::new();
    let mut next_free = 0;
    for k in 0..MAX_TICKS {
        let t = t0 + k as f64 * interval;
        if t > t1 + 1e-9 {
            break;
        }
        let x = x0 + col(t);
        if x >= next_free {
            let label = label_at(t, interval);
            next_free = x + string_width(&label) + 1;
            ticks.push((x, label));
        }
    }

    let title = gantt.title.as_deref().map(|t| fit_label(t, 2 * MAX_LABEL));
    let axis_y = if title.is_some() { 2 } else { 0 };
    let mut rows = 0;
    let mut section = None;
    for task in &gantt.tasks {
        if task.section.is_some() && task.section != section {
            rows += 1;
        }
        section = task.section;
        rows += 1;
    }
    let canvas_h = axis_y + 2 + rows;
    let canvas_w = (x0 + chart_w + 1).max(next_free).max(title.as_deref().map_or(0, string_width));
    if canvas_w * canvas_h > MAX_CANVAS_CELLS {
        return None;
    }

    let mut canvas = Canvas::new(canvas_w, canvas_h);
    if let Some(title) = &title {
        draw_text(&mut canvas, title, 0, 0, Cls::Title);
    }
    for x in x0..=x0 + chart_w {
        canvas.set_char(x, axis_y + 1, '─', Cls::Border);
    }
    for (x, label) in &ticks {
        draw_text(&mut canvas, label, *x, axis_y, Cls::EdgeLabel);
        canvas.set_char(*x, axis_y + 1, '┬', Cls::Border);
    }

    let mut y = axis_y + 2;
    let mut section = None;
    for (task, name) in gantt.tasks.iter().zip(&names) {
        if let Some(s) = task.section.filter(|_| task.section != section) {
            draw_text(&mut canvas, &sections[s], 0, y, Cls::Title);
            y += 1;
        }
        section = task.section;
        draw_text(&mut canvas, name, indent(task), y, Cls::Text);
        if task.flags.milestone {
            canvas.set_char(x0 + col((task.start + task.end) / 2.0), y, '◆', Cls::Title);
        } else {
            let (glyph, cls) = bar_style(task);
            let (a, b) = (col(task.start), col(task.end));
            for x in a..b.max(a + 1) {
                canvas.set_char(x0 + x, y, glyph, cls);
            }
        }
        y += 1;
    }
    Some(canvas)
}
