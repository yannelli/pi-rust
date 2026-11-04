//! Shared row fitting and line-based viewports for selectable lists.

use std::cell::Cell;
use std::ops::Range;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::Component;
use crate::ui_loop::{self, TimerId};
use crate::utils::{slice_by_column, truncate_to_width_with, visible_width, wrap_text_with_ansi};

const SCROLL_TICK: Duration = Duration::from_millis(100);
const SCROLL_PAUSE_TICKS: usize = 10;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ListOverflow {
    #[default]
    Truncate,
    Wrap,
    Scroll,
}

thread_local! {
    static MODE: Cell<ListOverflow> = const { Cell::new(ListOverflow::Truncate) };
    static SCROLL_NEEDED: Cell<bool> = const { Cell::new(false) };
    static FRAME: Cell<u64> = const { Cell::new(0) };
}

pub fn set_list_overflow(mode: ListOverflow) {
    MODE.set(mode);
}

pub fn list_overflow() -> ListOverflow {
    MODE.get()
}

pub fn reduced_motion() -> bool {
    std::env::var("PI_REDUCED_MOTION").is_ok_and(|value| value == "1")
}

/// The offset advances by a terminal column each tick, pauses at the end, and starts again.
pub fn selected_row_scroll_offset(overflow: usize, elapsed: Duration) -> usize {
    if overflow == 0 {
        return 0;
    }
    let tick = (elapsed.as_millis() / SCROLL_TICK.as_millis()) as usize;
    let phase = tick % (SCROLL_PAUSE_TICKS * 2 + overflow);
    phase.saturating_sub(SCROLL_PAUSE_TICKS).min(overflow)
}

/// Fits ANSI-styled text after a fixed prefix. Continuations align under the text.
pub fn fit_list_row(
    text: &str,
    prefix: &str,
    width: usize,
    mode: ListOverflow,
    scroll_offset: usize,
    selected: bool,
    reduced_motion: bool,
) -> Vec<String> {
    let prefix = truncate_to_width_with(prefix, width, "…", false);
    let indent = visible_width(&prefix);
    let available = width.saturating_sub(indent);
    if available == 0 {
        return vec![prefix];
    }
    let text = text.replace(['\r', '\n'], " ");
    let parts = match mode {
        ListOverflow::Wrap => wrap_text_with_ansi(&text, available),
        ListOverflow::Scroll if selected && !reduced_motion && visible_width(&text) > available => {
            let offset = scroll_offset.min(visible_width(&text).saturating_sub(available));
            vec![format!("{}\x1b[0m", slice_by_column(&text, offset, available, true))]
        }
        _ => vec![truncate_to_width_with(&text, available, "…", false)],
    };
    let continuation = " ".repeat(indent);
    parts
        .into_iter()
        .enumerate()
        .map(|(index, part)| {
            let part = truncate_to_width_with(&part, available, "…", false);
            format!("{}{part}", if index == 0 { &prefix } else { &continuation })
        })
        .collect()
}

pub struct ListRow {
    pub prefix: String,
    pub text: String,
    /// An existing column layout for truncate mode, including its prefix.
    pub truncated: Option<String>,
}

impl ListRow {
    pub fn new(prefix: impl Into<String>, text: impl Into<String>) -> Self {
        Self { prefix: prefix.into(), text: text.into(), truncated: None }
    }

    pub fn with_truncated(mut self, line: String) -> Self {
        self.truncated = Some(line);
        self
    }
}

#[derive(Default)]
pub struct RenderedList {
    pub lines: Vec<String>,
    /// Item index for each rendered terminal line, for mouse hit testing.
    pub row_indices: Vec<usize>,
    pub range: Range<usize>,
}

#[derive(Default)]
pub struct ListOverflowState {
    selection: Option<(usize, String, usize, ListOverflow, bool)>,
    started_at: Option<Instant>,
    last_frame: u64,
}

impl ListOverflowState {
    pub fn render(&mut self, rows: &[ListRow], selected: usize, max_lines: usize, width: usize) -> RenderedList {
        self.render_at(rows, selected, max_lines, width, Instant::now())
    }

