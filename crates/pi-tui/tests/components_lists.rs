//! Ports of upstream `select-list.test.ts`, `settings-list.test.ts`, and the SelectList and
//! SettingsList cases of `mouse-components.test.ts`.

mod alt_view_common;

use std::cell::RefCell;
use std::rc::Rc;

use alt_view_common::Harness;
use pi_tui::components::select_list::SelectListTheme;
use pi_tui::components::settings_list::SubmenuFactory;
use pi_tui::components::{
    SelectItem, SelectList, SelectListLayoutOptions, SettingItem, SettingsList, SettingsListOptions, SettingsListTheme,
};
use pi_tui::utils::visible_width;
use pi_tui::{
    Component, ComponentRef, Container, MouseResponse, TuiMouseButton, TuiMouseEvent, TuiMouseEventType, component,
    same_component,
};

fn identity() -> pi_tui::components::StyleFn {
    Rc::new(|text: &str| text.to_string())
}

fn select_theme() -> SelectListTheme {
    SelectListTheme {
        selected_prefix: identity(),
        selected_text: identity(),
        description: identity(),
        scroll_info: identity(),
        no_match: identity(),
    }
}

fn settings_theme() -> SettingsListTheme {
    SettingsListTheme {
        label: Rc::new(|text: &str, _| text.to_string()),
        value: Rc::new(|text: &str, _| text.to_string()),
        description: identity(),
        cursor: "> ".into(),
        hint: identity(),
    }
}

fn item(value: &str, label: &str, description: Option<&str>) -> SelectItem {
    SelectItem { value: value.into(), label: label.into(), description: description.map(str::to_string) }
}

fn mouse(kind: TuiMouseEventType, x: i32, y: i32, width: i32, height: i32) -> TuiMouseEvent {
    TuiMouseEvent {
        kind,
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
        click_count: (kind == TuiMouseEventType::Click).then_some(1),
    }
}

fn handled(response: Option<MouseResponse>) -> Option<bool> {
    response.map(|response| match response {
        MouseResponse::Handled(result) => result.handled,
        MouseResponse::Forwarded(result) => result.result.handled,
    })
}

/// Upstream `visibleWidth(line.slice(0, line.indexOf(text)))`.
fn visible_index_of(line: &str, text: &str) -> usize {
    let index = line.find(text).unwrap_or_else(|| panic!("{text:?} not in {line:?}"));
    visible_width(&line[..index])
}

/// Upstream `line.indexOf(text)` in UTF-16 units (all characters here are in the BMP).
fn index_of(line: &str, text: &str) -> usize {
    line[..line.find(text).expect("text")].chars().count()
}

type Changes = Rc<RefCell<Vec<(String, String)>>>;

fn settings_list(items: Vec<SettingItem>, max_visible: usize, enable_search: bool) -> (SettingsList, Changes) {
    let changes: Changes = Rc::default();
    let sink = changes.clone();
    let list = SettingsList::new(
        items,
        max_visible,
        settings_theme(),
        move |id, value| sink.borrow_mut().push((id.to_string(), value.to_string())),
        || {},
        SettingsListOptions { enable_search },
    );
    (list, changes)
}

fn cycling(id: &str, label: &str, value: &str, values: &[&str]) -> SettingItem {
    SettingItem {
        id: id.into(),
        label: label.into(),
        current_value: value.into(),
        values: Some(values.iter().map(|v| v.to_string()).collect()),
        ..SettingItem::default()
    }
}

fn change(id: &str, value: &str) -> (String, String) {
    (id.to_string(), value.to_string())
}

#[test]
fn normalizes_multiline_descriptions_to_single_line() {
    let items = vec![item("test", "test", Some("Line one\nLine two\nLine three"))];
    let mut list = SelectList::new(items, 5, select_theme(), SelectListLayoutOptions::default());
    let rendered = list.render(100);
    assert!(!rendered.is_empty());
    assert!(!rendered[0].contains('\n'));
    assert!(rendered[0].contains("Line one Line two Line three"));
}

