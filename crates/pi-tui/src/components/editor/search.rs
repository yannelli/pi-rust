//! Incremental reverse search through prompt history (pi-rust extension). The history search
//! key opens it, typed text filters entries case-insensitively, and the key again moves to the
//! next older match. The editor shows the match; a prompt line under it shows the query.

use super::input::printable_input;
use super::state::{EditorCore, TextState};
use crate::keybindings::{KeybindingsManager, ids};
use crate::utils::{graphemes, truncate_to_width_with};

/// An open reverse search.
#[derive(Debug)]
pub(crate) struct HistorySearch {
    pub query: String,
    /// History index of the shown match and the byte offset of the query in it.
    pub found: Option<(usize, usize)>,
    /// Entries shown for this query, so repeated searches skip identical prompts.
    pub visited: Vec<usize>,
    /// True when the last search found nothing; the last match stays shown.
    pub failed: bool,
    /// The text before the search, restored on cancel.
    pub draft: TextState,
}

/// Byte offset in `text` of the first match of `query`, ignoring case.
pub(super) fn find_ignore_case(text: &str, query: &str) -> Option<usize> {
    let query = query.to_lowercase();
    let mut lower = String::with_capacity(text.len());
    let mut origin = Vec::with_capacity(text.len());
    for (index, ch) in text.char_indices() {
        lower.extend(ch.to_lowercase());
        origin.resize(lower.len(), index);
    }
    lower.find(&query).map(|at| origin.get(at).copied().unwrap_or(text.len()))
}

impl EditorCore {
    pub fn start_history_search(&mut self) {
        self.cancel_autocomplete();
        self.exit_history_browsing();
        self.last_action = None;
        self.jump_mode = None;
        let draft = self.state.clone();
        self.history_search =
            Some(HistorySearch { query: String::new(), found: None, visited: Vec::new(), failed: false, draft });
    }

    /// Keys while searching. Returns false when the key ends the search and should be handled
    /// as usual afterwards.
    pub(super) fn handle_history_search_key(&mut self, data: &str, kb: &KeybindingsManager) -> bool {
        if kb.matches(data, ids::EDITOR_HISTORY_SEARCH) {
            self.search_older();
        } else if kb.matches(data, ids::SELECT_CANCEL) {
            self.cancel_history_search();
        } else if kb.matches(data, ids::INPUT_SUBMIT) || kb.matches(data, ids::SELECT_CONFIRM) {
            self.accept_history_search();
        } else if kb.matches(data, ids::EDITOR_DELETE_CHAR_BACKWARD) {
            let mut query = self.search_query();
            let last = graphemes(&query).next_back().map_or(0, str::len);
            query.truncate(query.len() - last);
            self.set_search_query(query);
        } else if let Some(text) = printable_input(data) {
            self.append_to_search(&text);
        } else {
            self.accept_history_search();
            return false;
        }
        true
    }

    /// Adds typed or pasted text to the query.
    pub(super) fn append_to_search(&mut self, text: &str) {
        let query = self.search_query() + &Self::normalize_text(text);
        self.set_search_query(query);
    }

    fn search_query(&self) -> String {
        self.history_search.as_ref().map(|search| search.query.clone()).unwrap_or_default()
    }

    fn set_search_query(&mut self, query: String) {
        let Some(search) = self.history_search.as_mut() else { return };
        search.query = query;
        search.visited.clear();
        if search.query.is_empty() {
            search.found = None;
            search.failed = false;
            let draft = search.draft.clone();
            self.restore_search_text(draft);
        } else {
            self.search_from(0);
        }
    }

    /// The next older match of the current query.
    fn search_older(&mut self) {
        let Some(search) = &self.history_search else { return };
        if !search.query.is_empty() {
            self.search_from(search.found.map_or(0, |(index, _)| index + 1));
        }
    }

    /// Shows the newest match at history index `from` or older.
    fn search_from(&mut self, from: usize) {
        let Some(search) = self.history_search.as_mut() else { return };
        let history = &self.history;
        let seen = |entry: &String| search.visited.iter().any(|&index| history.get(index) == Some(entry));
        let found = history
            .iter()
            .enumerate()
            .skip(from)
            .filter(|(_, entry)| !seen(entry))
            .find_map(|(index, entry)| find_ignore_case(entry, &search.query).map(|offset| (index, offset)));
        let Some((index, offset)) = found else {
            search.failed = true;
            return;
        };
        search.found = Some((index, offset));
        search.visited.push(index);
        search.failed = false;
        let entry = history[index].clone();
        self.set_text_internal(&entry, false);
        let before = &entry[..offset];
        self.state.cursor_line = before.matches('\n').count();
        self.set_cursor_col(before.rfind('\n').map_or(offset, |newline| offset - newline - 1));
    }

    /// Keeps the shown text. The text from before the search becomes an undo step.
    pub fn accept_history_search(&mut self) {
        let Some(search) = self.history_search.take() else { return };
        if self.state != search.draft {
            let shown = std::mem::replace(&mut self.state, search.draft);
            self.push_undo_snapshot();
            self.state = shown;
            self.pastes.clear();
            self.paste_counter = 0;
        }
    }

    /// Closes the search and restores the text from before it.
    pub fn cancel_history_search(&mut self) {
        if let Some(search) = self.history_search.take() {
            self.restore_search_text(search.draft);
        }
    }

    fn restore_search_text(&mut self, draft: TextState) {
        if self.state != draft {
            self.state = draft;
            self.preferred_visual_col = None;
            self.snapped_from_cursor_col = None;
            self.scroll_offset = 0;
            self.emit_change();
        }
    }

    /// The prompt line under the editor: `(reverse-i-search)'query': matching line`.
    pub(super) fn render_history_search(&self, width: usize) -> Option<String> {
        let search = self.history_search.as_ref()?;
        let label = if search.failed { "(failed reverse-i-search)" } else { "(reverse-i-search)" };
        let line = search.found.and_then(|(index, offset)| {
            let entry = self.history.get(index)?;
            let start = entry.get(..offset)?.rfind('\n').map_or(0, |newline| newline + 1);
            entry[start..].lines().next()
        });
        let label = (self.theme.select_list.description)(label);
        let text = format!("{label}'{}': {}", search.query.replace('\n', " "), line.unwrap_or_default());
        Some(truncate_to_width_with(&text, width, "...", true))
    }
}

#[cfg(test)]
mod tests {
    use super::find_ignore_case;

    #[test]
    fn finds_byte_offsets_ignoring_case() {
        assert_eq!(find_ignore_case("Run The Tests", "the"), Some(4));
        assert_eq!(find_ignore_case("Straße ÄBC", "äbc"), Some("Straße ".len()));
        assert_eq!(find_ignore_case("İx needle", "NEEDLE"), Some("İx ".len()));
        assert_eq!(find_ignore_case("abc", "abd"), None);
        assert_eq!(find_ignore_case("abc", ""), Some(0));
    }
}