    fn render_at(
        &mut self,
        rows: &[ListRow],
        selected: usize,
        max_lines: usize,
        width: usize,
        now: Instant,
    ) -> RenderedList {
        let mode = list_overflow();
        let reduced = reduced_motion();
        let frame = FRAME.get();
        if let Some(row) = rows.get(selected) {
            let selection =
                (selected, row.text.clone(), width.saturating_sub(visible_width(&row.prefix)), mode, reduced);
            if self.selection.as_ref() != Some(&selection) || frame.saturating_sub(self.last_frame) > 1 {
                self.selection = Some(selection);
                self.started_at = Some(now);
            }
        }
        self.last_frame = frame;
        let elapsed = self.started_at.map_or(Duration::ZERO, |start| now.saturating_duration_since(start));
        let fitted: Vec<Vec<String>> = rows
            .iter()
            .enumerate()
            .map(|(index, row)| {
                let is_selected = index == selected;
                let available = width.saturating_sub(visible_width(&row.prefix));
                let overflow = visible_width(&row.text).saturating_sub(available);
                if (mode == ListOverflow::Truncate || (mode == ListOverflow::Scroll && (!is_selected || reduced)))
                    && let Some(line) = &row.truncated
                {
                    return vec![truncate_to_width_with(line, width, "…", false)];
                }
                fit_list_row(
                    &row.text,
                    &row.prefix,
                    width,
                    mode,
                    selected_row_scroll_offset(overflow, elapsed),
                    is_selected,
                    reduced,
                )
            })
            .collect();
        let heights: Vec<usize> = fitted.iter().map(Vec::len).collect();
        let range = visible_list_range(&heights, selected, max_lines);
        if mode == ListOverflow::Scroll
            && !reduced
            && range.contains(&selected)
            && let Some(row) = rows.get(selected)
        {
            let available = width.saturating_sub(visible_width(&row.prefix));
            if available > 0 && visible_width(&row.text) > available {
                SCROLL_NEEDED.set(true);
            }
        }
        let mut rendered = RenderedList { range: range.clone(), ..RenderedList::default() };
        for index in range {
            rendered.row_indices.extend(std::iter::repeat_n(index, fitted[index].len()));
            rendered.lines.extend(fitted[index].iter().cloned());
        }
        rendered
    }
}

/// Centers a line budget on the selected item without cutting any item's continuation lines.
pub fn visible_list_range(heights: &[usize], selected: usize, max_lines: usize) -> Range<usize> {
    if heights.is_empty() || max_lines == 0 {
        return 0..0;
    }
    let selected = selected.min(heights.len() - 1);
    let mut start = selected;
    let mut end = selected + 1;
    let mut used = heights[selected];
    let mut before = 0;
    while start > 0 && before + heights[start - 1] <= max_lines / 2 && used + heights[start - 1] <= max_lines {
        start -= 1;
        before += heights[start];
        used += heights[start];
    }
    while end < heights.len() && used + heights[end] <= max_lines {
        used += heights[end];
        end += 1;
    }
    while start > 0 && used + heights[start - 1] <= max_lines {
        start -= 1;
        used += heights[start];
    }
    start..end
}

/// A row collection for selectors that rebuild their children after navigation.
#[derive(Default)]
pub struct ListRows {
    pub rows: Vec<ListRow>,
    pub selected: usize,
    pub max_lines: usize,
    state: ListOverflowState,
}

impl ListRows {
    pub fn new(rows: Vec<ListRow>, selected: usize, max_lines: usize) -> Self {
        Self { rows, selected, max_lines, state: ListOverflowState::default() }
    }
}

impl Component for ListRows {
    fn render(&mut self, width: usize) -> Vec<String> {
        let rendered = self.state.render(&self.rows, self.selected, self.max_lines, width);
        let mut lines = rendered.lines;
        if rendered.range.start > 0 || rendered.range.end < self.rows.len() {
            lines.push(truncate_to_width_with(
                &format!("  ({}/{})", self.selected + 1, self.rows.len()),
                width,
                "…",
                false,
            ));
        }
        lines
    }
}

#[derive(Default)]
pub(crate) struct ListAnimation {
    timer: Option<TimerId>,
}

impl ListAnimation {
    pub fn begin_frame() {
        SCROLL_NEEDED.set(false);
        FRAME.set(FRAME.get().wrapping_add(1));
    }

    pub fn end_frame(&mut self, request_render: impl FnMut() + 'static) {
        if SCROLL_NEEDED.get() {
            if self.timer.is_none() {
                self.timer = Some(ui_loop::set_interval(SCROLL_TICK, request_render));
            }
        } else {
            self.stop();
        }
    }

    pub fn stop(&mut self) {
        if let Some(timer) = self.timer.take() {
            ui_loop::clear_timer(timer);
        }
    }
}

