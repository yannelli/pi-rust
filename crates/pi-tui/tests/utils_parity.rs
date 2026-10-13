//! Expectations generated from upstream utils.ts by tools/gen_parity_tests.mjs; do not edit by hand.

use pi_tui::utils::{extract_segments, slice_with_width, truncate_to_width_with, visible_width, wrap_text_with_ansi};

#[test]
fn visible_width_matches_upstream() {
    let cases: &[(&str, usize)] = &[
        ("\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}\u{200D}\u{1F466}", 2),
        ("\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}", 2),
        ("\u{1F468}\u{200D}\u{1F469}", 2),
        ("\u{1F469}\u{200D}\u{2764}\u{FE0F}\u{200D}\u{1F468}", 2),
        ("\u{1F469}\u{200D}\u{2764}\u{200D}\u{1F468}", 2),
        ("\u{1F9D1}\u{1F3FB}\u{200D}\u{1F91D}\u{200D}\u{1F9D1}\u{1F3FF}", 2),
        ("\u{1FAF1}\u{1F3FB}\u{200D}\u{1FAF2}\u{1F3FB}", 2),
        ("\u{1F3F3}\u{FE0F}\u{200D}\u{26A7}\u{FE0F}", 2),
        ("\u{1F3F3}\u{200D}\u{26A7}\u{FE0F}", 1),
        ("\u{2764}\u{FE0F}\u{200D}\u{1F525}", 2),
        ("\u{2764}\u{200D}\u{1F525}", 1),
        ("\u{1F441}\u{FE0F}\u{200D}\u{1F5E8}\u{FE0F}", 2),
        ("\u{26F9}\u{1F3FB}\u{200D}\u{2640}\u{FE0F}", 2),
        ("\u{26F9}\u{FE0F}\u{200D}\u{2640}\u{FE0F}", 2),
        ("\u{1F3F4}\u{E0067}\u{E0062}\u{E0065}\u{E006E}\u{E0067}\u{E007F}", 2),
        ("\u{1F3F4}\u{E0067}\u{E0062}\u{E007F}", 2),
        ("#\u{FE0F}\u{20E3}", 2),
        ("#\u{20E3}", 1),
        ("5\u{FE0F}\u{20E3}", 2),
        ("\u{1F1FA}\u{1F1F8}", 2),
        ("\u{1F1FA}\u{1F1FA}", 2),
        ("\u{1F1FA}\u{1F1F8}\u{1F1E9}", 4),
        ("\u{261D}", 1),
        ("\u{261D}\u{FE0F}", 2),
        ("\u{261D}\u{1F3FD}", 2),
        ("\u{270C}\u{1F3FB}", 2),
        ("\u{A9}", 1),
        ("\u{A9}\u{FE0F}", 2),
        ("\u{2194}\u{FE0F}", 2),
        ("\u{25FD}", 2),
        ("\u{2B50}", 2),
        ("\u{1F170}", 1),
        ("\u{1F170}\u{FE0F}", 2),
        ("\u{1F3FB}", 2),
        ("\u{1F44B}\u{1F3FC}", 2),
        ("\u{200D}", 0),
        ("\u{FE0F}", 0),
        ("\u{AD}", 0),
        ("\u{600}1", 1),
        ("a\u{301}\u{302}", 1),
        ("\u{301}", 0),
        ("\u{93E}", 1),
        ("\u{915}\u{93F}", 2),
        ("\u{D15}\u{D4D}\u{D15}", 2),
        ("\u{E01}\u{E49}\u{E33}", 2),
        ("\u{1100}\u{1161}\u{11A8}", 2),
        ("\u{1100}", 2),
        ("\u{3131}", 2),
        ("\u{FF76}\u{FF9E}", 2),
        ("\u{FF21}\u{301}", 2),
        ("\u{3000}", 2),
        ("\u{20000}", 2),
        ("\u{1F600}x\u{4E00}", 5),
        ("\x1b[31m\u{1F468}\u{200D}\u{1F4BB}\x1b[0m", 2),
        ("a\tb\u{4E2D}", 7),
        ("\x1b]8;;http://a\x07\u{1F642}\x1b]8;;\x07", 2),
    ];
    for (input, width) in cases {
        assert_eq!(visible_width(input), *width, "{input:?}");
    }
}

#[test]
fn wrap_matches_upstream_0() {
    let expected: &[&str] = &["Hello", "\x1b[1mbold \u{4E2D}\u{6587}", "\x1b[1mtext\x1b[22m that", "keeps", "going"];
    assert_eq!(wrap_text_with_ansi("Hello \x1b[1mbold \u{4E2D}\u{6587} text\x1b[22m that keeps going", 9), expected);
}

