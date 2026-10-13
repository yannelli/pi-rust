//! Ported from upstream test/regression-regional-indicator-width.test.ts.

use pi_tui::utils::{visible_width, wrap_text_with_ansi};

#[test]
fn treats_partial_flag_grapheme_as_full_width_to_avoid_streaming_render_drift() {
    // While streaming, a flag often shows up as its first regional indicator alone.
    assert_eq!(visible_width("🇨"), 2);
    assert_eq!(visible_width("      - 🇨"), 10);
}

#[test]
fn wraps_intermediate_partial_flag_list_line_before_overflow() {
    let wrapped = wrap_text_with_ansi("      - 🇨", 9);
    assert_eq!(wrapped.len(), 2);
    assert_eq!(visible_width(&wrapped[0]), 7);
    assert_eq!(visible_width(&wrapped[1]), 2);
}

#[test]
fn treats_all_regional_indicator_singleton_graphemes_as_width_2() {
    for cp in 0x1F1E6..=0x1F1FF {
        let indicator = char::from_u32(cp).unwrap().to_string();
        assert_eq!(visible_width(&indicator), 2, "expected U+{cp:X} to be width 2");
    }
}

#[test]
fn keeps_full_flag_pairs_at_width_2() {
    for flag in ["🇯🇵", "🇺🇸", "🇬🇧", "🇨🇳", "🇩🇪", "🇫🇷"] {
        assert_eq!(visible_width(flag), 2, "expected {flag} to be width 2");
    }
}

#[test]
fn keeps_common_streaming_emoji_intermediates_at_stable_width() {
    let samples = [
        "\u{1F44D}",
        "\u{1F44D}\u{1F3FB}",
        "\u{2705}",
        "\u{26A1}",
        "\u{26A1}\u{FE0F}",
        "\u{1F468}",
        "\u{1F468}\u{200D}\u{1F4BB}",
        "\u{1F3F3}\u{FE0F}\u{200D}\u{1F308}",
    ];
    for sample in samples {
        assert_eq!(visible_width(sample), 2, "expected {sample:?} to be width 2");
    }
}
