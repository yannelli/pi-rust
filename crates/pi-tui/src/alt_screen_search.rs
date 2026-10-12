//! Transcript search for the fullscreen renderer: the match index and the search box overlay
//! (upstream `alt-screen-search.ts`).

use std::rc::Rc;

use regex::RegexBuilder;

use crate::component::Component;
use crate::components::input::{Input, InputOptions};
use crate::components::scroll_view::StyleFn;
use crate::keybindings::get_keybindings;
use crate::utils::{graphemes, strip_terminal_sequences, truncate_to_width_with, visible_width};

struct SearchSourceSpan {
    text_start: usize,
    text_end: usize,
    row: usize,
    start_col: usize,
    end_col: usize,
    linear_columns: bool,
}

struct SearchCorpus {
    text: String,
    spans: Vec<SearchSourceSpan>,
}

/// One row of a match, in content cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AltScreenSearchSegment {
    pub row: usize,
    pub start_col: usize,
    pub end_col: usize,
}

/// A match, possibly spanning rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AltScreenSearchMatch {
    pub segments: Vec<AltScreenSearchSegment>,
}

fn is_printable_ascii(line: &str) -> bool {
    line.bytes().all(|b| (0x20..=0x7e).contains(&b))
}

struct CorpusBuilder {
    text: String,
    spans: Vec<SearchSourceSpan>,
    pending_separator: bool,
}

impl CorpusBuilder {
    fn push(&mut self, segment: &str, row: usize, column: usize, width: usize, linear: bool) {
        if self.pending_separator {
            self.text.push(' ');
            self.pending_separator = false;
        }
        let start = self.text.len();
        self.text.push_str(segment);
        self.spans.push(SearchSourceSpan {
            text_start: start,
            text_end: self.text.len(),
            row,
            start_col: column,
            end_col: column + width,
            linear_columns: linear,
        });
    }

    fn separate(&mut self) {
        if !self.text.is_empty() {
            self.pending_separator = true;
        }
    }
}

fn build_search_corpus(lines: &[String]) -> SearchCorpus {
    let mut builder = CorpusBuilder { text: String::new(), spans: Vec::new(), pending_separator: false };
    for (row, raw) in lines.iter().enumerate() {
        let line = strip_terminal_sequences(raw);
        let mut column = 0;
        if is_printable_ascii(&line) {
            // Rendered transcripts are mostly ASCII: index whole non-space runs at once.
            for (index, word) in line.split(' ').enumerate() {
                if index > 0 {
                    builder.separate();
                    column += 1;
                }
                if !word.is_empty() {
                    builder.push(word, row, column, word.len(), true);
                    column += word.len();
                }
            }
        } else {
            for grapheme in graphemes(&line) {
                let width = visible_width(grapheme);
                if grapheme.chars().all(char::is_whitespace) {
                    builder.separate();
                } else {
                    builder.push(grapheme, row, column, width, false);
                }
                column += width;
            }
        }
        builder.separate();
    }
    SearchCorpus { text: builder.text, spans: builder.spans }
}

fn normalize_query(query: &str) -> String {
    query.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn find_corpus_matches(corpus: &SearchCorpus, query: &str) -> Vec<AltScreenSearchMatch> {
    if query.is_empty() {
        return Vec::new();
    }
    let Ok(expression) = RegexBuilder::new(&regex::escape(query)).case_insensitive(true).build() else {
        return Vec::new();
    };
    let mut matches = Vec::new();
    let mut span_index = 0;
    for found in expression.find_iter(&corpus.text) {
        let (start, end) = (found.start(), found.end());
        while span_index < corpus.spans.len() && corpus.spans[span_index].text_end <= start {
            span_index += 1;
        }
        let mut segments: Vec<AltScreenSearchSegment> = Vec::new();
        for span in &corpus.spans[span_index..] {
            if span.text_start >= end {
                break;
            }
            if span.text_end <= start {
                continue;
            }
            let (start_col, end_col) = if span.linear_columns {
                (
                    span.start_col + start.max(span.text_start) - span.text_start,
                    span.start_col + end.min(span.text_end) - span.text_start,
                )
            } else {
                (span.start_col, span.end_col)
            };
            match segments.last_mut() {
                Some(last) if last.row == span.row && start_col <= last.end_col => {
                    last.end_col = last.end_col.max(end_col)
                }
                _ => segments.push(AltScreenSearchSegment { row: span.row, start_col, end_col }),
            }
        }
        while span_index < corpus.spans.len() && corpus.spans[span_index].text_end <= end {
            span_index += 1;
        }
        if !segments.is_empty() {
            matches.push(AltScreenSearchMatch { segments });
        }
    }
    matches
}

/// Result of [`AltScreenSearchIndex::search`].
pub struct AltScreenSearchResult<'a> {
    pub matches: &'a [AltScreenSearchMatch],
    /// Whether the lines or the query changed since the last search.
    pub changed: bool,
}

