//! Port of upstream `tui-alt-screen.test.ts`: transcript search matching, the match index, and
//! the search box rendering.

mod alt_view_common;

use std::rc::Rc;

use alt_view_common::{column_of, last_column_of};
use pi_tui::Component;
use pi_tui::alt_screen_search::{
    AltScreenSearchComponent, AltScreenSearchIndex, AltScreenSearchMatch, AltScreenSearchSegment,
    find_alt_screen_search_matches,
};
use pi_tui::utils::{strip_terminal_sequences, visible_width};
use regex::Regex;

fn segment(row: usize, start_col: usize, end_col: usize) -> AltScreenSearchSegment {
    AltScreenSearchSegment { row, start_col, end_col }
}

fn lines(values: &[&str]) -> Rc<Vec<String>> {
    Rc::new(values.iter().map(|line| line.to_string()).collect())
}

#[test]
fn searches_normalized_rendered_transcript_text_across_rows() {
    assert_eq!(
        find_alt_screen_search_matches(&lines(&["alpha QUICK", "brown fox"]), "quick brown"),
        vec![AltScreenSearchMatch { segments: vec![segment(0, 6, 11), segment(1, 0, 5)] }]
    );
}

#[test]
fn maps_normalized_ascii_and_unicode_search_matches_back_to_rendered_columns() {
    assert_eq!(
        find_alt_screen_search_matches(&lines(&["\x1b[31mfoo  bar\x1b[0m", "A界🙂éZ"]), "oo   bar\nA界🙂é"),
        vec![AltScreenSearchMatch { segments: vec![segment(0, 1, 3), segment(0, 5, 8), segment(1, 0, 6)] }]
    );
}

#[test]
fn reuses_indexed_transcript_matches_until_the_query_or_rendered_lines_change() {
    let mut index = AltScreenSearchIndex::default();
    let initial = index.search(&lines(&["alpha needle", "omega"]), "needle");
    assert!(initial.changed);
    assert_eq!(initial.matches.len(), 1);
    let initial_matches = initial.matches.as_ptr();

    let cached = index.search(&lines(&["alpha needle", "omega"]), "needle");
    assert!(!cached.changed);
    assert_eq!(cached.matches.as_ptr(), initial_matches);

    let changed_query = index.search(&lines(&["alpha needle", "omega"]), "omega");
    assert!(changed_query.changed);
    assert_ne!(changed_query.matches.as_ptr(), initial_matches);
    assert_eq!(changed_query.matches.first().map(|found| found.segments.clone()), Some(vec![segment(1, 0, 5)]));

    let changed_lines = index.search(&lines(&["alpha needle", "no match"]), "omega");
    assert!(changed_lines.changed);
    assert!(changed_lines.matches.is_empty());
}

#[test]
fn renders_transcript_search_with_a_muted_placeholder_and_right_aligned_controls() {
    // The box reads the process-wide keybindings for its button labels.
    let _guard = pi_tui::env::global_state_lock();
    let mut search = AltScreenSearchComponent::new(Box::new(|_| {}), None);
    let state = search.state();
    let rendered = search.render(48);
    let lines: Vec<String> = rendered.iter().map(|line| strip_terminal_sequences(line)).collect();

    assert_eq!(lines.len(), 3);
    assert!(lines.iter().all(|line| visible_width(line) == 48));
    assert!(Regex::new("^┌─+┐$").unwrap().is_match(&lines[0]), "{:?}", lines[0]);
    assert!(Regex::new("^│ Find in transcript +│$").unwrap().is_match(&lines[1]), "{:?}", lines[1]);
    assert!(rendered[1].contains("\x1b[2m"));
    assert!(Regex::new(r"^└─+ ↑ Shift\+Enter · ↓ Enter ─┘$").unwrap().is_match(&lines[2]), "{:?}", lines[2]);
    let controls = &lines[2];
    let direction_at = |column: usize| state.borrow().get_navigation_direction_at(2, column);
    assert_eq!(direction_at(column_of(controls, "↑").unwrap()), Some(-1));
    assert_eq!(direction_at(column_of(controls, "Shift+Enter").unwrap() + 5), Some(-1));
    assert_eq!(direction_at(column_of(controls, "·").unwrap()), None);
    assert_eq!(direction_at(column_of(controls, "↓").unwrap()), Some(1));
    assert_eq!(direction_at(last_column_of(controls, "Enter").unwrap() + 2), Some(1));

    search.handle_input("n");
    state.borrow_mut().set_result(Some(0), 2);
    let populated_render = search.render(48);
    let populated: Vec<String> = populated_render.iter().map(|line| strip_terminal_sequences(line)).collect();
    assert!(populated[1].contains('n'));
    assert!(populated[1].contains("1/2"));
    assert!(populated_render[1].contains("\x1b[2m 1/2 \x1b[22m"));
    assert!(!populated.iter().any(|line| line.contains("Find in transcript")));
}
