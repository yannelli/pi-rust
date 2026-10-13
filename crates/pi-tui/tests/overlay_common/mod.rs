//! Shared helpers for the overlay test ports (upstream `overlay-non-capturing.test.ts` and
//! `overlay-options.test.ts`): test components and a terminal plus TUI pair.
#![allow(dead_code)]

use std::cell::RefCell;
use std::rc::Rc;

use pi_tui::env::MapEnv;
use pi_tui::testing::VirtualTerminal;
use pi_tui::{Component, ComponentRef, OverlayHandle, OverlayOptions, SizeValue, Tui, TuiOptions, ui_loop};

pub type InputHook = Box<dyn FnMut(&str)>;

/// Upstream `FocusableOverlay`: records focus and input. The hook runs after the input is recorded,
/// like the upstream tests that replace `handleInput`.
pub struct FocusableOverlay {
    pub focused: bool,
    pub inputs: Vec<String>,
    lines: Vec<String>,
    on_input: Option<InputHook>,
}

impl FocusableOverlay {
    pub fn set_on_input(&mut self, hook: impl FnMut(&str) + 'static) {
        self.on_input = Some(Box::new(hook));
    }
}

impl Component for FocusableOverlay {
    fn render(&mut self, _width: usize) -> Vec<String> {
        self.lines.clone()
    }

    fn handle_input(&mut self, data: &str) {
        self.inputs.push(data.to_string());
        if let Some(hook) = self.on_input.as_mut() {
            hook(data);
        }
    }

    fn handles_input(&self) -> bool {
        true
    }

    fn is_focusable(&self) -> bool {
        true
    }

    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
    }
}

pub fn focusable(text: &str) -> Rc<RefCell<FocusableOverlay>> {
    pi_tui::component(FocusableOverlay {
        focused: false,
        inputs: Vec::new(),
        lines: vec![text.to_string()],
        on_input: None,
    })
}

/// Upstream `StaticOverlay`: fixed lines; remembers the width it was asked to render at.
pub struct StaticOverlay {
    lines: Vec<String>,
    pub requested_width: Option<usize>,
}

impl Component for StaticOverlay {
    fn render(&mut self, width: usize) -> Vec<String> {
        self.requested_width = Some(width);
        self.lines.clone()
    }
}

pub fn static_overlay(lines: &[&str]) -> Rc<RefCell<StaticOverlay>> {
    pi_tui::component(StaticOverlay { lines: lines.iter().map(|l| l.to_string()).collect(), requested_width: None })
}

/// Upstream `EmptyContent`: renders nothing.
pub struct EmptyContent;

impl Component for EmptyContent {
    fn render(&mut self, _width: usize) -> Vec<String> {
        Vec::new()
    }
}

/// Converts a typed component handle into a [`ComponentRef`].
pub fn cref<T: Component + 'static>(component: &Rc<RefCell<T>>) -> ComponentRef {
    component.clone()
}

/// Options for a non-capturing overlay.
pub fn non_capturing() -> OverlayOptions {
    OverlayOptions { non_capturing: true, ..Default::default() }
}

/// Upstream `{ row: 0, col: 0, width: 1, nonCapturing }` used by the rendering order cases.
pub fn corner(non_capturing: bool) -> OverlayOptions {
    OverlayOptions {
        row: Some(SizeValue::Absolute(0)),
        col: Some(SizeValue::Absolute(0)),
        width: Some(SizeValue::Absolute(1)),
        non_capturing,
        ..Default::default()
    }
}

/// A virtual terminal and a main screen TUI with an empty environment.
pub struct Harness {
    pub terminal: Rc<RefCell<VirtualTerminal>>,
    pub tui: Tui,
}

impl Harness {
    pub fn new(columns: usize, rows: usize) -> Self {
        let terminal = VirtualTerminal::shared(columns, rows);
        let env = Rc::new(MapEnv::new([("TERM", "xterm-256color")]));
        let tui = Tui::new_main_screen(terminal.clone(), TuiOptions { env: Some(env), ..Default::default() });
        Self { terminal, tui }
    }

    /// Upstream setup shared by most focus cases: empty content, then the editor gets focus.
    pub fn with_editor(columns: usize, rows: usize, editor: Option<ComponentRef>) -> Self {
        let harness = Self::new(columns, rows);
        harness.tui.add_child(pi_tui::component(EmptyContent));
        if editor.is_some() {
            harness.tui.set_focus(editor);
        }
        harness
    }

    pub fn start(&self) {
        self.tui.start();
    }

    pub fn stop(&self) {
        self.tui.stop(Default::default());
    }

    pub fn show(&self, component: ComponentRef, options: OverlayOptions) -> OverlayHandle {
        self.tui.show_overlay(component, options)
    }

    pub fn focus(&self, component: Option<ComponentRef>) {
        self.tui.set_focus(component);
    }

    /// Upstream `terminal.sendInput`: the input reaches the TUI on the next loop pass.
    pub fn send(&self, data: &str) {
        self.terminal.borrow().send_input(data);
    }

    /// Upstream `renderAndFlush`: forced render, then the loop delivers input and renders.
    pub fn flush(&self) {
        self.tui.request_render(true);
        ui_loop::run_pending().expect("render");
    }

    pub fn viewport(&self) -> Vec<String> {
        self.terminal.borrow().viewport()
    }

    /// First character of the top viewport row.
    pub fn top_left(&self) -> Option<char> {
        self.viewport().first().and_then(|row| row.chars().next())
    }

    pub fn is_focused(&self, component: &ComponentRef) -> bool {
        self.tui.get_focused_component().is_some_and(|f| pi_tui::same_component(&f, component))
    }
}

/// Input hook that calls `tui.setFocus(target)` when the input equals `key`.
pub fn focus_on(tui: &Tui, key: &'static str, target: Option<ComponentRef>) -> impl FnMut(&str) + 'static {
    let tui = tui.downgrade();
    move |data| {
        if data == key
            && let Some(tui) = tui.upgrade()
        {
            tui.set_focus(target.clone());
        }
    }
}

/// Inputs recorded by a focusable test component.
pub fn inputs(component: &Rc<RefCell<FocusableOverlay>>) -> Vec<String> {
    component.borrow().inputs.clone()
}

pub fn focused(component: &Rc<RefCell<FocusableOverlay>>) -> bool {
    component.borrow().focused
}
