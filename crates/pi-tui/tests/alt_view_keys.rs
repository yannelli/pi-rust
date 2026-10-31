//! Port of upstream `tui-alt-screen.test.ts`: keyboard viewport navigation, custom scroll
//! bindings, OSC 133 prompt jumps, and iTerm2 output.

mod alt_view_common;
mod editor_common;
mod layout_common;
mod support;

use std::cell::RefCell;
use std::rc::Rc;

use alt_view_common::{Harness, numbered_lines, recording_editor, scroll_view};
use editor_common::{MockProvider, default_theme};
use layout_common::{entry, fn_component, text};
use pi_tui::autocomplete::{AutocompleteItem, AutocompleteSuggestions};
use pi_tui::components::select_list::SelectListTheme;
use pi_tui::components::{ScrollFollow, SelectItem, SelectList, SelectListLayoutOptions, StyleFn};
use pi_tui::keybindings::{KeybindingsConfig, tui_keybindings};
use pi_tui::terminal_image::{ImageDimensions, ImageProtocol, TerminalCapabilities, reset_capabilities_cache};
use pi_tui::{
    ComponentRef, Editor, EditorOptions, KeybindingsManager, ScrollViewOptions, StackOptions, VStack, component,
    get_keybindings, set_keybindings,
};
use support::{Image, ImageOptions, ImageTheme};

const OSC133_ZONE_START: &str = "\x1b]133;A\x07";

fn lines(range: std::ops::RangeInclusive<usize>) -> Vec<String> {
    range.map(|index| format!("line {index}")).collect()
}

#[test]
fn supports_configurable_keyboard_viewport_navigation_with_four_rows_of_page_overlap() {
    let h = Harness::new(20, 8);
    h.tui.add_child(text(&numbered_lines(12)));
    h.tui.start();
    h.wait();

    h.send("\x1b[57421u");
    h.send("\x1b[57421;1:3u");
    h.wait();
    assert_eq!(h.viewport(), lines(1..=8));

    h.send("\x1b[57422u");
    h.send("\x1b[57422;1:3u");
    h.wait();
    assert_eq!(h.viewport(), lines(5..=12));

    h.send("\x1b[7^");
    h.wait();
    assert_eq!(h.viewport(), lines(1..=8));

    h.send("\x1b[8^");
    h.wait();
    assert_eq!(h.viewport(), lines(5..=12));

    h.stop();
}

/// Installs custom bindings over the TUI defaults and puts the original bindings back on drop.
struct CustomBindings(KeybindingsManager);

impl CustomBindings {
    fn install(bindings: &[(&str, &str)]) -> Self {
        let original = (*get_keybindings()).clone();
        let mut config = KeybindingsConfig::new();
        for (id, key) in bindings {
            config.insert(*id, *key);
        }
        set_keybindings(KeybindingsManager::new(tui_keybindings(), config));
        Self(original)
    }
}

impl Drop for CustomBindings {
    fn drop(&mut self) {
        set_keybindings(self.0.clone());
    }
}

#[test]
fn scrolls_the_transcript_by_half_a_page_with_custom_bindings() {
    let h = Harness::new(20, 10);
    let _bindings =
        CustomBindings::install(&[("tui.altScreen.halfPageUp", "ctrl+u"), ("tui.altScreen.halfPageDown", "ctrl+d")]);
    h.tui.add_child(text(&numbered_lines(30)));
    h.tui.start();
    h.wait();
    assert_eq!(h.tui.viewport_top(), 20);

    h.send("\x15");
    h.wait();
    assert_eq!(h.tui.viewport_top(), 15);

    h.send("\x04");
    h.wait();
    assert_eq!(h.tui.viewport_top(), 20);
    h.stop();
}