#[test]
fn keeps_descriptions_aligned_when_the_primary_text_is_truncated() {
    let long = "very-long-command-name-that-needs-truncation";
    let items = vec![item("short", "short", Some("short description")), item(long, long, Some("long description"))];
    let mut list = SelectList::new(items, 5, select_theme(), SelectListLayoutOptions::default());
    let rendered = list.render(80);
    assert_eq!(visible_index_of(&rendered[0], "short description"), visible_index_of(&rendered[1], "long description"));
}

#[test]
fn uses_the_configured_minimum_primary_column_width() {
    let items = vec![item("a", "a", Some("first")), item("bb", "bb", Some("second"))];
    let layout = SelectListLayoutOptions {
        min_primary_column_width: Some(12),
        max_primary_column_width: Some(20),
        ..SelectListLayoutOptions::default()
    };
    let mut list = SelectList::new(items, 5, select_theme(), layout);
    let rendered = list.render(80);
    assert_eq!(index_of(&rendered[0], "first"), 14);
    assert_eq!(index_of(&rendered[1], "second"), 14);
}

#[test]
fn uses_the_configured_maximum_primary_column_width() {
    let long = "very-long-command-name-that-needs-truncation";
    let items = vec![item(long, long, Some("first")), item("short", "short", Some("second"))];
    let layout = SelectListLayoutOptions {
        min_primary_column_width: Some(12),
        max_primary_column_width: Some(20),
        ..SelectListLayoutOptions::default()
    };
    let mut list = SelectList::new(items, 5, select_theme(), layout);
    let rendered = list.render(80);
    assert_eq!(visible_index_of(&rendered[0], "first"), 22);
    assert_eq!(visible_index_of(&rendered[1], "second"), 22);
}

#[test]
fn allows_overriding_primary_truncation_while_preserving_description_alignment() {
    let long = "very-long-command-name-that-needs-truncation";
    let items = vec![item(long, long, Some("first")), item("short", "short", Some("second"))];
    let layout = SelectListLayoutOptions {
        min_primary_column_width: Some(12),
        max_primary_column_width: Some(12),
        truncate_primary: Some(Rc::new(|context| {
            if context.text.chars().count() <= context.max_width {
                return context.text.to_string();
            }
            let kept: String = context.text.chars().take(context.max_width.saturating_sub(1)).collect();
            format!("{kept}…")
        })),
    };
    let mut list = SelectList::new(items, 5, select_theme(), layout);
    let rendered = list.render(80);
    assert!(rendered[0].contains('…'));
    assert_eq!(visible_index_of(&rendered[0], "first"), visible_index_of(&rendered[1], "second"));
}

#[test]
fn includes_spaces_in_an_active_search_instead_of_changing_the_selected_setting() {
    let (mut list, changes) =
        settings_list(vec![cycling("tui-mode", "TUI mode", "regular", &["regular", "fullscreen"])], 10, true);
    for character in "TUI mode".chars() {
        list.handle_input(&character.to_string());
    }
    assert!(changes.borrow().is_empty());
    assert!(list.render(80)[0].contains("TUI mode"));

    list.handle_input("\r");
    assert_eq!(*changes.borrow(), [change("tui-mode", "fullscreen")]);
}

#[test]
fn keeps_space_as_a_change_shortcut_before_a_search_query_is_entered() {
    let (mut list, changes) =
        settings_list(vec![cycling("tui-mode", "TUI mode", "regular", &["regular", "fullscreen"])], 10, true);
    list.handle_input(" ");
    assert_eq!(*changes.borrow(), [change("tui-mode", "fullscreen")]);
}