impl Drop for ListAnimation {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::strip_terminal_sequences;

    #[test]
    fn scroll_offsets_pause_advance_and_repeat() {
        let offset = |millis| selected_row_scroll_offset(3, Duration::from_millis(millis));
        assert_eq!(offset(0), 0);
        assert_eq!(offset(999), 0);
        assert_eq!(offset(1100), 1);
        assert_eq!(offset(1200), 2);
        assert_eq!(offset(1300), 3);
        assert_eq!(offset(2200), 3);
        assert_eq!(offset(2300), 0);
        assert_eq!(offset(3400), 1);
        assert_eq!(selected_row_scroll_offset(0, Duration::MAX), 0);
    }

    #[test]
    fn scrolling_changes_the_selected_row_over_render_ticks() {
        let previous = list_overflow();
        set_list_overflow(ListOverflow::Scroll);
        let rows = [ListRow::new("> ", "abcdefghijk"), ListRow::new("  ", "ABCDEFGHIJK")];
        let mut state = ListOverflowState::default();
        let start = Instant::now();
        let first = state.render_at(&rows, 0, 4, 6, start);
        let next = state.render_at(&rows, 0, 4, 6, start + Duration::from_millis(1200));
        let changed = state.render_at(&rows, 1, 4, 6, start + Duration::from_millis(1300));
        set_list_overflow(previous);
        if reduced_motion() {
            assert_eq!(first.lines, next.lines);
        } else {
            assert_eq!(strip_terminal_sequences(&first.lines[0]), "> abcd");
            assert_eq!(strip_terminal_sequences(&next.lines[0]), "> cdef");
            assert_eq!(strip_terminal_sequences(&changed.lines[1]), "  ABCD");
        }
        assert_eq!(strip_terminal_sequences(&first.lines[1]), "  ABC…");
    }

    #[test]
    fn reduced_motion_and_unselected_rows_truncate() {
        let fit = |selected, reduced| fit_list_row("abcdefgh", "> ", 6, ListOverflow::Scroll, 3, selected, reduced);
        assert_eq!(strip_terminal_sequences(&fit(true, true)[0]), "> abc…");
        assert_eq!(strip_terminal_sequences(&fit(false, false)[0]), "> abc…");
        assert_eq!(strip_terminal_sequences(&fit(true, false)[0]), "> defg");
    }

    #[test]
    fn fitting_preserves_ansi_and_wide_graphemes_within_width() {
        for mode in [ListOverflow::Truncate, ListOverflow::Wrap, ListOverflow::Scroll] {
            for width in 0..9 {
                let lines = fit_list_row("\x1b[31m界界界e\u{301}abc\x1b[0m", "> ", width, mode, 1, true, false);
                assert!(!lines.is_empty());
                assert!(lines.iter().all(|line| visible_width(line) <= width), "{mode:?}: {lines:?}");
            }
        }
    }

    #[test]
    fn wrapped_viewport_keeps_whole_selected_items() {
        assert_eq!(visible_list_range(&[2, 2, 2, 2, 2], 3, 5), 2..4);
        assert_eq!(visible_list_range(&[1, 7, 1], 1, 4), 1..2);
        assert_eq!(visible_list_range(&[], 0, 5), 0..0);
        assert_eq!(visible_list_range(&[1], 0, 0), 0..0);
    }

    #[test]
    fn animation_timer_runs_while_an_overflowing_selection_is_rendered() {
        let previous = list_overflow();
        set_list_overflow(ListOverflow::Scroll);
        let mut state = ListOverflowState::default();
        let mut animation = ListAnimation::default();
        ListAnimation::begin_frame();
        state.render(&[ListRow::new("> ", "abcdefgh")], 0, 5, 6);
        animation.end_frame(|| {});
        let timer = animation.timer;
        assert_eq!(timer.is_some(), !reduced_motion());
        ListAnimation::begin_frame();
        state.render(&[ListRow::new("> ", "fits")], 0, 5, 6);
        animation.end_frame(|| {});
        assert!(animation.timer.is_none());
        if let Some(timer) = timer {
            assert!(!ui_loop::timer_active(timer));
        }
        ListAnimation::begin_frame();
        state.render(&[ListRow::new("> ", "abcdefgh")], 0, 5, 6);
        animation.end_frame(|| {});
        ListAnimation::begin_frame();
        animation.end_frame(|| {});
        assert!(animation.timer.is_none());
        set_list_overflow(previous);
    }
}