#[test]
fn scrolls_the_transcript_by_one_line_with_custom_bindings() {
    let h = Harness::new(20, 10);
    let _bindings =
        CustomBindings::install(&[("tui.altScreen.lineUp", "ctrl+y"), ("tui.altScreen.lineDown", "ctrl+e")]);
    h.tui.add_child(text(&numbered_lines(30)));
    h.tui.start();
    h.wait();
    assert_eq!(h.tui.viewport_top(), 20);

    h.send("\x19");
    h.wait();
    assert_eq!(h.tui.viewport_top(), 19);

    h.send("\x05");
    h.wait();
    assert_eq!(h.tui.viewport_top(), 20);
    h.stop();
}

#[test]
fn routes_home_and_end_to_the_focused_component_and_ctrl_home_end_to_the_transcript() {
    let h = Harness::new(20, 6);
    let options = ScrollViewOptions { follow: ScrollFollow::End, primary: true, ..ScrollViewOptions::default() };
    let (transcript, state) = scroll_view(text(&numbered_lines(12)), options);
    let (editor, editor_inputs) = recording_editor();
    h.tui.set_layout_root(Some(component(VStack::new(
        vec![entry(transcript).basis(0).grow(1).min(1).into(), entry(editor.clone()).basis(1).shrink(0).into()],
        StackOptions::default(),
    ))));
    h.tui.set_focus(Some(editor));
    h.tui.start();
    h.wait();

    let bottom = state.scroll_top();
    assert!(bottom > 0);

    // #10314: unmodified Home/End belong to the editor in every UI mode.
    let editor_keys = ["\x1bOH", "\x1b[F", "\x1b[57423u", "\x1b[5;5~", "\x1b[6;5~"];
    for input in editor_keys {
        h.send(input);
    }
    h.wait();
    assert_eq!(state.scroll_top(), bottom);
    assert_eq!(*editor_inputs.borrow(), editor_keys);

    h.send("\x1b[1;5H");
    h.wait();
    assert_eq!(state.scroll_top(), 0);

    h.send("\x1b[1;5F");
    h.wait();
    assert_eq!(state.scroll_top(), bottom);
    assert!(state.is_following_end());

    h.send("\x1b[57423;5u");
    h.send("\x1b[57423;5:3u");
    h.wait();
    assert_eq!(state.scroll_top(), 0);

    h.send("\x1b[6~");
    h.wait();
    assert_eq!(state.scroll_top(), 1);
    assert_eq!(*editor_inputs.borrow(), editor_keys);

    h.stop();
}

#[test]
fn defers_viewport_keys_to_a_focused_list_instead_of_scrolling_the_transcript() {
    let h = Harness::new(20, 6);
    let plain: StyleFn = Rc::new(|text: &str| text.to_string());
    let theme = SelectListTheme {
        selected_prefix: plain.clone(),
        selected_text: plain.clone(),
        description: plain.clone(),
        scroll_info: plain.clone(),
        no_match: plain,
    };
    let items = (0..12).map(|i| SelectItem::new(&format!("item-{i}"), &format!("item-{i}"))).collect();
    let list = component(SelectList::new(items, 3, theme, SelectListLayoutOptions::default()));
    h.tui.add_child(text(&numbered_lines(12)));
    let focused: ComponentRef = list.clone();
    h.tui.add_child(focused.clone());
    h.tui.set_focus(Some(focused));
    h.tui.start();
    h.wait();
    let top_before = h.tui.viewport_top();

    h.send("\x1b[6~");
    h.send("\x1b[1;5H");
    h.wait();
    assert_eq!(list.borrow().get_selected_item().map(|item| item.value.clone()).as_deref(), Some("item-3"));
    assert_eq!(h.tui.viewport_top(), top_before);
    h.stop();
}