/// Caches the corpus and matches while the rendered transcript lines stay the same.
#[derive(Default)]
pub struct AltScreenSearchIndex {
    source_lines: Option<Rc<Vec<String>>>,
    corpus: Option<SearchCorpus>,
    normalized_query: Option<String>,
    matches: Vec<AltScreenSearchMatch>,
}

impl AltScreenSearchIndex {
    pub fn search(&mut self, lines: &Rc<Vec<String>>, query: &str) -> AltScreenSearchResult<'_> {
        let source_changed = match &self.source_lines {
            Some(source) => !Rc::ptr_eq(source, lines) && source.as_slice() != lines.as_slice(),
            None => true,
        };
        if source_changed || self.corpus.is_none() {
            self.source_lines = Some(lines.clone());
            self.corpus = Some(build_search_corpus(lines));
        }
        let normalized = normalize_query(query);
        let changed = source_changed || self.normalized_query.as_deref() != Some(normalized.as_str());
        if changed {
            self.matches = self.corpus.as_ref().map(|c| find_corpus_matches(c, &normalized)).unwrap_or_default();
            self.normalized_query = Some(normalized);
        }
        AltScreenSearchResult { matches: &self.matches, changed }
    }
}

/// Matches of `query` in `lines`, ignoring case and collapsing whitespace.
pub fn find_alt_screen_search_matches(lines: &[String], query: &str) -> Vec<AltScreenSearchMatch> {
    let normalized = normalize_query(query);
    if normalized.is_empty() { Vec::new() } else { find_corpus_matches(&build_search_corpus(lines), &normalized) }
}

/// Stable key of a match: first row and column, last row and end column.
pub fn get_alt_screen_search_match_key(found: &AltScreenSearchMatch) -> String {
    match (found.segments.first(), found.segments.last()) {
        (Some(first), Some(last)) => format!("{}:{}:{}:{}", first.row, first.start_col, last.row, last.end_col),
        _ => String::new(),
    }
}

/// Style for a search navigation button, given whether the pointer hovers it.
pub type NavigationButtonStyle = Rc<dyn Fn(&str, bool) -> String>;

/// Result and button state of the search box, shared with the renderer so it can update the
/// box while the box handles input.
#[derive(Debug, Default)]
pub struct AltScreenSearchBoxState {
    pub result_count: usize,
    pub result_index: Option<usize>,
    previous_button: Option<(usize, usize)>,
    next_button: Option<(usize, usize)>,
    hovered: Option<i32>,
}

impl AltScreenSearchBoxState {
    /// Sets the selected match (`None` for no selection) and the match count.
    pub fn set_result(&mut self, index: Option<usize>, count: usize) {
        self.result_index = index;
        self.result_count = count;
    }

    /// Navigation direction of the button at a cell of the rendered box, if any.
    pub fn get_navigation_direction_at(&self, row: usize, column: usize) -> Option<i32> {
        if row != 2 {
            return None;
        }
        if self.previous_button.is_some_and(|(start, end)| column >= start && column < end) {
            return Some(-1);
        }
        if self.next_button.is_some_and(|(start, end)| column >= start && column < end) {
            return Some(1);
        }
        None
    }

    /// Updates the hovered button. Returns true when it changed.
    pub fn set_hovered_navigation_direction(&mut self, direction: Option<i32>) -> bool {
        if self.hovered == direction {
            return false;
        }
        self.hovered = direction;
        true
    }
}

/// The search box shown as an overlay while searching the transcript.
pub struct AltScreenSearchComponent {
    input: Input,
    on_query_change: Box<dyn FnMut(&str)>,
    navigation_button_style: NavigationButtonStyle,
    shared: Rc<std::cell::RefCell<AltScreenSearchBoxState>>,
    focused: bool,
}

