//! Port of upstream `test/input.test.ts` plus the Input case of `test/mouse-components.test.ts`.

#[path = "components_input/kill_ring.rs"]
mod kill_ring;
#[path = "components_input/undo.rs"]
mod undo;

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::MutexGuard;

use pi_tui::component::{CURSOR_MARKER, Component, MouseResponse, TuiMouseButton, TuiMouseEvent, TuiMouseEventType};
use pi_tui::components::input::{Input, InputOptions};
use pi_tui::env::global_state_lock;
use pi_tui::utils::{strip_terminal_sequences, visible_width};

const UNDO: &str = "\x1b[45;5u";
const CTRL_A: &str = "\x01";
const CTRL_E: &str = "\x05";
const CTRL_K: &str = "\x0b";
const CTRL_U: &str = "\x15";
const CTRL_W: &str = "\x17";
const CTRL_Y: &str = "\x19";
const ALT_D: &str = "\x1bd";
const ALT_Y: &str = "\x1by";
const RIGHT: &str = "\x1b[C";

/// Input reads the global keybindings, so every test holds the global state lock.
fn setup() -> (MutexGuard<'static, ()>, Input) {
    (global_state_lock(), Input::default())
}

/// Sends each char of `text` as its own input chunk, like typing.
fn type_text(input: &mut Input, text: &str) {
    for c in text.chars() {
        input.handle_input(c.encode_utf8(&mut [0; 4]));
    }
}

fn press(input: &mut Input, key: &str, times: usize) {
    for _ in 0..times {
        input.handle_input(key);
    }
}

fn value_with_cursor_at_end(input: &mut Input, value: &str) {
    input.set_value(value);
    input.handle_input(CTRL_E);
}

fn mouse_press(x: i32, y: i32, width: i32, height: i32) -> TuiMouseEvent {
    TuiMouseEvent {
        kind: TuiMouseEventType::Press,
        button: TuiMouseButton::Left,
        x,
        y,
        screen_x: x,
        screen_y: y,
        width,
        height,
        shift: false,
        alt: false,
        ctrl: false,
        wheel_delta: None,
        click_count: None,
    }
}

fn render_one(input: &mut Input, width: usize) -> String {
    let lines = input.render(width);
    assert_eq!(lines.len(), 1);
    lines.into_iter().next().unwrap_or_default()
}

#[test]
fn submits_value_including_backslash_on_enter() {
    let (_lock, mut input) = setup();
    let submitted = Rc::new(RefCell::new(None::<String>));
    let sink = Rc::clone(&submitted);
    input.on_submit = Some(Box::new(move |value: &str| *sink.borrow_mut() = Some(value.to_string())));

    type_text(&mut input, "hello\\");
    input.handle_input("\r");

    // Input is single-line, with no backslash+Enter workaround.
    assert_eq!(submitted.borrow().as_deref(), Some("hello\\"));
}

#[test]
fn inserts_backslash_as_regular_character() {
    let (_lock, mut input) = setup();
    input.handle_input("\\");
    input.handle_input("x");
    assert_eq!(input.get_value(), "\\x");
}

#[test]
fn render_supports_custom_prompt_and_styled_placeholder() {
    let _lock = global_state_lock();
    let mut input = Input::new(InputOptions {
        prompt: Some(String::new()),
        placeholder: Some("Find transcript".into()),
        placeholder_style: Some(Rc::new(|text: &str| format!("\x1b[2m{text}\x1b[22m"))),
        ..InputOptions::default()
    });
    input.focused = true;

    let empty = render_one(&mut input, 20);
    assert!(empty.contains("\x1b[2m"));
    assert_eq!(strip_terminal_sequences(&empty).trim_end(), "Find transcript");

    input.handle_input("n");
    let populated = render_one(&mut input, 20);
    assert_eq!(strip_terminal_sequences(&populated).trim_end(), "n");
}

#[test]
fn masked_input_renders_one_star_per_grapheme() {
    let _lock = global_state_lock();
    let mut input = Input::new(InputOptions { masked: true, ..InputOptions::default() });
    input.handle_input("pé日");
    assert_eq!(input.get_value(), "pé日");
    let line = strip_terminal_sequences(&render_one(&mut input, 20));
    assert_eq!(line.trim_end(), "> ***");
}