#[test]
fn gives_viewport_keys_back_to_the_transcript_when_editor_autocomplete_closes() {
    let h = Harness::new(40, 8);
    let editor = Editor::new(&h.tui, default_theme(), EditorOptions::default());
    let submitted: Rc<RefCell<String>> = Rc::default();
    let sink = submitted.clone();
    editor.on_submit(move |text| *sink.borrow_mut() = text.to_string());
    let provider = MockProvider::new(|_, _, _, _| {
        let items = (0..12).map(|i| AutocompleteItem::new(format!("/command-{i}"), format!("command-{i}"))).collect();
        Some(AutocompleteSuggestions { items, prefix: "/".into() })
    });
    editor.set_autocomplete_provider(provider.shared());
    h.tui.add_child(text(&numbered_lines(30)));
    let focused: ComponentRef = component(editor.clone());
    h.tui.add_child(focused.clone());
    h.tui.set_focus(Some(focused));
    h.tui.start();
    h.wait();

    h.send("/");
    h.wait();
    assert!(editor.is_showing_autocomplete());
    let top_with_menu = h.tui.viewport_top();
    // PageDown pages the menu.
    h.send("\x1b[6~");
    h.wait();
    assert_eq!(h.tui.viewport_top(), top_with_menu);

    h.send("\r");
    h.wait();
    assert_eq!(*submitted.borrow(), "/command-5");
    assert!(!editor.is_showing_autocomplete());
    let top_without_menu = h.tui.viewport_top();
    // PageUp scrolls the transcript again.
    h.send("\x1b[5~");
    h.wait();
    assert!(h.tui.viewport_top() < top_without_menu);
    h.stop();
}

#[test]
fn jumps_between_osc_133_semantic_prompt_markers() {
    let h = Harness::new(20, 3);
    let transcript: Vec<String> =
        (1..=4).flat_map(|message| [format!("{OSC133_ZONE_START}message {message}"), "detail".to_string()]).collect();
    h.tui.add_child(text(&transcript.join("\n")));
    h.tui.start();
    h.wait();
    assert_eq!(h.tui.viewport_top(), 5);

    h.send("\x1b[57419;6u");
    h.send("\x1b[57419;6:3u");
    h.wait();
    assert_eq!(h.tui.viewport_top(), 4);
    assert_eq!(h.viewport()[0], "message 3");

    h.send("\x1b[1;6A");
    h.wait();
    assert_eq!(h.tui.viewport_top(), 2);
    assert_eq!(h.viewport()[0], "message 2");

    h.send("\x1b[57420;6u");
    h.send("\x1b[57420;6:3u");
    h.wait();
    assert_eq!(h.tui.viewport_top(), 4);
    assert_eq!(h.viewport()[0], "message 3");

    h.send("\x1b[1;6B");
    h.wait();
    assert_eq!(h.tui.viewport_top(), 5);
    assert_eq!(h.viewport()[1], "message 4");
    assert!(h.tui.is_following_output());

    h.stop();
}

/// Resets the capability cache when dropped (upstream `finally { resetCapabilitiesCache() }`).
struct CapabilitiesReset;

impl Drop for CapabilitiesReset {
    fn drop(&mut self) {
        reset_capabilities_cache();
    }
}

#[test]
fn does_not_emit_kitty_graphics_commands_or_osc_133_zones_in_iterm2() {
    let h = Harness::new(20, 3);
    let _reset = CapabilitiesReset;
    pi_tui::terminal_image::set_capabilities(TerminalCapabilities {
        images: Some(ImageProtocol::Iterm2),
        true_color: true,
        hyperlinks: true,
    });
    h.tui.add_child(fn_component(|_| vec!["\x1b]133;B\x07\x1b]133;C\x07\x1b]133;A\x07content".to_string()]));
    let image_options = ImageOptions { filename: Some("example.png".into()), ..ImageOptions::default() };
    let dimensions = ImageDimensions { width_px: 10, height_px: 10 };
    h.tui.add_child(component(Image::new("AAAA", "image/png", ImageTheme::default(), image_options, Some(dimensions))));
    h.tui.start();
    h.wait();
    h.stop();
    assert!(!h.wrote("\x1b_G"));
    assert!(!h.wrote("\x1b]133;"));
    assert!(!h.wrote("\x1b]1337;File="));
    assert!(h.wrote("[Image:"));
}
