//! Port of upstream `tui-alt-screen.test.ts`: transcript search through the fullscreen TUI.

mod alt_view_common;
mod layout_common;
mod support;

use std::rc::Rc;

use alt_view_common::{Harness, column_of, last_column_of, recording_editor, scroll_view};
use layout_common::{entry, text};
use pi_tui::components::ScrollFollow;
use pi_tui::{ScrollViewOptions, StackOptions, TuiAltScreenOptions, VStack, component};

const OPEN_SEARCH: &str = "\x1b[102;6u";
const CONTROLS: &str = "↑ Shift+Enter · ↓ Enter";

/// Row and line of the search box controls.
fn controls_row(h: &Harness) -> Option<(usize, String)> {
    h.viewport().into_iter().enumerate().find(|(_, line)| line.contains('↑') && line.contains('↓'))
}

#[test]
fn navigates_transcript_search_with_hoverable_arrow_buttons_and_toggles_it_with_its_shortcut() {
    let options = TuiAltScreenOptions {
        search_navigation_button_style: Some(Rc::new(|text, hovered| {
            format!("{}{text}\x1b[49m", if hovered { "\x1b[45m" } else { "\x1b[44m" })
        })),
        ..TuiAltScreenOptions::default()
    };
    let h = Harness::with_options(120, 6, options);
    h.tui.add_child(text("needle one\nmiddle\nneedle two\nend"));
    h.tui.start();
    h.wait();

    h.send(OPEN_SEARCH);
    h.send("needle");
    h.wait();
    assert!(h.viewport_has("1/2"));
    assert!(h.viewport_has(CONTROLS));

    let (arrow_row, line) = controls_row(&h).expect("controls row");
    let arrow_column = last_column_of(&line, "Enter").expect("next button");
    let hover_event_count = h.write_count();
    h.send(&format!("\x1b[<35;{};{}M", arrow_column + 1, arrow_row + 1));
    h.wait();
    assert!(h.wrote_since(hover_event_count, "\x1b[45m↓ Enter\x1b[49m"));
    h.send(&format!("\x1b[<0;{};{}M", arrow_column + 1, arrow_row + 1));
    h.wait();
    assert!(h.viewport_has("2/2"));
    assert!(h.viewport_has(CONTROLS));

    let (arrow_row, line) = controls_row(&h).expect("controls row");
    let arrow_column = column_of(&line, "Shift+Enter").expect("previous button") + 3;
    h.send(&format!("\x1b[<0;{};{}M", arrow_column + 1, arrow_row + 1));
    h.wait();
    assert!(h.viewport_has("1/2"));
    assert!(h.viewport_has(CONTROLS));

    h.send(OPEN_SEARCH);
    h.wait();
    assert!(!h.viewport_has(CONTROLS));
    h.stop();
}

#[test]
fn does_not_treat_transcript_box_drawing_as_search_navigation_buttons() {
    let h = Harness::new(80, 10);
    let transcript = [
        "needle one",
        "middle",
        "needle two",
        "filler",
        "┌────────────────────────────────────────┐",
        "│ box                                    │",
        "└────────────────────────────────────────┘",
        "end",
    ];
    h.tui.add_child(text(&transcript.join("\n")));
    h.tui.start();
    h.wait();

    h.send(OPEN_SEARCH);
    h.send("needle");
    h.wait();
    assert!(h.viewport_has("1/2"));
    assert!(!h.viewport_has("2/2"));

    let box_bottom_row = h.viewport().iter().position(|line| line.starts_with('└')).expect("box bottom row");
    h.send(&format!("\x1b[<0;24;{}M", box_bottom_row + 1));
    h.wait();

    assert!(h.viewport_has("1/2"));
    assert!(!h.viewport_has("2/2"));
    h.stop();
}

#[test]
fn uses_configured_styles_for_current_and_non_current_search_matches() {
    let options = TuiAltScreenOptions {
        search_match_style: Some(Rc::new(|text| format!("\x1b[41m{text}\x1b[49m"))),
        search_current_match_style: Some(Rc::new(|text| format!("\x1b[42m{text}\x1b[49m"))),
        ..TuiAltScreenOptions::default()
    };
    let h = Harness::with_options(60, 4, options);
    h.tui.add_child(text("needle first\nmiddle\nneedle second\nend"));
    h.tui.start();
    h.wait();

    h.send(OPEN_SEARCH);
    h.send("needle");
    h.wait();

    assert!(h.wrote("\x1b[42mneedle\x1b[49m"));
    assert!(h.wrote("\x1b[41mneedle\x1b[49m"));
    h.stop();
}

#[test]
fn searches_the_transcript_with_ctrl_shift_f_and_restores_editor_focus_on_close() {
    let h = Harness::new(60, 8);
    let transcript_lines: Vec<String> = (0..12)
        .map(|index| match index {
            4 => "line 5 needle one".to_string(),
            9 => "line 10 needle two".to_string(),
            _ => format!("line {}", index + 1),
        })
        .collect();
    let options = ScrollViewOptions { follow: ScrollFollow::End, primary: true, ..ScrollViewOptions::default() };
    let (transcript, state) = scroll_view(text(&transcript_lines.join("\n")), options);
    let (editor, editor_inputs) = recording_editor();
    h.tui.set_layout_root(Some(component(VStack::new(
        vec![entry(transcript).basis(0).grow(1).min(1).into(), entry(editor.clone()).basis(1).shrink(0).into()],
        StackOptions::default(),
    ))));
    h.tui.set_focus(Some(editor));
    h.tui.start();
    h.wait();

    h.send(OPEN_SEARCH);
    h.send("needle");
    h.wait();
    assert!(!state.is_following_end());
    assert!(h.viewport_has("2/2"));
    assert!(h.viewport_has(CONTROLS));
    assert!(h.viewport_has("line 10 needle two"));
    assert!(editor_inputs.borrow().is_empty());
    assert!(h.wrote("\x1b[1;7mneedle\x1b[22;27m"));

    for _ in 0..6 {
        h.send("\x1b[<64;1;4M");
    }
    h.wait();
    assert_eq!(state.scroll_top(), 0);
    assert!(h.viewport().iter().any(|line| line.contains("needle") && line.contains("2/2")));

    h.send("\x07");
    h.wait();
    assert!(h.viewport_has("1/2"));
    assert!(h.viewport_has("line 5 needle one"));

    h.send("\x1b[103;6u");
    h.wait();
    assert!(h.viewport_has("2/2"));
    assert!(h.viewport_has("line 10 needle two"));

    h.send("\x1b");
    h.send("x");
    h.wait();
    assert!(!h.viewport_has(CONTROLS));
    assert_eq!(*editor_inputs.borrow(), ["x"]);

    h.stop();
}
