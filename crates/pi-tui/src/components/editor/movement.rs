//! Cursor movement: visual lines, the sticky column, paging, words, and character jumps.

use super::segment::{is_paste_marker, word_segments_with_markers};
use super::state::{EditorCore, JumpDirection};
use super::wrap::word_wrap_line_with_segments;
use crate::utils::visible_width;
use crate::word_navigation::{WordNavigationOptions, find_word_backward, find_word_forward};

/// One visual (wrapped) line: its logical line and byte range.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct VisualLine {
    pub logical_line: usize,
    pub start_col: usize,
    pub length: usize,
}

fn is_last_segment(visual_lines: &[VisualLine], index: usize) -> bool {
    visual_lines.get(index + 1).is_none_or(|next| next.logical_line != visual_lines[index].logical_line)
}

impl EditorCore {
    pub fn build_visual_line_map(&self, width: usize) -> Vec<VisualLine> {
        let mut visual_lines = Vec::new();
        for (index, line) in self.state.lines.iter().enumerate() {
            if line.is_empty() {
                visual_lines.push(VisualLine { logical_line: index, start_col: 0, length: 0 });
            } else if visible_width(line) <= width {
                visual_lines.push(VisualLine { logical_line: index, start_col: 0, length: line.len() });
            } else {
                let segments = self.graphemes(line);
                for chunk in word_wrap_line_with_segments(line, width, Some(&segments)) {
                    visual_lines.push(VisualLine {
                        logical_line: index,
                        start_col: chunk.start_index,
                        length: chunk.end_index - chunk.start_index,
                    });
                }
            }
        }
        visual_lines
    }

    /// The visual line holding `col` of logical `line`; the end of a line belongs to its last segment.
    pub fn find_visual_line_at(visual_lines: &[VisualLine], line: usize, col: usize) -> usize {
        for (index, visual) in visual_lines.iter().enumerate() {
            if visual.logical_line != line || col < visual.start_col {
                continue;
            }
            let offset = col - visual.start_col;
            if offset < visual.length || (is_last_segment(visual_lines, index) && offset == visual.length) {
                return index;
            }
        }
        visual_lines.len().saturating_sub(1)
    }

    pub fn find_current_visual_line(&self, visual_lines: &[VisualLine]) -> usize {
        Self::find_visual_line_at(visual_lines, self.state.cursor_line, self.state.cursor_col)
    }

    pub fn is_on_first_visual_line(&self) -> bool {
        let visual_lines = self.build_visual_line_map(self.last_width);
        self.find_current_visual_line(&visual_lines) == 0
    }

    pub fn is_on_last_visual_line(&self) -> bool {
        let visual_lines = self.build_visual_line_map(self.last_width);
        self.find_current_visual_line(&visual_lines) + 1 == visual_lines.len()
    }

    /// Moves to `target` keeping the sticky column, and snaps out of the middle of paste markers.
    fn move_to_visual_line(&mut self, visual_lines: &[VisualLine], current: usize, target: usize) {
        let (Some(&current_vl), Some(&target_vl)) = (visual_lines.get(current), visual_lines.get(target)) else {
            return;
        };

        // A cursor snapped to a marker start resolves its old column against the line it is on.
        let current_visual_col = match self.snapped_from_cursor_col {
            Some(snapped) => {
                let index = Self::find_visual_line_at(visual_lines, current_vl.logical_line, snapped);
                snapped.saturating_sub(visual_lines[index].start_col)
            }
            None => self.state.cursor_col.saturating_sub(current_vl.start_col),
        };

        // Non-last segments end one column early so the cursor stays in the segment.
        let source_max = if is_last_segment(visual_lines, current) {
            current_vl.length
        } else {
            current_vl.length.saturating_sub(1)
        };
        let target_max =
            if is_last_segment(visual_lines, target) { target_vl.length } else { target_vl.length.saturating_sub(1) };
        let move_to = self.compute_vertical_move_column(current_visual_col, source_max, target_max);

        self.state.cursor_line = target_vl.logical_line;
        let logical = self.state.lines[target_vl.logical_line].clone();
        let mut col = (target_vl.start_col + move_to).min(logical.len());
        while !logical.is_char_boundary(col) {
            col -= 1;
        }
        self.state.cursor_col = col;

        // Never leave the cursor inside a multi-unit segment such as a paste marker.
        for segment in self.graphemes(&logical) {
            if segment.index > col {
                break;
            }
            if segment.segment.encode_utf16().nth(1).is_none() || col >= segment.index + segment.segment.len() {
                continue;
            }
            if segment.index < target_vl.start_col && target > current {
                // Moving down into the tail of a segment visited already: skip its continuation lines.
                let segment_end = segment.index + segment.segment.len();
                let mut next = target + 1;
                while next < visual_lines.len()
                    && visual_lines[next].logical_line == target_vl.logical_line
                    && visual_lines[next].start_col < segment_end
                {
                    next += 1;
                }
                if next < visual_lines.len() {
                    self.move_to_visual_line(visual_lines, current, next);
                    return;
                }
            }
            // Snap to the segment start so it is highlighted; remember where the cursor was.
            self.snapped_from_cursor_col = Some(col);
            self.state.cursor_col = segment.index;
            return;
        }
        self.snapped_from_cursor_col = None;
    }

