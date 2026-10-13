//! Ported from upstream test/visible-width.test.ts and the visibleWidth cases of truncate-to-width.test.ts.

use pi_tui::utils::{normalize_terminal_output, visible_width};

#[test]
fn measures_styled_ascii_without_counting_escape_sequences() {
    assert_eq!(visible_width("\x1b[38;5;4mhello\x1b[39m world"), 11);
    assert_eq!(visible_width("\x1b]8;;https://example.com\x07link\x1b]8;;\x07"), 4);
    assert_eq!(visible_width("\x1b]133;A\x1b\\prompt"), 6);
    assert_eq!(visible_width("\x1b_pi:c\x07cursor"), 6);
}

#[test]
fn counts_tabs_as_three_columns_in_styled_text() {
    assert_eq!(visible_width("\x1b[1ma\tb\x1b[22m"), 5);
}

#[test]
fn measures_styled_non_ascii_text() {
    assert_eq!(visible_width("\x1b[31m日本\x1b[39m ok"), 7);
    assert_eq!(visible_width("\x1b[31m\u{2500}\u{2192}\x1b[39m"), 2);
}

#[test]
fn treats_unterminated_escape_sequences_as_zero_width_control_characters() {
    assert_eq!(visible_width("\x1b[31"), 3);
    assert_eq!(visible_width("a\x1b"), 1);
}

#[test]
fn counts_tabs_inline_and_skips_ansi_inline() {
    assert_eq!(visible_width("\t\x1b[31m界\x1b[0m"), 5);
}

#[test]
fn counts_indic_conjunct_spacing_code_points_within_grapheme_clusters() {
    assert_eq!(visible_width("र्क"), 2);
    assert_eq!(visible_width("नेटवर्क"), 5);
    assert_eq!(visible_width("सर्वाधिकार सुरक्षित। ऑर्डर पर क्लिक करें"), 33);
    assert_eq!(visible_width("র্ক"), 2);
    assert_eq!(visible_width("ર્ક"), 2);
    assert_eq!(visible_width("ର୍କ"), 2);
    assert_eq!(visible_width("ర్క"), 2);
    assert_eq!(visible_width("ര\u{D4D}\u{200D}ക"), 2);
}

#[test]
fn keeps_ordinary_combining_marks_zero_width() {
    assert_eq!(visible_width("e\u{0301}"), 1);
    assert_eq!(visible_width("čřžůú"), 5);
    assert_eq!(visible_width("שָׁ"), 1);
    assert_eq!(visible_width("بّ"), 1);
    assert_eq!(visible_width("རྐ"), 1);
    assert_eq!(visible_width("\u{1720}\u{1734}"), 1);
    assert_eq!(visible_width("가\u{302E}"), 2);
    assert_eq!(visible_width("가\u{302F}"), 2);
}

#[test]
fn keeps_cjk_and_japanese_width_accounting_unchanged() {
    assert_eq!(visible_width("网络"), 4);
    assert_eq!(visible_width("ネットワーク"), 12);
    assert_eq!(visible_width("が"), 2);
    assert_eq!(visible_width("か\u{3099}"), 2);
}

#[test]
fn counts_myanmar_marks_that_terminals_allocate_cells_for() {
    for text in ["ကာ", "ကေ", "က်", "ကျ", "ကြ", "ကဳ", "ကဴ", "ကဵ", "ကး"] {
        assert_eq!(visible_width(text), 2, "{text:?}");
    }
    assert_eq!(visible_width("ကို"), 1);
    assert_eq!(visible_width("က္"), 1);
}

#[test]
fn keeps_thai_and_lao_am_clusters_at_their_normal_cell_width() {
    assert_eq!(visible_width("ำ"), 1);
    assert_eq!(visible_width("ຳ"), 1);
    assert_eq!(visible_width("กำ"), 2);
    assert_eq!(visible_width("ກຳ"), 2);
}

#[test]
fn normalizes_thai_and_lao_am_vowels_only_for_terminal_output() {
    assert_eq!(normalize_terminal_output("ำ"), "\u{0E4D}\u{0E32}");
    assert_eq!(normalize_terminal_output("ຳ"), "\u{0ECD}\u{0EB2}");
    assert_eq!(visible_width(&normalize_terminal_output("ำabc")), visible_width("ำabc"));
    assert_eq!(visible_width(&normalize_terminal_output("ຳabc")), visible_width("ຳabc"));
}
