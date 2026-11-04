//! Caret and harbor prompt boxes: frame, gutter, placeholder, scrolling, popup, and mouse.

use crate::editor_common;

use std::rc::Rc;

use editor_common::{MockProvider, editor_with, flush, plain_theme, suggestions};
use pi_tui::components::editor::{BorderContext, EditorLayout, EditorOptions, PromptBox};
use pi_tui::{
    CURSOR_MARKER, TuiMouseButton, TuiMouseEvent, TuiMouseEventType, render_fake_cursor, strip_terminal_sequences,
    visible_width,
};

const IDLE: &str = "Ask anything \u{b7} / for commands \u{b7} @ for files";
const RUNNING: &str = "Queue a message \u{b7} esc to stop the agent";

fn prompt(placeholder: &str) -> PromptBox {
    PromptBox {
        marker: Rc::new(|text: &str| format!("\x1b[1m{text}\x1b[22m")),
        muted: Rc::new(str::to_string),
        placeholder: placeholder.to_string(),
    }
}

fn boxed(layout: EditorLayout, padding_x: usize) -> editor_common::TestEditor {
    let editor = editor_with(80, plain_theme(), EditorOptions { padding_x, ..EditorOptions::default() });
    editor.set_layout(layout);
    editor
}

fn plain(lines: &[String]) -> Vec<String> {
    lines.iter().map(|line| strip_terminal_sequences(line)).collect()
}

#[test]
fn frame_is_the_rounded_editor() {
    let editor = editor_with(80, plain_theme(), EditorOptions { padding_x: 1, ..EditorOptions::default() });
    editor.set_rounded(true);
    editor.set_text("hi");
    assert_eq!(plain(&editor.render(12)), ["╭──────────╮", "│hi        │", "╰──────────╯"]);
}

#[test]
fn caret_shows_the_placeholder_with_the_cursor_on_its_first_cell() {
    let editor = boxed(EditorLayout::Caret(prompt(IDLE)), 0);
    editor.set_focused(true);
    let lines = editor.render(52);
    assert_eq!(
        plain(&lines),
        [format!("╭{}╮", "─".repeat(50)), format!("│ › {IDLE}    │"), format!("╰{}╯", "─".repeat(50)),]
    );
    assert!(lines[1].starts_with("│ \x1b[1m›\x1b[22m "), "{:?}", lines[1]);
    assert!(lines[1].contains(&format!("{CURSOR_MARKER}{}sk anything", render_fake_cursor("A"))), "{:?}", lines[1]);
}

#[test]
fn caret_cuts_the_running_placeholder_to_the_box() {
    let editor = boxed(EditorLayout::Caret(prompt(RUNNING)), 0);
    assert_eq!(plain(&editor.render(30))[1], "│ › Queue a message \u{b7} esc to │");
}

#[test]
fn caret_wraps_lines_under_the_gutter() {
    let editor = boxed(EditorLayout::Caret(prompt(IDLE)), 0);
    editor.set_text("hello world foo bar\nsecond");
    assert_eq!(
        plain(&editor.render(20)),
        [
            format!("╭{}╮", "─".repeat(18)),
            "│ › hello world    │".to_string(),
            "│   foo bar        │".to_string(),
            "│   second         │".to_string(),
            format!("╰{}╯", "─".repeat(18)),
        ]
    );
}

#[test]
fn caret_keeps_the_padding_of_the_editor() {
    let editor = boxed(EditorLayout::Caret(prompt(IDLE)), 2);
    editor.set_text("hi");
    assert_eq!(plain(&editor.render(14))[1], "│  › hi      │");
}

#[test]
fn caret_keeps_scroll_labels_and_harbor_moves_them_to_the_gutter() {
    let text = (1..=10).map(|n| format!("line {n}")).collect::<Vec<_>>().join("\n");
    let caret = boxed(EditorLayout::Caret(prompt(IDLE)), 0);
    caret.set_text(&text);
    let lines = plain(&caret.render(30));
    assert!(lines[0].starts_with('╭') && lines[0].contains("↑ 3 more"), "{lines:?}");

    let harbor = boxed(EditorLayout::Harbor(prompt(IDLE)), 0);
    harbor.set_top_border_renderer(Some(Box::new(|_: &BorderContext<'_>| "LABEL".to_string())));
    harbor.set_text(&text);
    let lines = plain(&harbor.render(30));
    assert_eq!(lines[0], "");
    assert_eq!(lines[1], format!("╭{}╮", "─".repeat(28)));
    assert_eq!(lines[2], "│ ↑ line 4                   │");
    assert_eq!(lines[3], "│   line 5                   │");
    assert_eq!(lines.last().unwrap(), &format!("╰{}╯", "─".repeat(28)));

    for _ in 0..9 {
        harbor.handle_input("\x1b[A");
    }
    let lines = plain(&harbor.render(30));
    assert_eq!(lines[2], "│ › line 1                   │");
    assert_eq!(lines[8], "│ ↓ line 7                   │");
}

#[test]
fn boxes_fit_every_width() {
    for layout in [EditorLayout::Caret(prompt(IDLE)), EditorLayout::Harbor(prompt(IDLE))] {
        for padding_x in [0, 1, 3] {
            let editor = boxed(layout.clone(), padding_x);
            editor.set_text("a long line that wraps at narrow widths\nwith a second line");
            for width in 2..60 {
                for line in editor.render(width) {
                    assert!(visible_width(&line) <= width, "width {width}: {line:?}");
                }
            }
            editor.set_text("");
            for width in 6..60 {
                for line in editor.render(width).iter().skip(1) {
                    assert_eq!(visible_width(line), width, "width {width}: {line:?}");
                }
            }
        }
    }
}

#[test]
fn cursor_at_the_end_of_a_full_row_stands_in_the_padding() {
    let editor = boxed(EditorLayout::Caret(prompt(IDLE)), 0);
    editor.set_text("abcd");
    let lines = editor.render(10);
    assert_eq!(visible_width(&lines[1]), 10, "{lines:?}");
    assert_eq!(strip_terminal_sequences(&lines[1]), "│ › abcd │");
}

#[test]
fn autocomplete_opens_under_the_caret_box() {
    let editor = boxed(EditorLayout::Caret(prompt(IDLE)), 0);
    editor.set_autocomplete_provider(MockProvider::new(|_, _, _, _| suggestions(&["alpha", "beta"], "")).shared());
    editor.handle_input("\t");
    flush();
    assert!(editor.is_showing_autocomplete());
    let lines = plain(&editor.render(30));
    assert_eq!(lines[2], format!("╰{}╯", "─".repeat(28)));
    let popup = &lines[3..];
    assert!(popup.iter().any(|line| line.starts_with("    → alpha")), "{lines:?}");
    assert!(popup.iter().all(|line| visible_width(line) == 30), "{lines:?}");
}

#[test]
fn clicks_place_the_cursor_after_the_gutter() {
    for (layout, top) in [(EditorLayout::Caret(prompt(IDLE)), 1), (EditorLayout::Harbor(prompt(IDLE)), 2)] {
        let editor = boxed(layout, 0);
        editor.set_text("hello");
        editor.render(30);
        let click = TuiMouseEvent {
            kind: TuiMouseEventType::Click,
            button: TuiMouseButton::Left,
            x: 4 + 2,
            y: top,
            screen_x: 4 + 2,
            screen_y: top,
            width: 30,
            height: 3,
            shift: false,
            alt: false,
            ctrl: false,
            wheel_delta: None,
            click_count: Some(1),
        };
        editor.handle_mouse(&click);
        assert_eq!(editor.get_cursor().col, 2);
    }
}