#[test]
fn render_does_not_overflow_with_wide_cjk_and_fullwidth_text() {
    let _lock = global_state_lock();
    let width = 93;
    let cases = [
        "가나다라마바사아자차카타파하 한글 텍스트가 터미널 너비를 초과하면 크래시가 발생합니다 이것은 재현용 테스트입니다",
        "これはテスト文章です。日本語のテキストが正しく表示されるかどうかを確認するためのサンプルテキストです。あいうえお",
        "这是一段测试文本，用于验证中文字符在终端中的显示宽度是否被正确计算，如果不正确就会导致用户界面崩溃的问题",
        "ＡＢＣＤＥＦＧＨＩＪＫＬＭＮＯＰＱＲＳＴＵＶＷＸＹＺ０１２３４５６７８９ａｂｃｄｅｆｇｈｉｊｋｌｍ",
    ];
    let moves: [(&str, &[&str]); 3] = [("start", &[]), ("middle", &[RIGHT; 10]), ("end", &[CTRL_E])];
    for text in cases {
        for (label, keys) in moves {
            let mut input = Input::default();
            input.set_value(text);
            input.focused = true;
            for key in keys {
                input.handle_input(key);
            }
            let line = render_one(&mut input, width);
            assert!(visible_width(&line) <= width, "rendered line overflowed for {text} at {label}");
        }
    }
}

#[test]
fn render_keeps_cursor_visible_when_scrolling_wide_text() {
    let (_lock, mut input) = setup();
    let width = 20;
    input.set_value("가나다라마바사아자차카타파하");
    input.focused = true;
    input.handle_input(CTRL_A);
    press(&mut input, RIGHT, 5);

    let line = render_one(&mut input, width);
    assert!(visible_width(&line) <= width);
    assert!(line.contains(CURSOR_MARKER));
}

#[test]
fn mouse_press_positions_cursor() {
    let (_lock, mut input) = setup();
    input.set_value("hello");
    input.render(20);

    let response = input.handle_mouse(&mouse_press(4, 0, 20, 1));
    assert!(matches!(response, Some(MouseResponse::Handled(result)) if result.handled && result.focus));
    input.handle_input("X");
    assert_eq!(input.get_value(), "heXllo");
}

// The cases below are not in upstream; they cover ported paths the upstream suite skips.

#[test]
fn mouse_press_accounts_for_horizontal_scroll_and_ignores_other_rows() {
    let (_lock, mut input) = setup();
    value_with_cursor_at_end(&mut input, "abcdefghijklmnopqrstuvwxyz");
    // Width 12 leaves 10 columns, 9 for text plus the cursor cell, so "rstuvwxyz" is visible.
    let line = render_one(&mut input, 12);
    assert_eq!(strip_terminal_sequences(&line), "> rstuvwxyz ");

    assert!(input.handle_mouse(&mouse_press(4, 1, 12, 1)).is_none());
    assert!(input.handle_mouse(&mouse_press(4, 0, 12, 1)).is_some());
    input.handle_input("_");
    assert_eq!(input.get_value(), "abcdefghijklmnopqrs_tuvwxyz");
}

#[test]
fn bracketed_paste_spans_chunks_and_strips_line_breaks() {
    let (_lock, mut input) = setup();
    input.handle_input("\x1b[200~a\tb\r\n");
    assert_eq!(input.get_value(), "");
    input.handle_input("c\x1b[201~d");
    assert_eq!(input.get_value(), "a    bcd");
}

#[test]
fn kitty_printable_is_inserted_and_control_chars_are_rejected() {
    let (_lock, mut input) = setup();
    input.handle_input("\x1b[97u");
    input.handle_input("\x07");
    input.handle_input("\u{85}");
    assert_eq!(input.get_value(), "a");
}

#[test]
fn escape_calls_on_escape() {
    let (_lock, mut input) = setup();
    let escaped = Rc::new(RefCell::new(0));
    let sink = Rc::clone(&escaped);
    input.set_on_escape(move || *sink.borrow_mut() += 1);
    input.handle_input("\x1b");
    assert_eq!(*escaped.borrow(), 1);
    assert_eq!(input.get_value(), "");
}

#[test]
fn render_emits_cursor_marker_only_when_focused_and_truncates_narrow_prompt() {
    let (_lock, mut input) = setup();
    input.set_value("hi");
    let unfocused = render_one(&mut input, 10);
    assert!(!unfocused.contains(CURSOR_MARKER));
    assert_eq!(visible_width(&unfocused), 10);

    input.set_focused(true);
    assert!(render_one(&mut input, 10).contains(CURSOR_MARKER));
    assert_eq!(strip_terminal_sequences(&render_one(&mut input, 1)), ">");
}

#[test]
fn set_value_clamps_cursor_to_char_boundary() {
    let (_lock, mut input) = setup();
    input.set_value("abcd");
    input.handle_input(CTRL_A);
    input.handle_input(RIGHT);
    // Byte 1 falls inside "é", so the cursor moves back to 0.
    input.set_value("é");
    input.handle_input("x");
    assert_eq!(input.get_value(), "xé");
}
