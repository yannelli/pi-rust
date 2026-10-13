//! Ports of the Editor cases of upstream `mouse-components.test.ts`, plus popup mouse handling.

mod editor_common;

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use editor_common::{cursor, editor, flush, plain_lines, plain_theme};
use pi_tui::autocomplete::{CombinedAutocompleteProvider, SlashCommand};
use pi_tui::components::editor::{Editor, EditorOptions};
use pi_tui::testing::VirtualTerminal;
use pi_tui::{
    ComponentRef, CopyCompletion, CopyOutcome, CopySelectionFn, Tui, TuiAltScreenOptions, TuiMouseButton,
    TuiMouseEvent, TuiMouseEventType, TuiOptions, TuiStopOptions, component, same_component, ui_loop,
};

fn wait_for_render() {
    ui_loop::run_for(Duration::from_millis(20)).expect("render");
}

fn alt_editor(alt: TuiAltScreenOptions) -> (Rc<RefCell<VirtualTerminal>>, Tui, Editor, ComponentRef) {
    let terminal = VirtualTerminal::shared(20, 6);
    let tui = Tui::new_alt_screen(terminal.clone(), TuiOptions::default(), alt);
    let editor = Editor::new(&tui, plain_theme(), EditorOptions::default());
    let handle: ComponentRef = component(editor.clone());
    tui.add_child(handle.clone());
    (terminal, tui, editor, handle)
}

#[test]
fn positions_and_focuses_the_editor_through_alt_screen_dispatch() {
    let (terminal, tui, editor, handle) = alt_editor(TuiAltScreenOptions::default());
    editor.set_text("hello");
    tui.start();
    wait_for_render();

    terminal.borrow().send_input("\x1b[<0;3;2M");
    terminal.borrow().send_input("\x1b[<0;3;2m");
    terminal.borrow().send_input("X");
    wait_for_render();

    assert_eq!(editor.get_text(), "heXllo");
    let focused = tui.get_focused_component().expect("focused component");
    assert!(same_component(&focused, &handle));
    tui.stop(TuiStopOptions::default());
}

#[test]
fn selects_and_copies_on_drag_instead_of_moving_the_cursor() {
    let copied: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
    let sink = copied.clone();
    let copy: CopySelectionFn = Rc::new(move |text: String, completion: CopyCompletion| {
        sink.borrow_mut().push(text);
        completion.complete(CopyOutcome::Copied);
    });
    let (terminal, tui, editor, _) =
        alt_editor(TuiAltScreenOptions { copy_selection: Some(copy), ..Default::default() });
    editor.set_text("hello world");
    tui.start();
    wait_for_render();
    let before = editor.get_cursor();

    terminal.borrow().send_input("\x1b[<0;1;2M");
    terminal.borrow().send_input("\x1b[<32;5;2M");
    terminal.borrow().send_input("\x1b[<0;5;2m");
    wait_for_render();

    assert_eq!(*copied.borrow(), ["hello"]);
    assert_eq!(editor.get_cursor(), before);
    tui.stop(TuiStopOptions::default());
}

fn mouse(kind: TuiMouseEventType, x: i32, y: i32) -> TuiMouseEvent {
    TuiMouseEvent {
        kind,
        button: if kind == TuiMouseEventType::Wheel { TuiMouseButton::None } else { TuiMouseButton::Left },
        x,
        y,
        screen_x: x,
        screen_y: y,
        width: 40,
        height: 10,
        shift: false,
        alt: false,
        ctrl: false,
        wheel_delta: None,
        click_count: (kind == TuiMouseEventType::Click).then_some(1),
    }
}

#[test]
fn clicks_place_the_cursor_on_wrapped_rows() {
    let editor = editor();
    editor.set_text("aaaa bbbb cccc dddd");
    editor.render(10);
    let result = editor.handle_mouse(&mouse(TuiMouseEventType::Click, 2, 2)).expect("handled");
    assert!(result.handled && result.focus);
    assert_eq!(cursor(&editor), (0, 7));

    // Past the end of a non-last row stays on that row.
    editor.handle_mouse(&mouse(TuiMouseEventType::Click, 9, 1));
    assert_eq!(cursor(&editor), (0, 4));

    // Presses are left to the renderer's selection.
    assert!(editor.handle_mouse(&mouse(TuiMouseEventType::Press, 1, 1)).is_none());
    // Borders take focus without moving the cursor.
    let border = editor.handle_mouse(&mouse(TuiMouseEventType::Click, 1, 0)).expect("handled");
    assert!(border.focus);
    assert_eq!(cursor(&editor), (0, 4));
}

#[test]
fn popup_takes_wheel_and_clicks() {
    let editor = editor();
    let commands = vec![
        SlashCommand::new("help", Some("Show help")).into(),
        SlashCommand::new("model", Some("Switch model")).into(),
        SlashCommand::new("quit", Some("Quit")).into(),
    ];
    editor.set_autocomplete_provider(Rc::new(CombinedAutocompleteProvider::new(commands, ".", None)));
    editor.handle_input("/");
    flush();
    let lines = plain_lines(&editor.render(40));
    assert_eq!(lines.len(), 6);
    assert!(lines[3].starts_with("\u{2192} help"));

    let mut wheel = mouse(TuiMouseEventType::Wheel, 2, 3);
    wheel.wheel_delta = Some(1);
    let result = editor.handle_mouse(&wheel).expect("wheel handled");
    assert!(result.handled && result.focus);
    assert!(plain_lines(&editor.render(40))[4].starts_with("\u{2192} model"));

    editor.handle_mouse(&mouse(TuiMouseEventType::Press, 2, 5));
    editor.handle_mouse(&mouse(TuiMouseEventType::Click, 2, 5));
    assert_eq!(editor.get_text(), "/quit ");
    assert!(!editor.is_showing_autocomplete());
}