#[test]
fn selects_and_activates_list_rows() {
    let items = ["a", "b", "c", "d", "e"].iter().map(|v| item(v, &v.to_uppercase(), None)).collect();
    let mut list = SelectList::new(items, 3, select_theme(), SelectListLayoutOptions::default());
    let selected: Rc<RefCell<Option<String>>> = Rc::default();
    let sink = selected.clone();
    list.on_select = Some(Box::new(move |item| *sink.borrow_mut() = Some(item.value.clone())));

    assert_eq!(handled(list.handle_mouse(&mouse(TuiMouseEventType::Press, 1, 2, 40, 3))), Some(true));
    assert_eq!(list.get_selected_item().map(|item| item.value.as_str()), Some("c"));
    assert_eq!(handled(list.handle_mouse(&mouse(TuiMouseEventType::Click, 1, 2, 40, 3))), Some(true));
    assert_eq!(selected.borrow().as_deref(), Some("c"));
}

#[test]
fn activates_settings_rows() {
    let items = vec![
        cycling("mode", "Mode", "one", &["one", "two"]),
        cycling("other", "Other", "off", &["off", "on"]),
        cycling("third", "Third", "low", &["low", "high"]),
        cycling("fourth", "Fourth", "x", &["x", "y"]),
    ];
    let (mut list, changes) = settings_list(items, 3, false);
    list.handle_mouse(&mouse(TuiMouseEventType::Press, 1, 2, 40, 5));
    list.handle_mouse(&mouse(TuiMouseEventType::Click, 1, 2, 40, 5));
    assert_eq!(*changes.borrow(), [change("third", "high")]);
}

fn hover(y: i32) -> TuiMouseEvent {
    TuiMouseEvent { button: TuiMouseButton::None, ..mouse(TuiMouseEventType::Move, 1, y, 80, 10) }
}

fn wheel(y: i32) -> TuiMouseEvent {
    TuiMouseEvent { wheel_delta: Some(1), ..mouse(TuiMouseEventType::Wheel, 1, y, 80, 10) }
}

#[test]
fn ignores_hover_and_clicks_visible_select_list_rows_after_scrolling() {
    for row in [0, 4] {
        let items = (0..12).map(|i| item(&format!("item-{i}"), &format!("Item {i}"), None)).collect();
        let mut list = SelectList::new(items, 5, select_theme(), SelectListLayoutOptions::default());
        let changes: Rc<RefCell<Vec<String>>> = Rc::default();
        let selected: Rc<RefCell<Option<String>>> = Rc::default();
        let (change_sink, select_sink) = (changes.clone(), selected.clone());
        list.on_selection_change = Some(Box::new(move |item| change_sink.borrow_mut().push(item.value.clone())));
        list.on_select = Some(Box::new(move |item| *select_sink.borrow_mut() = Some(item.value.clone())));
        list.set_selected_index(5);
        list.handle_mouse(&wheel(row));
        assert_eq!(list.get_selected_item().map(|item| item.value.as_str()), Some("item-6"));
        assert_eq!(*changes.borrow(), ["item-6"]);
        let before = list.render(80);
        assert!(before[row as usize].ends_with(&format!("Item {}", 4 + row)), "{before:?}");

        for y in [0, 1, 2, 3, 4, row] {
            assert!(list.handle_mouse(&hover(y)).is_none());
            assert_eq!(list.render(80), before);
        }
        assert_eq!(list.get_selected_item().map(|item| item.value.as_str()), Some("item-6"));
        assert_eq!(*changes.borrow(), ["item-6"]);
        assert!(selected.borrow().is_none());

        list.handle_mouse(&mouse(TuiMouseEventType::Press, 1, row, 80, 10));
        list.render(80);
        list.handle_mouse(&mouse(TuiMouseEventType::Click, 1, row, 80, 10));
        assert_eq!(selected.borrow().as_deref(), Some(format!("item-{}", 4 + row).as_str()));
        assert_eq!(*changes.borrow(), ["item-6".to_string(), format!("item-{}", 4 + row)]);
    }
}