#[test]
fn wrap_matches_upstream_1() {
    let expected: &[&str] =
        &["\x1b[4munderlin\x1b[24m", "\x1b[4med words\x1b[24m", "\x1b[4mwrap\x1b[24m", "\x1b[4mhere\x1b[24m", "done"];
    assert_eq!(wrap_text_with_ansi("\x1b[4munderlined words wrap here\x1b[24m done", 8), expected);
}

#[test]
fn wrap_matches_upstream_2() {
    let expected: &[&str] = &["\u{1F468}\u{200D}\u{1F4BB}", "\u{1F468}\u{200D}\u{1F4BB}", "\u{1F468}\u{200D}\u{1F4BB}"];
    assert_eq!(
        wrap_text_with_ansi("\u{1F468}\u{200D}\u{1F4BB}\u{1F468}\u{200D}\u{1F4BB}\u{1F468}\u{200D}\u{1F4BB}", 3),
        expected
    );
}

#[test]
fn wrap_matches_upstream_3() {
    let expected: &[&str] = &[
        "\x1b]8;;https://x.test\x1b\\link\x1b]8;;\x1b\\",
        "\x1b]8;;https://x.test\x1b\\text\x1b]8;;\x1b\\",
        "\x1b]8;;https://x.test\x1b\\here\x1b]8;;\x1b\\",
        "tail",
    ];
    assert_eq!(wrap_text_with_ansi("\x1b]8;;https://x.test\x1b\\link text here\x1b]8;;\x1b\\ tail", 6), expected);
}

#[test]
fn wrap_matches_upstream_4() {
    let expected: &[&str] = &["tabs", "here", "and\tt", "here"];
    assert_eq!(wrap_text_with_ansi("tabs\there\tand\tthere", 7), expected);
}

#[test]
fn wrap_matches_upstream_5() {
    let expected: &[&str] =
        &["\x1b[44m\x1b[31mred", "\x1b[31;44mon", "\x1b[31;44mblue\x1b[39m", "\x1b[44mplain\x1b[0m"];
    assert_eq!(wrap_text_with_ansi("\x1b[44m\x1b[31mred on blue\x1b[39m plain\x1b[0m", 5), expected);
}

#[test]
fn trunc_matches_upstream_6() {
    let result = truncate_to_width_with("\x1b[1m\u{1F642}\u{754C}abc\x1b[0m", 4, "...", false);
    assert_eq!(result, "\x1b[0m...\x1b[0m");
}

#[test]
fn trunc_matches_upstream_7() {
    let result = truncate_to_width_with("\u{915}\u{94D}\u{937}abcdef", 5, "\u{2026}", true);
    assert_eq!(result, "\u{915}\u{94D}\u{937}ab\x1b[0m\u{2026}\x1b[0m");
}

#[test]
fn trunc_matches_upstream_8() {
    let result = truncate_to_width_with("\x1b]8;;http://u\x1b\\a long link label\x1b]8;;\x1b\\", 8, "...", false);
    assert_eq!(result, "\x1b]8;;http://u\x1b\\a lon\x1b]8;;\x1b\\\x1b[0m...\x1b[0m");
}

#[test]
fn trunc_matches_upstream_9() {
    let result = truncate_to_width_with("abc\tdef", 5, "", true);
    assert_eq!(result, "abc\x1b[0m  ");
}

#[test]
fn slice_matches_upstream_10() {
    let r = slice_with_width("\x1b[31m\u{4E2D}a\u{6587}b\x1b[0mc", 1, 3, false);
    assert_eq!((r.text.as_str(), r.width), ("\x1b[31ma\u{6587}", 3));
}

#[test]
fn slice_matches_upstream_11() {
    let r = slice_with_width("\x1b[31m\u{4E2D}a\u{6587}b\x1b[0mc", 1, 3, true);
    assert_eq!((r.text.as_str(), r.width), ("\x1b[31ma", 1));
}

#[test]
fn slice_matches_upstream_12() {
    let r = slice_with_width("ab\x1b_pi:c\x07cd\u{1F642}e", 2, 3, true);
    assert_eq!((r.text.as_str(), r.width), ("\x1b_pi:c\x07cd", 2));
}

#[test]
fn segments_matches_upstream_13() {
    let r = extract_segments("\x1b[1mbold\x1b[22m and \x1b[4munder\x1b[24m\u{1F642}end", 3, 6, 6, false);
    assert_eq!((r.before.as_str(), r.before_width), ("\x1b[1mbol", 3));
    assert_eq!((r.after.as_str(), r.after_width), ("nd \x1b[4mund", 6));
}

#[test]
fn segments_matches_upstream_14() {
    let r = extract_segments("\x1b[42m\u{4E2D}\u{6587}\u{5B57}\x1b_pi:c\x07xyz", 3, 4, 3, true);
    assert_eq!((r.before.as_str(), r.before_width), ("\x1b[42m\u{4E2D}", 2));
    assert_eq!((r.after.as_str(), r.after_width), ("\x1b[42m\u{5B57}\x1b_pi:c\x07x", 3));
}