    /// Sticky column decision (upstream table): start or continue a preferred column when the
    /// target is shorter, return to it when a later line is long enough.
    fn compute_vertical_move_column(&mut self, current: usize, source_max: usize, target_max: usize) -> usize {
        let cursor_in_middle = current < source_max;
        let target_too_short = target_max < current;
        let Some(preferred) = self.preferred_visual_col.filter(|_| !cursor_in_middle) else {
            if target_too_short {
                self.preferred_visual_col = Some(current);
                return target_max;
            }
            self.preferred_visual_col = None;
            return current;
        };
        if target_too_short || target_max < preferred {
            return target_max;
        }
        self.preferred_visual_col = None;
        preferred
    }

    pub fn move_to_line_start(&mut self) {
        self.last_action = None;
        self.set_cursor_col(0);
    }

    pub fn move_to_line_end(&mut self) {
        self.last_action = None;
        let length = self.current_line().len();
        self.set_cursor_col(length);
    }

    pub fn move_cursor(&mut self, delta_line: isize, delta_col: isize) {
        self.last_action = None;
        let visual_lines = self.build_visual_line_map(self.last_width);
        let current = self.find_current_visual_line(&visual_lines);

        if delta_line != 0 {
            let target = current as isize + delta_line;
            if target >= 0 && (target as usize) < visual_lines.len() {
                self.move_to_visual_line(&visual_lines, current, target as usize);
            }
        }

        if delta_col > 0 {
            let line = self.current_line();
            if self.state.cursor_col < line.len() {
                let after = &line[self.state.cursor_col..];
                let length = self.graphemes(after).first().map_or(1, |s| s.segment.len());
                self.set_cursor_col(self.state.cursor_col + length);
            } else if self.state.cursor_line + 1 < self.state.lines.len() {
                self.state.cursor_line += 1;
                self.set_cursor_col(0);
            } else if let Some(visual) = visual_lines.get(current) {
                // At the very end: remember the column for a following Up or Down.
                self.preferred_visual_col = Some(self.state.cursor_col.saturating_sub(visual.start_col));
            }
        } else if delta_col < 0 {
            if self.state.cursor_col > 0 {
                let before = self.text_before_cursor();
                let length = self.graphemes(before).last().map_or(1, |s| s.segment.len());
                self.set_cursor_col(self.state.cursor_col - length);
            } else if self.state.cursor_line > 0 {
                self.state.cursor_line -= 1;
                let length = self.current_line().len();
                self.set_cursor_col(length);
            }
        }

        // An open picker was computed for the old position; refresh or close it.
        if self.autocomplete_mode.is_some() {
            self.update_autocomplete();
        }
    }

    /// Moves the cursor a page (30% of the terminal, at least 5 lines) up or down.
    pub fn page_scroll(&mut self, direction: isize) {
        self.last_action = None;
        let page_size = self.max_visible_lines() as isize;
        let visual_lines = self.build_visual_line_map(self.last_width);
        let current = self.find_current_visual_line(&visual_lines);
        let last = visual_lines.len() as isize - 1;
        let target = (current as isize + direction * page_size).clamp(0, last.max(0)) as usize;
        self.move_to_visual_line(&visual_lines, current, target);
    }

    fn word_options(&self) -> WordNavigationOptions<'_> {
        let pastes = &self.pastes;
        WordNavigationOptions::default()
            .with_segment(move |text| word_segments_with_markers(text, pastes))
            .with_atomic_segment(is_paste_marker)
    }

    pub fn move_word_backwards(&mut self) {
        self.last_action = None;
        if self.state.cursor_col == 0 {
            if self.state.cursor_line > 0 {
                self.state.cursor_line -= 1;
                let length = self.current_line().len();
                self.set_cursor_col(length);
            }
            return;
        }
        let col = find_word_backward(self.current_line(), self.state.cursor_col, &self.word_options());
        self.set_cursor_col(col);
    }

    pub fn move_word_forwards(&mut self) {
        self.last_action = None;
        if self.state.cursor_col >= self.current_line().len() {
            if self.state.cursor_line + 1 < self.state.lines.len() {
                self.state.cursor_line += 1;
                self.set_cursor_col(0);
            }
            return;
        }
        let col = find_word_forward(self.current_line(), self.state.cursor_col, &self.word_options());
        self.set_cursor_col(col);
    }

    /// Jumps to the nearest occurrence of `target` in `direction`, across lines, skipping the cursor.
    pub fn jump_to_char(&mut self, target: &str, direction: JumpDirection) {
        self.last_action = None;
        let forward = direction == JumpDirection::Forward;
        let line_count = self.state.lines.len();
        let lines: Box<dyn Iterator<Item = usize>> = if forward {
            Box::new(self.state.cursor_line..line_count)
        } else {
            Box::new((0..=self.state.cursor_line).rev())
        };
        for line_index in lines {
            let line = &self.state.lines[line_index];
            let found = if line_index != self.state.cursor_line {
                if forward { line.find(target) } else { line.rfind(target) }
            } else if forward {
                // Search from one character past the cursor.
                let cursor = self.state.cursor_col;
                let from = line[cursor..].chars().next().map_or(cursor + 1, |c| cursor + c.len_utf8());
                line.get(from..).and_then(|rest| rest.find(target)).map(|index| from + index)
            } else {
                // Matches starting before the cursor; at column 0 only a match at 0 counts.
                let cursor = self.state.cursor_col;
                line.match_indices(target).map(|(index, _)| index).filter(|&i| i < cursor || i == 0).last()
            };
            if let Some(col) = found {
                self.state.cursor_line = line_index;
                self.set_cursor_col(col);
                return;
            }
        }
    }
}