#[test]
fn ignores_hover_and_clicks_visible_settings_rows_after_scrolling() {
    for row in [0, 4] {
        let items = (0..12)
            .map(|i| SettingItem {
                description: Some(format!("Description {i}")),
                ..cycling(&format!("item-{i}"), &format!("Item {i}"), "off", &["off", "on"])
            })
            .collect();
        let (mut list, changes) = settings_list(items, 5, true);
        list.select_item("item-5");
        list.handle_mouse(&wheel(row + 2));
        let before = list.render(80);
        assert!(before[4].starts_with("> Item 6"), "{before:?}");
        assert!(before[row as usize + 2].contains(&format!("Item {} ", 4 + row)), "{before:?}");

        for y in [0, 1, 2, 3, 4, row] {
            assert!(list.handle_mouse(&hover(y + 2)).is_none());
            assert_eq!(list.render(80), before);
        }
        assert!(changes.borrow().is_empty());

        list.handle_mouse(&mouse(TuiMouseEventType::Press, 1, row + 2, 80, 10));
        list.render(80);
        list.handle_mouse(&mouse(TuiMouseEventType::Click, 1, row + 2, 80, 10));
        assert_eq!(*changes.borrow(), [change(&format!("item-{}", 4 + row), "on")]);
    }
}

/// A settings submenu that routes keys to its nested list, like the coding agent's theme submenu.
struct SubmenuHost {
    list: Rc<RefCell<SelectList>>,
    container: Container,
}

impl Component for SubmenuHost {
    fn render(&mut self, width: usize) -> Vec<String> {
        self.container.render_children(width)
    }

    fn handle_input(&mut self, data: &str) {
        self.list.borrow_mut().handle_input(data);
    }

    fn handles_input(&self) -> bool {
        true
    }

    fn handle_mouse(&mut self, event: &TuiMouseEvent) -> Option<MouseResponse> {
        self.container.handle_children_mouse(event)
    }

    fn container_children(&self) -> Option<Vec<ComponentRef>> {
        Some(self.container.children.clone())
    }
}

#[test]
fn keeps_a_settings_list_focused_when_a_click_in_its_submenu_closes_the_submenu() {
    let h = Harness::new(30, 6);
    let submenu: SubmenuFactory = Rc::new(|_value, done| {
        let items = vec![item("first", "First", None), item("second", "Second", None)];
        let list = component(SelectList::new(items, 5, select_theme(), SelectListLayoutOptions::default()));
        let (on_select, on_cancel) = (done.clone(), done);
        list.borrow_mut().on_select = Some(Box::new(move |item| on_select.done(Some(&item.value))));
        list.borrow_mut().on_cancel = Some(Box::new(move || on_cancel.done(None)));
        let mut container = Container::new();
        container.add_child(list.clone());
        component(SubmenuHost { list, container })
    });
    let items = vec![
        SettingItem {
            id: "theme".into(),
            label: "Theme".into(),
            current_value: "first".into(),
            submenu: Some(submenu),
            ..SettingItem::default()
        },
        cycling("other", "Other", "off", &["off", "on"]),
    ];
    let (list, changes) = settings_list(items, 5, false);
    let list: ComponentRef = component(list);
    h.tui.add_child(list.clone());
    h.tui.set_focus(Some(list.clone()));
    h.tui.start();
    h.wait();

    h.send("\r");
    h.wait();
    // Press and release on the submenu's second row selects it and closes the submenu.
    h.send("\x1b[<0;3;2M");
    h.send("\x1b[<0;3;2m");
    h.wait();
    assert_eq!(*changes.borrow(), [change("theme", "second")]);
    assert!(h.tui.get_focused_component().is_some_and(|focused| same_component(&focused, &list)));

    // Keys reach the visible list again instead of the closed submenu.
    h.send("\x1b[B");
    h.send("\r");
    h.wait();
    assert_eq!(*changes.borrow(), [change("theme", "second"), change("other", "on")]);
    h.stop();
}