fn format_key(key: Option<&String>) -> String {
    let Some(key) = key else { return "Unbound".to_string() };
    key.split('+')
        .map(|part| {
            if cfg!(target_os = "macos") && part.eq_ignore_ascii_case("alt") {
                return "Option".to_string();
            }
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().chain(chars).collect(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join("+")
}

impl AltScreenSearchComponent {
    pub fn new(on_query_change: Box<dyn FnMut(&str)>, navigation_button_style: Option<NavigationButtonStyle>) -> Self {
        let placeholder_style: StyleFn = Rc::new(|text| format!("\x1b[2m{text}\x1b[22m"));
        Self {
            input: Input::new(InputOptions {
                prompt: Some(" ".to_string()),
                placeholder: Some("Find in transcript".to_string()),
                placeholder_style: Some(placeholder_style),
            }),
            on_query_change,
            navigation_button_style: navigation_button_style.unwrap_or_else(|| Rc::new(|text, _| text.to_string())),
            shared: Rc::default(),
            focused: false,
        }
    }

    /// Current query text.
    pub fn query(&self) -> &str {
        self.input.get_value()
    }

    /// Result and button state shared with the renderer.
    pub fn state(&self) -> Rc<std::cell::RefCell<AltScreenSearchBoxState>> {
        self.shared.clone()
    }
}

impl Component for AltScreenSearchComponent {
    fn handle_input(&mut self, data: &str) {
        let previous = self.input.get_value().to_string();
        self.input.handle_input(data);
        let query = self.input.get_value().to_string();
        if query != previous {
            (self.on_query_change)(&query);
        }
    }

    fn handles_input(&self) -> bool {
        true
    }

    fn invalidate(&mut self) {
        self.input.invalidate();
    }

    fn is_focusable(&self) -> bool {
        true
    }

    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
        self.input.set_focused(focused);
    }

    fn render(&mut self, width: usize) -> Vec<String> {
        let safe_width = width.max(1);
        let inner_width = safe_width.saturating_sub(2);
        let keybindings = get_keybindings();
        let previous_key = format_key(keybindings.get_keys("tui.altScreen.searchPrevious").first());
        let next_key = format_key(keybindings.get_keys("tui.altScreen.searchNext").first());
        let query = self.input.get_value().to_string();
        let (result_index, result_count, hovered) = {
            let shared = self.shared.borrow();
            (shared.result_index, shared.result_count, shared.hovered)
        };
        let result = match (query.is_empty(), result_count) {
            (true, _) => String::new(),
            (false, 0) => "No matches".to_string(),
            (false, count) => format!("{}/{count}", result_index.map_or(0, |i| i + 1)),
        };
        let visible_result = truncate_to_width_with(&result, inner_width.saturating_sub(3), "", false);
        let result_text =
            if visible_result.is_empty() { String::new() } else { format!("\x1b[2m {visible_result} \x1b[22m") };
        let input_width = inner_width.saturating_sub(visible_width(&result_text));
        let rendered_input = self.input.render(input_width.max(1)).into_iter().next().unwrap_or_default();
        let input_line = truncate_to_width_with(&rendered_input, input_width, "", false);
        let input_padding = " ".repeat(input_width.saturating_sub(visible_width(&input_line)));
        let content = format!("{input_line}{input_padding}{result_text}");

        let mut previous_button = format!("↑ {previous_key}");
        let mut next_button = format!("↓ {next_key}");
        let mut separator = " · ";
        let outer_gap = 1;
        let available = inner_width.saturating_sub(outer_gap * 2 + 1);
        let mut controls_width =
            visible_width(&previous_button) + visible_width(separator) + visible_width(&next_button);
        if controls_width > available {
            previous_button = "↑".to_string();
            next_button = "↓".to_string();
            separator = " ";
            controls_width = visible_width(&previous_button) + visible_width(separator) + visible_width(&next_button);
        }
        let show_buttons = controls_width <= available;
        let rendered_buttons = if show_buttons {
            let style = &self.navigation_button_style;
            format!(
                "{}{separator}{}",
                style(&previous_button, hovered == Some(-1)),
                style(&next_button, hovered == Some(1))
            )
        } else {
            String::new()
        };
        let outer_gaps = if show_buttons { outer_gap * 2 } else { 0 };
        let right_rule = usize::from(!rendered_buttons.is_empty() && inner_width > controls_width + outer_gaps);
        let left_rule =
            inner_width.saturating_sub(if show_buttons { controls_width } else { 0 } + outer_gaps + right_rule);
        {
            let mut shared = self.shared.borrow_mut();
            if show_buttons {
                let previous_start = 1 + left_rule + outer_gap;
                let previous_end = previous_start + visible_width(&previous_button);
                let next_start = previous_end + visible_width(separator);
                shared.previous_button = Some((previous_start, previous_end));
                shared.next_button = Some((next_start, next_start + visible_width(&next_button)));
            } else {
                shared.previous_button = None;
                shared.next_button = None;
            }
        }

        if safe_width == 1 {
            return vec!["┌".to_string(), "│".to_string(), "└".to_string()];
        }
        let gap = if rendered_buttons.is_empty() { "" } else { " " };
        vec![
            format!("┌{}┐", "─".repeat(inner_width)),
            format!("│{content}│"),
            format!("└{}{gap}{rendered_buttons}{gap}{}┘", "─".repeat(left_rule), "─".repeat(right_rule)),
        ]
    }
}
