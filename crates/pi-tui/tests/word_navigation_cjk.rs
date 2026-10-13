//! Word movement over Chinese, Japanese, and Thai text, which ICU splits into dictionary words.
//! Cursors are byte offsets, so expected positions are written as prefixes.

use pi_tui::word_navigation::{WordNavigationOptions, find_word_backward, find_word_forward};

/// Cursor stops when walking from the end of `text` to its start.
fn backward_stops(text: &str) -> Vec<&str> {
    let options = WordNavigationOptions::default();
    let mut stops = Vec::new();
    let mut cursor = text.len();
    while cursor > 0 {
        cursor = find_word_backward(text, cursor, &options);
        stops.push(&text[..cursor]);
    }
    stops
}

/// Cursor stops when walking from the start of `text` to its end.
fn forward_stops(text: &str) -> Vec<&str> {
    let options = WordNavigationOptions::default();
    let mut stops = Vec::new();
    let mut cursor = 0;
    while cursor < text.len() {
        cursor = find_word_forward(text, cursor, &options);
        stops.push(&text[..cursor]);
    }
    stops
}

#[test]
fn moves_over_chinese_words() {
    let text = "今天天气很好，我们去公园";
    assert_eq!(
        forward_stops(text),
        ["今天", "今天天气", "今天天气很好", "今天天气很好，", "今天天气很好，我们", "今天天气很好，我们去", text]
    );
    assert_eq!(
        backward_stops(text),
        ["今天天气很好，我们去", "今天天气很好，我们", "今天天气很好，", "今天天气很好", "今天天气", "今天", ""]
    );
}

#[test]
fn moves_over_japanese_words() {
    let text = "パソコンでプログラミングを勉強する";
    assert_eq!(
        forward_stops(text),
        [
            "パソコン",
            "パソコンで",
            "パソコンでプログラミング",
            "パソコンでプログラミングを",
            "パソコンでプログラミングを勉強",
            text
        ]
    );
    assert_eq!(backward_stops("こんにちは世界"), ["こんにちは", ""]);
}

#[test]
fn moves_over_thai_words() {
    let text = "สวัสดีครับ ทดสอบ";
    assert_eq!(forward_stops(text), ["สวัสดี", "สวัสดีครับ", text]);
    assert_eq!(backward_stops(text), ["สวัสดีครับ ", "สวัสดี", ""]);
    assert_eq!(forward_stops("ฉันชอบกินข้าวผัด"), ["ฉัน", "ฉันชอบ", "ฉันชอบกิน", "ฉันชอบกินข้าว", "ฉันชอบกินข้าวผัด"]);
}

#[test]
fn moves_over_mixed_scripts() {
    let text = "use 日本語 and ภาษาไทย";
    assert_eq!(forward_stops(text), ["use", "use 日本語", "use 日本語 and", "use 日本語 and ภาษา", text]);
}
